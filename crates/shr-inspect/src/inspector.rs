//! The `Inspector` abstraction: a source of parsed system state. The real
//! [`SystemInspector`] runs `lsblk` / reads `/proc/mdstat` / runs `smartctl`;
//! [`StaticInspector`] serves pre-parsed fixtures for tests and for feeding
//! captured data through the same code path.

use std::collections::HashMap;

use thiserror::Error;

use crate::identity::{ByIdIndex, IdentityError};
use crate::lsblk::{parse_lsblk, LsblkOutput};
use crate::mdstat::{parse_mdstat, MdStat};
use crate::smart::{parse_smartctl, SmartInfo};

#[derive(Debug, Error)]
pub enum InspectError {
    #[error("failed to run `{cmd}`: {source}")]
    Spawn { cmd: String, source: std::io::Error },
    #[error("`{cmd}` failed (status {code:?}): {stderr}")]
    Status {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("failed to read {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("failed to parse {what} output: {source}")]
    Parse { what: String, source: serde_json::Error },
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// A source of parsed system inspection data.
///
/// `Send + Sync` (mirroring `shr_exec::CommandRunner`'s identical bound) so
/// a `&dyn Inspector` can be captured by a background thread -- the TUI's
/// Add Disk wizard needs this to run its real, potentially long-running
/// `execute()` off the terminal event loop's thread.
pub trait Inspector: Send + Sync {
    fn block_devices(&self) -> Result<LsblkOutput, InspectError>;
    fn mdstat(&self) -> Result<MdStat, InspectError>;
    /// SMART for a single disk by device name (e.g. `sda`).
    fn smart(&self, dev: &str) -> Result<SmartInfo, InspectError>;
    /// Stable by-id index. Default empty; real systems override.
    fn by_id_index(&self) -> Result<ByIdIndex, InspectError> {
        Ok(ByIdIndex::empty())
    }
    /// The most recent `max_lines` kernel log lines, oldest first -- the
    /// TUI's Logs tab reads this. There is no dedicated shr-rs log
    /// store (nothing in `state.toml`'s schema records scrub/reshape
    /// events yet), so this is deliberately the honest, already-real
    /// substitute: the kernel ring buffer via `journalctl -k` naturally
    /// carries mdadm/btrfs/block-layer messages without shr-rs having to
    /// invent its own logging pipeline. Default empty; real systems
    /// override.
    fn recent_log_lines(&self, _max_lines: usize) -> Result<Vec<String>, InspectError> {
        Ok(Vec::new())
    }
}

/// The lsblk columns shr-rs relies on.
pub const LSBLK_COLUMNS: &str = "NAME,SIZE,TYPE,MODEL,SERIAL,ROTA,TRAN,PARTUUID,FSTYPE,MOUNTPOINT,PTTYPE";

/// Runs the real system tools. Intended for the Linux target; on other hosts
/// the commands simply fail to spawn.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemInspector;

impl Inspector for SystemInspector {
    fn block_devices(&self) -> Result<LsblkOutput, InspectError> {
        let stdout = run("lsblk", &["-J", "-b", "-o", LSBLK_COLUMNS], true)?;
        parse_lsblk(&stdout).map_err(|e| InspectError::Parse {
            what: "lsblk".into(),
            source: e,
        })
    }

    fn mdstat(&self) -> Result<MdStat, InspectError> {
        let text = std::fs::read_to_string("/proc/mdstat").map_err(|e| InspectError::Read {
            path: "/proc/mdstat".into(),
            source: e,
        })?;
        Ok(parse_mdstat(&text))
    }

    fn smart(&self, dev: &str) -> Result<SmartInfo, InspectError> {
        let path = format!("/dev/{dev}");
        // smartctl uses nonzero exit codes for warnings but still emits JSON
        // (with `smartctl.exit_status` inside), so do NOT treat nonzero as a
        // hard failure here — parse the JSON regardless.
        let stdout = run("smartctl", &["-j", "-H", "-A", "-i", &path], false)?;
        parse_smartctl(&stdout).map_err(|e| InspectError::Parse {
            what: "smartctl".into(),
            source: e,
        })
    }

    fn by_id_index(&self) -> Result<ByIdIndex, InspectError> {
        Ok(ByIdIndex::scan_system()?)
    }

    fn recent_log_lines(&self, max_lines: usize) -> Result<Vec<String>, InspectError> {
        let n = max_lines.to_string();
        // `-k`: kernel ring buffer only -- this is a general-purpose "what's
        // going on" tab, not a filter tuned to any one subsystem, so no
        // `-g`/`--grep` narrowing. Nonzero exit is NOT a hard failure here
        // (mirrors `smart()`'s `fail_on_nonzero: false`): an empty journal
        // or a container without persistent logging still exits 0 with
        // nothing to show, but a locked-down environment can exit nonzero
        // while still printing a usable diagnostic to stdout/stderr that's
        // more helpful shown than swallowed.
        let stdout = run(
            "journalctl",
            &["-k", "--no-pager", "-n", &n, "-o", "short-iso"],
            false,
        )?;
        Ok(stdout.lines().map(str::to_string).collect())
    }
}

/// Run a command, returning stdout. If `fail_on_nonzero`, a nonzero exit is an
/// error; otherwise stdout is returned regardless (for tools like smartctl).
fn run(cmd: &str, args: &[&str], fail_on_nonzero: bool) -> Result<String, InspectError> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| InspectError::Spawn {
            cmd: cmd.into(),
            source: e,
        })?;
    if fail_on_nonzero && !output.status.success() {
        return Err(InspectError::Status {
            cmd: cmd.into(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// An `Inspector` backed by pre-parsed data — for tests and replaying captures.
#[derive(Debug, Clone, Default)]
pub struct StaticInspector {
    pub lsblk: LsblkOutput,
    pub mdstat: MdStat,
    pub smart: HashMap<String, SmartInfo>,
    pub by_id: ByIdIndex,
    pub logs: Vec<String>,
}

impl StaticInspector {
    /// Build from raw command output strings (as if captured from a system).
    pub fn from_raw(
        lsblk_json: &str,
        mdstat_text: &str,
        smart_json: HashMap<String, String>,
    ) -> Result<Self, InspectError> {
        let lsblk = parse_lsblk(lsblk_json).map_err(|e| InspectError::Parse {
            what: "lsblk".into(),
            source: e,
        })?;
        let mut smart = HashMap::new();
        for (dev, json) in smart_json {
            let info = parse_smartctl(&json).map_err(|e| InspectError::Parse {
                what: "smartctl".into(),
                source: e,
            })?;
            smart.insert(dev, info);
        }
        Ok(Self {
            lsblk,
            mdstat: parse_mdstat(mdstat_text),
            smart,
            by_id: ByIdIndex::empty(),
            logs: Vec::new(),
        })
    }

    /// Attach a synthetic by-id index (tests / captured replays).
    pub fn with_by_id(mut self, by_id: ByIdIndex) -> Self {
        self.by_id = by_id;
        self
    }

    /// Attach synthetic log lines (tests exercising the TUI's Logs tab).
    pub fn with_logs(mut self, logs: Vec<String>) -> Self {
        self.logs = logs;
        self
    }
}

impl Inspector for StaticInspector {
    fn block_devices(&self) -> Result<LsblkOutput, InspectError> {
        Ok(self.lsblk.clone())
    }
    fn mdstat(&self) -> Result<MdStat, InspectError> {
        Ok(self.mdstat.clone())
    }
    fn smart(&self, dev: &str) -> Result<SmartInfo, InspectError> {
        Ok(self.smart.get(dev).cloned().unwrap_or_default())
    }
    fn by_id_index(&self) -> Result<ByIdIndex, InspectError> {
        Ok(self.by_id.clone())
    }
    fn recent_log_lines(&self, max_lines: usize) -> Result<Vec<String>, InspectError> {
        let skip = self.logs.len().saturating_sub(max_lines);
        Ok(self.logs[skip..].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_log_lines_default_is_empty_not_an_error() {
        let inspector = StaticInspector::default();
        assert_eq!(inspector.recent_log_lines(10).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn recent_log_lines_returns_the_most_recent_n_lines_oldest_first() {
        let logs = vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()];
        let inspector = StaticInspector::default().with_logs(logs);

        assert_eq!(
            inspector.recent_log_lines(2).unwrap(),
            vec!["c".to_string(), "d".to_string()]
        );
        assert_eq!(
            inspector.recent_log_lines(100).unwrap(),
            vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()],
            "asking for more lines than exist must return everything, not panic"
        );
    }
}
