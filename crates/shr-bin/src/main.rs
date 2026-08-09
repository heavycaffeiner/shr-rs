//! `shr-rs`: the actual installed entry point. No
//! arguments in an interactive terminal enters the TUI; a subcommand (or a
//! non-interactive invocation) runs the CLI. All business logic lives in
//! `shr-cli`/`shr-tui`/`shr-orchestrate` -- this crate only decides which
//! frontend to hand control to.

use shr_command::{detect_ui_mode, UiMode};
use std::process::ExitCode;

fn main() -> ExitCode {
    reset_sigpipe();

    let argv: Vec<String> = std::env::args().collect();
    let program = argv.first().cloned().unwrap_or_default();
    let rest = argv.get(1..).unwrap_or_default();
    let mode = detect_ui_mode(rest);

    // The fallback filter depends on `mode` -- see `init_tracing`'s
    // doc comment -- so `detect_ui_mode` must run before this.
    init_tracing(mode);

    match mode {
        UiMode::Tui => match shr_tui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        UiMode::Cli => shr_cli::run(cli_args(program, rest)),
    }
}

/// `sudo shr-rs --json disk smart | head -20` exits 101
/// (a Rust panic) even though the same command with no pipe exits 0. The
/// Rust runtime sets `SIGPIPE` to `SIG_IGN` before `main` ever runs, so a
/// write to a closed pipe (`head` exiting once it has its 20 lines) comes
/// back as an `EPIPE` `io::Error` instead of killing the process the way
/// every other Unix CLI expects -- and something downstream (a `writeln!`
/// on stdout with `.unwrap()`/`?` reaching `main`'s `Result`) turns that
/// error into a panic. Piping `--json` output into `head`/`jq` is exactly
/// how this project's own the design expects it to be used, so this must
/// not panic. Restoring the default disposition here, at the very first
/// instruction of `main` (before any other code can write a byte), makes
/// SIGPIPE terminate the process the normal Unix way again, matching every
/// other CLI's behavior. Windows has no `SIGPIPE` at all, hence `cfg(unix)`.
#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY: `signal` with `SIGPIPE`/`SIG_DFL` are both plain, always-valid
    // integer constants; this is called once, at the very start of `main`,
    // before any other thread exists or any signal handler could already be
    // relying on `SIG_IGN`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

/// `shr-exec::SystemRunner::run` emits an INFO-level `tracing` event
/// per executed command, but a library must never configure global logging
/// -- that belongs here, the one binary entry point. Quiet by default for
/// THAT per-command trace (an operator's terminal must not fill up with
/// command traces on an ordinary `create`/`expand`): with no `RUST_LOG` set
/// (or an invalid one), the fallback filter is `"warn"`, not `"off"` --
/// INFO/DEBUG/TRACE stay suppressed, so stdout/stderr are still
/// byte-for-byte what they were before that fix for ordinary commands, and the
/// `--json` contract on stdout is untouched either way (this writes to
/// stderr only).
///
/// `"off"` (this function's behavior before that fix) also silenced
/// `OrchestrationEngine::notify`'s `tracing::warn!` event -- the ONE
/// mechanism that gets a degraded-band/array-missing/scrub-error alert in
/// front of an operator watching `journalctl -u shr-rs-health-check
/// .service` (`systemd-notify --status=...` does not reach the journal for
/// a `Type=oneshot` unit; see `NotifyExecutor::systemd_notify`'s doc
/// comment). Every generated timer unit runs with THIS default filter, no
/// `Environment=RUST_LOG=...` line, so a WARN-passing fallback is what
/// makes that alert actually reach an operator who never set `RUST_LOG` by
/// hand -- while still keeping the noisy per-command INFO trace off by
/// default, same as before.
///
/// To see every command shr-rs executes (program, args, exit code,
/// duration): `RUST_LOG=shr_exec=info shr-rs create ...` (or `RUST_LOG=info`
/// for every crate's events, not just shr-exec's).
///
/// the WARN-by-default fallback is right for `UiMode::Cli` (every
/// generated systemd unit runs a subcommand, which forces `Cli` --
/// `detect_ui_mode` never returns `Tui` for those) but wrong for
/// `UiMode::Tui`: `shr-tui::run` (`ratatui::run`) puts the SAME tty this
/// subscriber's `with_writer(stderr)` writes to into the alternate screen.
/// A WARN emitted mid-session (`OrchestrationEngine::notify`, reachable from
/// the TUI's own `reconcile()` call) paints raw text straight into cells
/// ratatui's diffing never repaints -- the display stays corrupted until a
/// full redraw. `UiMode::Tui` therefore falls back to `"off"` (older
/// behavior, TUI-only), same as before this event existed: nothing is
/// silently dropped by this, because the one notify() event actually
/// reachable while a TUI owns the terminal (`NotifyEvent::ScrubErrorsFound`,
/// via `reconcile_group_scrub`) is already shown to the operator through
/// `shr-tui::runtime::describe_reconcile_action`'s `ScrubSelfHealed` text in
/// the reconcile-result overlay -- a real, rendered channel, not a log line
/// nobody is tailing. An explicit `RUST_LOG` still wins in either mode
/// (`try_from_default_env` is checked first) -- that's an operator opting
/// in to stderr output on purpose (e.g. redirected to a file), unchanged
/// from before that fix.
fn init_tracing(mode: UiMode) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback_filter_for(mode)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// The no-`RUST_LOG` fallback level, split out from `init_tracing` so it's
/// testable without touching the process-global `tracing` subscriber (which
/// can only be installed once per test binary). `Cli` keeps the `"warn"`
/// so a generated systemd unit's `journalctl` still gets the alert; `Tui`
/// stays `"off"` so `OrchestrationEngine::notify`'s WARN never lands on the
/// tty ratatui owns -- see `init_tracing`'s doc comment for why dropping it
/// there is not silent.
fn fallback_filter_for(mode: UiMode) -> &'static str {
    match mode {
        UiMode::Cli => "warn",
        UiMode::Tui => "off",
    }
}

