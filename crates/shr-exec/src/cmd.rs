use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("Required runtime prerequisite is unavailable: {0}")]
    Prerequisite(String),

    #[error("Command IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Command '{program}' failed with exit code {exit_code}: {stderr}")]
    NonZeroExit {
        program: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },

    #[error("Safety violation: {0}")]
    SafetyViolation(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError>;
    fn is_dry_run(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct SystemRunner;

impl SystemRunner {
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let start = std::time::Instant::now();
        let output = std::process::Command::new(program).args(args).output()?;

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // This project's whole job is issuing destructive storage
        // commands (mdadm/lvm/parted/btrfs); only the REAL runner logs --
        // `DryRunRunner` and every test mock never execute anything, so
        // logging them would be noise about nothing that happened.
        log_executed_command(program, args, exit_code, start.elapsed());

        if !output.status.success() {
            return Err(ExecError::NonZeroExit {
                program: program.to_string(),
                exit_code,
                stdout,
                stderr,
            });
        }

        Ok(CommandOutput { stdout, stderr })
    }

    fn is_dry_run(&self) -> bool {
        false
    }
}

/// Audit trail: one `tracing` event per command `SystemRunner` actually
/// executes, carrying its arguments, exit code, and wall-clock duration --
/// so a post-incident read of the log shows exactly what ran against real
/// disks. Silent unless the operator opts in (`RUST_LOG`, see
/// `shr-bin`'s subscriber setup) -- this crate never configures a
/// subscriber itself (a library must not own global logging).
///
/// `curl` is the one command in this workspace whose argument can carry a
/// secret: `NotifyExecutor::webhook` passes an operator-configured webhook
/// URL straight through as an arg, and that URL commonly embeds a bearer
/// token in its query string (see `shr_state::policy::NotifyPolicy::
/// webhook_url`'s doc comment). Every other command this project issues
/// (mdadm/lvm/parted/btrfs/systemctl/smartctl/the sysfs `sh -c echo`
/// writes) takes only device paths, sizes, and RAID/FS parameters -- none
/// of it is credential-shaped -- so only `curl`'s args are withheld.
fn log_executed_command(program: &str, args: &[&str], exit_code: i32, elapsed: std::time::Duration) {
    let elapsed_ms = elapsed.as_millis();
    if program == "curl" {
        tracing::info!(
            program,
            exit_code,
            elapsed_ms,
            "executed (args redacted: may carry a webhook secret)"
        );
    } else {
        tracing::info!(program, ?args, exit_code, elapsed_ms, "executed");
    }
}

/// Writes `value` into a `/proc`/`/sys` control file via `CommandRunner`
/// (never raw `std::fs` -- unmockable and always an IO error on the Windows
/// dev host `cargo test` runs on natively). These files only accept a
/// whole-file overwrite via shell redirection -- there is no
/// `CommandRunner::run`-only way to do `echo VALUE > path`, since `run`
/// execs a program directly with no shell to interpret `>`.
///
/// `sh -c` was chosen over `tee path <<< VALUE` (Stage B's decision,
/// `shr-exec::throttle`'s reshape speed writes; reused verbatim here for
/// `MdadmExecutor::scrub_start`/`scrub_cancel` -- so the project has
/// exactly one sysfs-write convention, not two) because it keeps the
/// recorded (dry-run) command literally identical to the manual
/// `echo VALUE ... path` an operator would type by hand, which `tee` does
/// not read as naturally.
pub fn write_sysfs(runner: &dyn CommandRunner, path: &str, value: &str) -> Result<(), ExecError> {
    runner.run("sh", &["-c", &format!("echo {value} > {path}")])?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct DryRunRunner {
    recorded_commands: Arc<Mutex<Vec<String>>>,
}

impl DryRunRunner {
    pub fn new() -> Self {
        Self {
            recorded_commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn get_recorded(&self) -> Vec<String> {
        self.recorded_commands.lock().unwrap().clone()
    }
}

impl CommandRunner for DryRunRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let cmd_str = format!("{} {}", program, args.join(" "));
        self.recorded_commands.lock().unwrap().push(cmd_str);
        Ok(CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn is_dry_run(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// Captures every byte a `tracing_subscriber::fmt` subscriber writes,
    /// so a test can assert on the rendered event text without racing a
    /// real log file or `stdout`/`stderr` capture.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Installs a `tracing` subscriber ONLY for the duration of `f`
    /// (`with_default`, never `init` -- this is a test, not the process-
    /// wide setup that belongs solely to `shr-bin`) and returns everything
    /// it wrote.
    fn capture_tracing(f: impl FnOnce()) -> String {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        log.text()
    }

    // A command guaranteed to exist and succeed on whichever OS `cargo
    // test` actually runs on (this project's dev host is Windows; `sh`
    // covers a Linux CI host if this ever runs on one).
    #[cfg(windows)]
    const ECHO: (&str, &[&str]) = ("cmd", &["/C", "echo hi"]);
    #[cfg(not(windows))]
    const ECHO: (&str, &[&str]) = ("sh", &["-c", "echo hi"]);

    #[test]
    fn system_runner_logs_program_args_exit_code_and_duration() {
        let (program, args) = ECHO;
        let output = capture_tracing(|| {
            SystemRunner::new().run(program, args).unwrap();
        });

        assert!(
            output.contains(program),
            "expected the program name in the log, got: {output}"
        );
        assert!(
            output.contains("exit_code"),
            "expected an exit code field, got: {output}"
        );
        assert!(
            output.contains("elapsed_ms"),
            "expected a duration field, got: {output}"
        );
        assert!(
            output.contains("args"),
            "expected the arguments recorded, got: {output}"
        );
    }

    #[test]
    fn system_runner_never_logs_curl_arguments_since_a_webhook_url_can_carry_a_secret() {
        // No real `curl` process here (and no real network call wanted in
        // a unit test) -- the redaction is a pure function of the program
        // name, exercised directly against the logging helper `run` calls.
        let output = capture_tracing(|| {
            log_executed_command(
                "curl",
                &["-d", "{}", "https://hooks.example.com/x?token=super-secret"],
                0,
                std::time::Duration::from_millis(5),
            );
        });

        assert!(
            output.contains("curl"),
            "expected the program name still logged, got: {output}"
        );
        assert!(
            !output.contains("super-secret"),
            "webhook token leaked into the log: {output}"
        );
        assert!(
            !output.contains("hooks.example.com"),
            "webhook URL leaked into the log: {output}"
        );
    }

    #[test]
    fn dry_run_runner_never_emits_a_tracing_event() {
        // DryRunRunner never executes anything -- an log line about it
        // would describe a command that never actually ran.
        let output = capture_tracing(|| {
            DryRunRunner::new()
                .run("mdadm", &["--create", "/dev/md0"])
                .unwrap();
        });

        assert!(
            output.trim().is_empty(),
            "DryRunRunner must stay silent, got: {output}"
        );
    }
}
