//! Operator-authored policy configuration -- distinct from `state.toml`
//! (shr-rs's OWN record of what it manages, rewritten wholesale on every
//! `create`/`expand`/`scrub`/...) because policy holds things an operator
//! configures once by hand and that this project must never silently
//! overwrite or expose alongside machine-maintained state.
//!
//! the webhook URL is the first field here, and it commonly embeds a
//! bearer token or similar secret -- `state.toml` being 0600 is not enough
//! justification to put it there: 0600 makes state.toml unreadable to
//! OTHER users, it does not make it a safe place for an operator's secret
//! to live next to data this project rewrites on every write path (and
//! that every future `state.toml`-reading code path -- `status --json`,
//! Cockpit, a future export/backup feature -- has to remember NOT to leak).
//! A dedicated file, same restrictive 0600 mode, is one less thing every
//! future `state.toml` reader has to get right.
//!
//! this is also where snapshot-automation policy lives --
//! ONE policy file, not one per feature. `toml`'s default (unknown-field-
//! tolerant) deserialization meant adding `[snapshot]` alongside `[notify]`
//! needed no change to how `[notify]` itself loads, and does not break an
//! existing operator's file that predates it (every field defaults).

use crate::error::StateError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Every group this host manages shares this one policy file -- there is
/// no per-group policy scoping (unlike `state.toml`'s per-group
/// filesystem/bands): a webhook URL is an operator's own alerting
/// destination, not a property of any one SHR group.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub notify: NotifyPolicy,
    /// scheduled snapshot automation, sharing this ONE policy
    /// file with `notify` rather than a second config file -- see the
    /// module doc comment.
    #[serde(default)]
    pub snapshot: SnapshotPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyPolicy {
    /// Webhook URL to POST a JSON event payload to on scrub errors, a
    /// degraded transition, and worsening SMART health. `None` (the
    /// default -- nothing configured yet) disables webhook delivery
    /// entirely; this is normal, not an error.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Also report the same events locally, no network dependency,
    /// working even before an operator has configured a webhook. Governs
    /// TWO things now, both fired by `OrchestrationEngine::notify`: the
    /// `systemd-notify --status=...` subprocess call (the original
    /// mechanism -- see `NotifyExecutor::systemd_notify`'s doc comment for
    /// why that alone does NOT reach `journalctl` for any unit this
    /// project generates), and a `tracing::warn!` event that DOES
    /// reach `journalctl -u <unit>` with no `RUST_LOG` needed -- see
    /// `shr-bin::init_tracing`. Defaults to `true`: this project just went
    /// through the "an implemented safety feature that nothing in
    /// production ever called" trap, so a notification channel that needs
    /// explicit opt-in tends to stay silently off forever.
    #[serde(default = "default_true")]
    pub systemd_notify: bool,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        Self { webhook_url: None, systemd_notify: true }
    }
}

fn default_true() -> bool {
    true
}

/// Scheduled snapshot automation policy -- ONE global setting
/// applied to every group (same "no per-group scoping" reasoning as
/// `NotifyPolicy`: schedule/retention are an operator's own housekeeping
/// preference, not a property of any one SHR group).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotPolicy {
    /// Off by default -- unlike `systemd_notify` (a free, local, harmless
    /// default-on channel, the lesson), scheduled snapshots actually
    /// consume disk space and create new subvolumes unattended, so this
    /// stays opt-in, the same "an operator must explicitly turn this on"
    /// posture `webhook_url: None` already takes for the other kind of
    /// automation this file governs. `schedule install` only creates/
    /// enables `shr-rs-snapshot-auto.timer` when this is `true` (the
    /// pruning then also removes it again if a later `schedule install`
    /// finds it turned back off).
    #[serde(default)]
    pub enabled: bool,
    /// A systemd `OnCalendar=` value (`"daily"`, `"weekly"`, or a full
    /// calendar expression) -- passed straight through to the generated
    /// timer unit (`shr_state::conf::write_snapshot_timer_unit`), never
    /// parsed/validated here; systemd itself is the source of truth for
    /// what's a valid calendar expression.
    #[serde(default = "default_snapshot_schedule")]
    pub schedule: String,
    /// How many of shr-rs's OWN automated snapshots to keep per group,
    /// oldest deleted first once a new one pushes the count over this.
    /// Never touches a snapshot `fs snapshot create` made by hand -- see
    /// `OrchestrationEngine::snapshot_auto_run`'s doc comment for exactly
    /// how "ours" is identified.
    #[serde(default = "default_snapshot_keep")]
    pub keep: u32,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self { enabled: false, schedule: default_snapshot_schedule(), keep: default_snapshot_keep() }
    }
}

