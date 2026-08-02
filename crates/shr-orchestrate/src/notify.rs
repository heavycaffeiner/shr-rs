//! Notification events -- fired on a scrub finding errors, a band
//! transitioning to degraded, and worsening SMART health (the design #44,
//!). This module only knows how to DESCRIBE an event (human status
//! line + JSON payload); delivery itself lives in `shr_exec::NotifyExecutor`
//! (webhook/systemd-notify, both `CommandRunner`-routed subprocess calls)
//! and a direct `tracing::warn!` in `OrchestrationEngine::notify`
//! (`engine.rs`) -- the actual journal-visible channel, since
//! `systemd-notify --status=...` alone never reaches `journalctl` for any
//! unit this project generates. `notify()` is what calls all of it
//! together, discarding delivery failures per the "must not fail the
//! underlying operation" requirement.

#[derive(Debug, Clone, PartialEq)]
pub enum NotifyEvent {
    ScrubErrorsFound { group: String, band_index: u8, error_count: u64 },
    Degraded { group: String, band_index: u8 },
    SmartWorsened { group: String, band_index: u8, reallocated_delta: u64 },
    /// `check_health()`'s per-band read of `/sys/block/<md>/md/
    /// degraded` found no such array at all (not merely reduced-redundancy
    /// -- entirely unassembled, e.g. a reboot came back without its member
    /// devices). Distinct from `Degraded`: a missing array is total loss of
    /// this band, a strictly worse state than a still-live-but-reduced one,
    /// and the operator needs to know which of the two they're looking at.
    ArrayMissing { group: String, band_index: u8 },
}

impl NotifyEvent {
    /// JSON body a webhook receiver gets. Built with `serde_json::json!`
    /// (proper escaping for free) rather than hand-formatted -- a group
    /// name is operator-chosen free text (`create --name`, no charset
    /// restriction -- see `shr_state::conf`'s unit-name sanitizer for the
    /// same fact elsewhere in this project) and must never be able to
    /// break the JSON structure.
    pub fn to_json(&self) -> String {
        let value = match self {
            NotifyEvent::ScrubErrorsFound { group, band_index, error_count } => serde_json::json!({
                "kind": "scrub_errors_found",
                "group": group,
                "band_index": band_index,
                "error_count": error_count,
            }),
            NotifyEvent::Degraded { group, band_index } => serde_json::json!({
                "kind": "degraded",
                "group": group,
                "band_index": band_index,
            }),
            NotifyEvent::SmartWorsened { group, band_index, reallocated_delta } => serde_json::json!({
                "kind": "smart_worsened",
                "group": group,
                "band_index": band_index,
                "reallocated_delta": reallocated_delta,
            }),
            NotifyEvent::ArrayMissing { group, band_index } => serde_json::json!({
                "kind": "array_missing",
                "group": group,
                "band_index": band_index,
            }),
        };
        value.to_string()
    }

    /// One-line human summary. Used for `systemd-notify --status=...` AND
    /// as the `tracing::warn!` message `OrchestrationEngine::notify`
    /// emits -- the one that actually reaches `journalctl -u <unit>` with
    /// no `RUST_LOG` needed; see that function's doc comment.
    pub fn status_line(&self) -> String {
        match self {
            NotifyEvent::ScrubErrorsFound { group, band_index, error_count } => {
                format!("shr-rs: group `{group}` band {band_index} scrub found {error_count} error(s)")
            }
            NotifyEvent::Degraded { group, band_index } => {
                format!("shr-rs: group `{group}` band {band_index} is DEGRADED")
            }
            NotifyEvent::SmartWorsened { group, band_index, reallocated_delta } => {
                format!(
                    "shr-rs: group `{group}` band {band_index} SMART reallocated sectors rose by {reallocated_delta}"
                )
            }
            NotifyEvent::ArrayMissing { group, band_index } => {
                format!(
                    "shr-rs: group `{group}` band {band_index}: no live mdadm array (expected but not assembled)"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NotifyEvent;

    #[test]
    fn scrub_errors_found_json_is_well_formed_even_with_a_quote_in_the_group_name() {
        let event =
            NotifyEvent::ScrubErrorsFound { group: "shr\"1".to_string(), band_index: 0, error_count: 3 };
        let parsed: serde_json::Value = serde_json::from_str(&event.to_json()).unwrap();
        assert_eq!(parsed["kind"], "scrub_errors_found");
        assert_eq!(parsed["group"], "shr\"1");
        assert_eq!(parsed["error_count"], 3);
    }

    #[test]
    fn status_lines_are_distinct_and_mention_the_group_and_band() {
        let a = NotifyEvent::Degraded { group: "shr1".to_string(), band_index: 2 };
        let b = NotifyEvent::SmartWorsened { group: "shr1".to_string(), band_index: 2, reallocated_delta: 5 };
        assert_ne!(a.status_line(), b.status_line());
        assert!(a.status_line().contains("shr1"));
        assert!(a.status_line().contains('2'));
    }
}
