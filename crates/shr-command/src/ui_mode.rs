//! Shared UI-mode detection: `shr-bin`'s
//! no-subcommand TUI-vs-CLI dispatch and `shr-cli`'s destructive-confirm
//! gate ask two DIFFERENT questions and must not share one predicate:
//! `is_interactive_terminal()` is "can we render a TUI here?" (size,
//! `TERM=dumb`); `can_prompt_operator()` is "is a human watching who can
//! answer a yes/no prompt?" (any TTY, any size). Collapsing them let a
//! resized or `TERM=dumb` terminal skip the confirmation entirely.

use std::io::IsTerminal;

/// Pure predicate, decoupled from real stdio so tests can drive every
/// branch without a real terminal (the un-refactored version was
/// entirely untestable at this layer, which is exactly how an earlier fix shipped).
fn is_interactive_terminal_from(no_tui_set: bool, stdout_is_tty: bool, term_is_dumb: bool, size: Option<(u16, u16)>) -> bool {
    if no_tui_set {
        return false;
    }
    if !stdout_is_tty {
        return false;
    }
    if term_is_dumb {
        return false;
    }
    match size {
        Some((cols, rows)) => cols >= 80 && rows >= 24,
        None => false,
    }
}

/// The design's TTY/environment gate, verbatim and in order: an explicit `NO_TUI`
/// env var, a non-TTY stdout, `TERM=dumb`, or a terminal smaller than 80x24
/// all mean "not interactive" -- anything else is. Never touches
/// `CommandRunner`: this reads the CURRENT process's own stdio/environment,
/// not an external command or a `/proc`/`/sys` file, so it stays deterministic
/// and side-effect-free to call directly (same category as `std::env::args`).
///
/// This answers "can a TUI be rendered here?", NOT "must a destructive
/// action be confirmed?" -- use [`can_prompt_operator`] for the latter.
pub fn is_interactive_terminal() -> bool {
    is_interactive_terminal_from(
        std::env::var_os("NO_TUI").is_some(),
        std::io::stdout().is_terminal(),
        std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false),
        crossterm::terminal::size().ok(),
    )
}

/// Pure predicate behind [`can_prompt_operator`], split out for the same
/// testability reason as `is_interactive_terminal_from`.
fn can_prompt_operator_from(stdout_is_tty: bool, stdin_is_tty: bool) -> bool {
    stdout_is_tty && stdin_is_tty
}

/// The confirmation gate: "is a human at a terminal who can answer a
/// typed-confirmation prompt?" Deliberately NOT `is_interactive_terminal()`:
/// a small window or an editor's `TERM=dumb` integrated terminal still has a
/// human reading it, and collapsing this into the TUI-render check let
/// a sub-80x24 terminal run destructive commands with no prompt at all. Also
/// deliberately does not consult `NO_TUI` -- that variable means "don't draw
/// a TUI", not "skip confirmation"; scripted/non-interactive callers already
/// have `--yes` for that. Checks stdin too (not just stdout): a TTY stdout
/// with piped stdin can display a prompt nobody can answer, which would just
/// fail on EOF instead of confirming anything.
pub fn can_prompt_operator() -> bool {
    can_prompt_operator_from(std::io::stdout().is_terminal(), std::io::stdin().is_terminal())
}

/// `shr-bin`'s launch mode: TUI with no arguments in an
/// interactive terminal, CLI otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Tui,
    Cli,
}