fn default_snapshot_schedule() -> String {
    "daily".to_string()
}

fn default_snapshot_keep() -> u32 {
    7
}

/// Reads (never writes) `policy.toml` -- this file is operator-authored,
/// the same convention as a typical `/etc/<app>/config.toml`, not
/// something shr-rs generates or rewrites the way it owns `state.toml`/
/// `mdadm.conf`/`fstab`'s managed blocks.
pub struct PolicyStore {
    path: PathBuf,
}

impl PolicyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A missing file means "operator hasn't configured anything yet" --
    /// `PolicyFile::default()` (every channel off except the free, local
    /// `systemd_notify`), not an error. Every OTHER read failure (bad
    /// permissions, malformed TOML) propagates: silently falling back to
    /// "no policy" for a file that DOES exist but can't be read/parsed
    /// would make a typo in an operator's webhook URL fail invisibly
    /// instead of being reported.
    pub fn load(&self) -> Result<PolicyFile, StateError> {
        if !self.path.exists() {
            return Ok(PolicyFile::default());
        }
        let content = std::fs::read_to_string(&self.path)?;
        Ok(toml::from_str(&content)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn a_missing_policy_file_loads_as_every_channel_defaulted_not_an_error() {
        let dir = tempdir().unwrap();
        let store = PolicyStore::new(dir.path().join("policy.toml"));

        let policy = store.load().unwrap();

        assert_eq!(policy.notify.webhook_url, None);
        assert!(policy.notify.systemd_notify, "the free, local channel must default ON (earlier lesson)");
    }

    #[test]
    fn loads_a_configured_webhook_url() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "[notify]\nwebhook_url = \"https://hooks.example.com/abc?token=secret\"\n").unwrap();
        let store = PolicyStore::new(&path);

        let policy = store.load().unwrap();

        assert_eq!(policy.notify.webhook_url.as_deref(), Some("https://hooks.example.com/abc?token=secret"));
    }

    #[test]
    fn systemd_notify_can_be_explicitly_disabled() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "[notify]\nsystemd_notify = false\n").unwrap();
        let store = PolicyStore::new(&path);

        assert!(!store.load().unwrap().notify.systemd_notify);
    }

    #[test]
    fn a_malformed_policy_file_reports_an_error_not_a_silent_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();
        let store = PolicyStore::new(&path);

        assert!(store.load().is_err(), "a malformed file must not silently fall back to defaults");
    }

    #[test]
    fn a_policy_file_with_both_notify_and_snapshot_sections_loads_both() {
        // `[snapshot]` now exists alongside `[notify]` in the
        // SAME file -- proves loading one section doesn't disturb the
        // other, the same "one shared policy file" the module doc comment
        // describes.
        let dir = tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(
            &path,
            "[notify]\nwebhook_url = \"https://hooks.example.com/x\"\n\n[snapshot]\nenabled = true\nschedule = \"daily\"\nkeep = 7\n",
        )
        .unwrap();
        let store = PolicyStore::new(&path);

        let policy = store.load().unwrap();
        assert_eq!(policy.notify.webhook_url.as_deref(), Some("https://hooks.example.com/x"));
        assert!(policy.snapshot.enabled);
        assert_eq!(policy.snapshot.schedule, "daily");
        assert_eq!(policy.snapshot.keep, 7);
    }

    #[test]
    fn a_missing_snapshot_section_defaults_to_disabled_with_sane_retention() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "[notify]\nsystemd_notify = false\n").unwrap();
        let store = PolicyStore::new(&path);

        let policy = store.load().unwrap();
        assert!(!policy.snapshot.enabled, "snapshot automation must stay opt-in, unlike systemd_notify");
        assert_eq!(policy.snapshot.schedule, "daily");
        assert_eq!(policy.snapshot.keep, 7);
    }

    #[test]
    fn a_snapshot_section_can_set_enabled_without_repeating_schedule_or_keep() {
        // `#[serde(default = ...)]` on `schedule`/`keep` individually --
        // proves an operator turning automation on doesn't also have to
        // spell out every other field just to avoid a TOML parse error.
        let dir = tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "[snapshot]\nenabled = true\n").unwrap();
        let store = PolicyStore::new(&path);

        let policy = store.load().unwrap();
        assert!(policy.snapshot.enabled);
        assert_eq!(policy.snapshot.schedule, "daily");
        assert_eq!(policy.snapshot.keep, 7);
    }
}
