use crate::cmd::{CommandRunner, ExecError};

/// Delivers a notification via `curl` (webhook) or `systemd-notify` (local
/// status), through `CommandRunner` like every other external program this
/// project invokes -- never a raw HTTP client crate, so this stays
/// mockable the same way `MdadmExecutor`/`BtrfsExecutor` already are, with
/// no new dependency surface.
///
/// Deliberately its own tiny wrapper, not folded into `BtrfsExecutor`/
/// `MdadmExecutor` -- notification delivery has nothing to do with disk/
/// filesystem state. Both methods here are best-effort from the CALLER's
/// perspective: `shr-orchestrate`'s firing code discards whatever `Result`
/// comes back rather than propagating it, since a dead webhook or a
/// `systemd-notify` call outside a supervised process must never make an
/// otherwise-successful scrub/reconcile/health-check look like it failed
/// (the explicit requirement). This wrapper itself still returns a real
/// `Result` rather than swallowing errors here -- so that guarantee is
/// visible and testable at the call site, not hidden two layers down.
pub struct NotifyExecutor<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> NotifyExecutor<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }

    /// POST `json_body` to `url`. No shell involved -- `args` go straight
    /// to `exec`, never through `sh -c` -- so nothing in `json_body`/`url`
    /// (a group name, a scrub error count) can ever be interpreted as
    /// shell syntax. `--max-time 10`: an unreachable/hanging webhook
    /// endpoint must not block the caller indefinitely -- 10s is generous
    /// for a same-datacenter/same-LAN webhook receiver without risking a
    /// scrub-result observation or health check stalling on a dead
    /// endpoint.
    pub fn webhook(&self, url: &str, json_body: &str) -> Result<(), ExecError> {
        self.runner.run(
            "curl",
            &["-fsS", "--max-time", "10", "-X", "POST", "-H", "Content-Type: application/json", "-d", json_body, url],
        )?;
        Ok(())
    }

    /// Report `status` locally via `systemd-notify --status=...`.
    ///
    /// This does NOT reach `journalctl`
    /// for any unit this project generates. Every generated `ExecStart=`
    /// runs under `Type=oneshot` with no `NotifyAccess=`, so systemd sets
    /// no `$NOTIFY_SOCKET` and `systemd-notify` exits nonzero having sent
    /// nothing (`No status data could be sent: $NOTIFY_SOCKET was not
    /// set`). Adding `NotifyAccess=all` to a oneshot unit is NOT a fix
    /// either -- delivery then succeeds DURING the run, but systemd clears
    /// `StatusText` the moment a oneshot process exits, so `systemctl
    /// status` shows nothing afterward either. The only place this call's
    /// `--status=` text is actually visible is `systemctl status <unit>`
    /// while a LONG-RUNNING `Type=notify` service (not a oneshot) that made
    /// this same call is still running -- shr-rs has no such service today.
    /// Kept anyway (harmless, and correct if this project ever gains a
    /// long-running notify-aware service) but `OrchestrationEngine::notify`
    /// (`shr-orchestrate::engine`) is what actually gets an event in front
    /// of the operator now, via `tracing::warn!` to this process's own
    /// stderr, which the journal DOES capture for every generated unit
    /// (systemd's own default, no `StandardError=` needed).
    ///
    /// A no-op (nonzero exit, no crash) outside a systemd-supervised
    /// process (no `NOTIFY_SOCKET` set) -- `systemd-notify` itself already
    /// handles that, so this wrapper does not need its own "am I
    /// supervised" detection.
    pub fn systemd_notify(&self, status: &str) -> Result<(), ExecError> {
        self.runner.run("systemd-notify", &[&format!("--status={status}")])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::NotifyExecutor;
    use crate::cmd::{CommandOutput, CommandRunner, ExecError};
    use std::sync::Mutex;

    #[derive(Default)]
    struct SpyRunner {
        commands: Mutex<Vec<String>>,
        fail: bool,
    }
    impl SpyRunner {
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
        fn failing() -> Self {
            Self { fail: true, ..Self::default() }
        }
    }
    impl CommandRunner for SpyRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
            self.commands.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            if self.fail {
                return Err(ExecError::NonZeroExit {
                    program: program.to_string(),
                    exit_code: 7,
                    stdout: String::new(),
                    stderr: "simulated failure".to_string(),
                });
            }
            Ok(CommandOutput { stdout: String::new(), stderr: String::new() })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    #[test]
    fn webhook_posts_the_json_body_to_the_url_via_curl_never_a_shell() {
        let runner = SpyRunner::default();
        NotifyExecutor::new(&runner).webhook("https://hooks.example.com/x", r#"{"kind":"Degraded"}"#).unwrap();

        let cmds = runner.commands();
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert!(cmds[0].starts_with("curl "), "{cmds:?}");
        assert!(cmds[0].contains("-X POST"), "{cmds:?}");
        assert!(cmds[0].contains(r#"{"kind":"Degraded"}"#), "{cmds:?}");
        assert!(cmds[0].contains("https://hooks.example.com/x"), "{cmds:?}");
        assert!(cmds[0].contains("--max-time 10"), "must bound how long a dead endpoint can block: {cmds:?}");
    }

    #[test]
    fn webhook_failure_surfaces_as_a_real_error_here_for_the_caller_to_choose_what_to_do_with() {
        let runner = SpyRunner::failing();
        assert!(NotifyExecutor::new(&runner).webhook("https://dead.example.com", "{}").is_err());
    }

    #[test]
    fn systemd_notify_reports_status_and_tolerates_running_unsupervised() {
        let runner = SpyRunner::default();
        NotifyExecutor::new(&runner).systemd_notify("scrub found 3 errors").unwrap();
        let cmds = runner.commands();
        assert_eq!(cmds, vec!["systemd-notify --status=scrub found 3 errors"]);

        // Outside a systemd-supervised process `systemd-notify` itself
        // exits nonzero (no NOTIFY_SOCKET) -- this wrapper surfaces that
        // as a real Err, same as any other command failure; it is the
        // ENGINE's firing code that decides to discard it, not this
        // wrapper pretending it succeeded.
        let failing = SpyRunner::failing();
        assert!(NotifyExecutor::new(&failing).systemd_notify("x").is_err());
    }
}