/// `--tui`/`--no-tui` are this crate's own dispatch flags -- `shr-cli`'s
/// `Cli` knows nothing about them, so strip both before handing the rest of
/// argv to clap. `program` (argv[0]) is kept so `--help`/usage text still
/// shows the real path the operator invoked.
fn cli_args(program: String, rest: &[String]) -> Vec<String> {
    std::iter::once(program)
        .chain(
            rest.iter()
                .filter(|a| a.as_str() != "--tui" && a.as_str() != "--no-tui")
                .cloned(),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_dispatch_only_flags_but_keeps_argv0_and_everything_else() {
        let rest = vec!["--no-tui".to_string(), "status".to_string(), "--json".to_string()];
        assert_eq!(
            cli_args("shr-rs".to_string(), &rest),
            vec!["shr-rs".to_string(), "status".to_string(), "--json".to_string()]
        );
    }

    #[test]
    fn a_subcommand_run_with_no_dispatch_flags_passes_through_unchanged() {
        let rest = vec!["expand".to_string(), "--add".to_string(), "sdb".to_string()];
        assert_eq!(
            cli_args("shr-rs".to_string(), &rest),
            vec![
                "shr-rs".to_string(),
                "expand".to_string(),
                "--add".to_string(),
                "sdb".to_string()
            ]
        );
    }

    /// `UiMode::Cli` (every generated systemd unit runs a subcommand,
    /// which is always `Cli`) must keep the `"warn"` fallback so
    /// `journalctl -u <unit>` still gets the notify() alert with no
    /// `RUST_LOG` set. `UiMode::Tui` must NOT -- `shr-tui::run` writes to
    /// the same tty this crate's `EnvFilter` fallback governs, and a WARN
    /// mid-session corrupts the alternate screen (see `init_tracing`'s doc
    /// comment). Pins the actual VALUE on both branches, not just that they
    /// differ -- an earlier fix shipped with this fallback string entirely unpinned
    /// (reverting `"warn"` to `"off"` still passed the whole workspace
    /// suite).
    #[test]
    fn cli_mode_keeps_e111s_warn_fallback_and_tui_mode_does_not() {
        assert_eq!(fallback_filter_for(UiMode::Cli), "warn");
        assert_eq!(fallback_filter_for(UiMode::Tui), "off");
    }
}