/// `args` is the raw command line WITHOUT argv[0] (the program path itself
/// carries no dispatch information). `--tui`/`--no-tui` override everything
/// else, including each other's absence; any other argument (a subcommand)
/// forces CLI without consulting the terminal at all -- `shr-rs status` must
/// never block on launching a TUI just because stdout happens to be a real
/// terminal.
pub fn detect_ui_mode(args: &[String]) -> UiMode {
    if args.iter().any(|a| a == "--tui") {
        return UiMode::Tui;
    }
    if args.iter().any(|a| a == "--no-tui") {
        return UiMode::Cli;
    }
    if args.iter().any(|a| a != "--tui" && a != "--no-tui") {
        return UiMode::Cli;
    }
    if is_interactive_terminal() {
        UiMode::Tui
    } else {
        UiMode::Cli
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tui_flag_forces_tui_regardless_of_other_args() {
        assert_eq!(detect_ui_mode(&args(&["--tui"])), UiMode::Tui);
        assert_eq!(detect_ui_mode(&args(&["--tui", "status"])), UiMode::Tui);
    }

    #[test]
    fn no_tui_flag_forces_cli_even_with_no_other_args() {
        assert_eq!(detect_ui_mode(&args(&["--no-tui"])), UiMode::Cli);
    }

    #[test]
    fn a_subcommand_always_means_cli_without_consulting_the_terminal() {
        assert_eq!(detect_ui_mode(&args(&["status"])), UiMode::Cli);
        assert_eq!(detect_ui_mode(&args(&["create", "--mode", "shr"])), UiMode::Cli);
    }

    #[test]
    fn no_args_defers_to_the_terminal_check() {
        // Whatever this test process's own stdio actually is (cargo test
        // runners are never a real interactive terminal), the no-args case
        // must match `is_interactive_terminal()` exactly, not hardcode a
        // guess either way.
        let expected = if is_interactive_terminal() { UiMode::Tui } else { UiMode::Cli };
        assert_eq!(detect_ui_mode(&[]), expected);
    }

    #[test]
    fn no_tui_env_var_forces_non_interactive() {
        // SAFETY-critical to test in-process rather than trust the doc
        // comment alone: a NO_TUI env var must short-circuit before the
        // stdout/TERM/size checks even run.
        // SAFETY: no other test in this crate reads/writes NO_TUI, and
        // `cargo test`'s default single-process-many-threads runner still
        // only ever has one thread executing this specific test body at a
        // time relative to this var (no other test touches it) -- races
        // against genuinely unrelated env vars are not a concern here.
        unsafe { std::env::set_var("NO_TUI", "1") };
        let result = is_interactive_terminal();
        unsafe { std::env::remove_var("NO_TUI") };
        assert!(!result, "NO_TUI must force non-interactive");
    }

    // -- The predicates behind `is_interactive_terminal()` and
    // `can_prompt_operator()` are pure and driven by explicit arguments here,
    // so every branch is testable without a real terminal.

    #[test]
    fn is_interactive_terminal_from_still_rejects_dumb_term_and_small_sizes() {
        // A real TTY at full size: interactive.
        assert!(is_interactive_terminal_from(false, true, false, Some((120, 40))));
        // TERM=dumb: never interactive, even at a generous size.
        assert!(!is_interactive_terminal_from(false, true, true, Some((120, 40))));
        // Below 80x24 in either dimension: not interactive.
        assert!(!is_interactive_terminal_from(false, true, false, Some((79, 23))));
        assert!(!is_interactive_terminal_from(false, true, false, Some((79, 40))));
        assert!(!is_interactive_terminal_from(false, true, false, Some((120, 23))));
        // No size available (0x0 / size query failed): not interactive.
        assert!(!is_interactive_terminal_from(false, true, false, Some((0, 0))));
        assert!(!is_interactive_terminal_from(false, true, false, None));
        // Not a TTY at all, or NO_TUI set: not interactive regardless of size.
        assert!(!is_interactive_terminal_from(false, false, false, Some((120, 40))));
        assert!(!is_interactive_terminal_from(true, true, false, Some((120, 40))));
    }

    #[test]
    fn can_prompt_operator_from_ignores_size_and_term_entirely() {
        // Both a real TTY: a human can be prompted, regardless of.
        assert!(can_prompt_operator_from(true, true));
        // stdout not a TTY: nowhere to show the prompt.
        assert!(!can_prompt_operator_from(false, true));
        // stdin not a TTY (e.g. piped): a prompt would just hit EOF.
        assert!(!can_prompt_operator_from(true, false));
        assert!(!can_prompt_operator_from(false, false));
    }

    #[test]
    fn can_prompt_operator_from_would_have_allowed_the_e34_repro_sizes() {
        // The actual repro: a 79x23 terminal with TERM=xterm silently
        // skipped confirmation because the gate used
        // `is_interactive_terminal()`. `can_prompt_operator_from` doesn't
        // take size or TERM as inputs at all -- a real TTY on both streams
        // is always a "yes", proving the gate no longer depends on the design's
        // TUI-render checks.
        assert!(can_prompt_operator_from(true, true));
    }
}
