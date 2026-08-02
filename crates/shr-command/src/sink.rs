//! Progress reporting and destructive-action confirmation -- the foundation
//! every long-running operation (`create`/`expand` today; scrub and reshape
//! throttle in later Phase 5 stages) reports through, instead of each
//! feature inventing its own ad-hoc progress/confirm mechanism. See
//! the design Stage 0.
//!
//! Kept in `shr-command`, not `shr-orchestrate` or a frontend crate, for the
//! same reason `build_status` takes `&dyn Inspector`: business logic must
//! stay testable without a real terminal, a real Cockpit websocket, or a
//! real human -- these traits are the seam. Neither trait does I/O itself;
//! `TextProgressSink` below writes to whatever `io::Write` the caller hands
//! it (a real stdout handle in `shr-cli`, an in-memory buffer in tests)
//! rather than reaching for stdout directly, matching this crate's "never
//! touch a filesystem/terminal itself" rule.

use std::io::Write;
use std::sync::Mutex;

use serde::Serialize;

/// One update from a long-running operation: which stage it's in, how far
/// along (if known), and a human-readable message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProgressUpdate {
    /// Which long-running operation this update belongs to (e.g. `"create"`,
    /// `"expand"`, `"scrub"`). Lets a consumer following several operations
    /// at once (Cockpit driving a create while a scheduled scrub also
    /// reports, once Stage C exists) tell them apart without guessing from
    /// `stage`/`message` text.
    pub operation: String,
    /// A short, stable, machine-friendly phase name (e.g. `"partition"`,
    /// `"mdadm"`, `"lvm"`, `"filesystem"`) -- stable across releases so a
    /// long-lived Cockpit session or a log grep isn't broken by wording
    /// changes to `message`.
    pub stage: String,
    /// 0.0..=100.0 when known; `None` when the underlying step has no
    /// meaningful fractional progress (e.g. "running mkfs.btrfs", which
    /// either is done or isn't).
    pub percent: Option<f64>,
    pub message: String,
}

/// Receives [`ProgressUpdate`]s from a long-running operation. Implementors
/// decide what to do with them (print, serialize, record) -- the operation
/// itself never knows or cares which.
pub trait ProgressSink {
    fn report(&self, update: ProgressUpdate);
}

/// Default when a caller doesn't wire up a real sink: does nothing. This is
/// what every pre-Stage-5 call site (and every existing orchestrate test)
/// gets implicitly, so `create`/`expand`/`reconcile`'s behavior is
/// bit-for-bit unchanged when no sink is supplied (Stage 0 DoD: with no
/// sink, or a no-op one, behavior must be exactly what it is today).
pub struct NullProgressSink;

impl ProgressSink for NullProgressSink {
    fn report(&self, _update: ProgressUpdate) {}
}

/// Human-readable text, one line per update, written to any `io::Write` as
/// it arrives. `shr-cli` uses this for `create`/`expand`: those run
/// for hours, so their updates have to reach the operator *while* the work
/// happens. `reconcile`/`disk replace` buffer through
/// `RecordingProgressSink` instead — their updates are end-of-run
/// summaries, so printing after the call returns loses nothing. Guarded by
/// a `Mutex`, not a `RefCell`, so a sink instance can be shared across
/// threads.
pub struct TextProgressSink<W> {
    writer: Mutex<W>,
}

impl<W: Write> TextProgressSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer: Mutex::new(writer) }
    }
}

impl<W> TextProgressSink<W> {
    /// Unwrap back to the underlying writer -- mainly so tests can inspect
    /// what was written after the sink is done being used.
    pub fn into_inner(self) -> W {
        self.writer.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write> ProgressSink for TextProgressSink<W> {
    fn report(&self, update: ProgressUpdate) {
        let mut w = match self.writer.lock() {
            Ok(w) => w,
            Err(e) => e.into_inner(),
        };
        let _ = match update.percent {
            Some(p) => writeln!(
                w,
                "[{}] {} ({p:.0}%): {}",
                update.operation, update.stage, update.message
            ),
            None => writeln!(w, "[{}] {}: {}", update.operation, update.stage, update.message),
        };
    }
}

/// Test double: records every update it receives instead of writing
/// anywhere, so a test can assert on exactly what an operation reported
/// (which stages fired, in what order) without parsing text/JSON output.
#[derive(Default)]
pub struct RecordingProgressSink {
    updates: Mutex<Vec<ProgressUpdate>>,
}

impl RecordingProgressSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn updates(&self) -> Vec<ProgressUpdate> {
        self.updates.lock().unwrap().clone()
    }
}

impl ProgressSink for RecordingProgressSink {
    fn report(&self, update: ProgressUpdate) {
        self.updates.lock().unwrap().push(update);
    }
}

/// A caller's answer to a [`ConfirmSink::confirm`] request. A dedicated
/// enum, not a bare `bool` -- review here has repeatedly caught "reports
/// success for work it never actually did" bugs, and
/// `if confirmed { .. }` reads identically whether `confirmed`
/// means "user said proceed" or "user said cancel". A reversed sense at one
/// call site would silently let a rejected destructive action through.
/// Matching on `Confirmation::Proceed`/`Confirmation::Reject` can't be
/// misread either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    Proceed,
    Reject,
}

/// What's about to happen, for a [`ConfirmSink`] to show/ask about.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmRequest {
    /// Which operation is asking (e.g. `"create"`, `"expand"`) -- same key
    /// space as `ProgressUpdate::operation`.
    pub operation: String,
    /// Human-readable description of what will happen (which disks, which
    /// group) -- built by the caller, since only the caller (the
    /// orchestration engine) knows the concrete request.
    pub summary: String,
    /// True when the action, once started, cannot be cleanly undone (both
    /// `create` and `expand` are: each has a "point of no return" past which
    /// this project's own rollback logic gives up and tells the operator to
    /// inspect the array by hand instead). Every call site sets this `true`
    /// today (no reversible operation goes through `ConfirmSink` yet);
    /// `shr-orchestrate`'s tests assert on it directly.
    pub irreversible: bool,
}

/// Asks whether a destructive action should proceed. A `Reject` answer MUST
/// stop the action before it touches anything real -- see the
/// `shr-orchestrate` engine tests that prove a rejected `create`/`expand`
/// issues zero commands against the `CommandRunner`, not just that it
/// returns an error.
pub trait ConfirmSink {
    fn confirm(&self, request: &ConfirmRequest) -> Confirmation;
}

/// Always answers `Proceed` -- the ONLY sink that reproduces "no
/// confirmation ever happens", which is what every call site got before
/// Stage 0 (no `ConfirmSink` existed at all) and MUST keep getting when a
/// caller doesn't opt into one (Stage 0 DoD: unchanged default behavior).
///
/// Do not reach for this as a generic "skip confirmation" shortcut in a NEW
/// call site that runs unattended -- see [`AlwaysRejectConfirmSink`] for
/// what that should use instead. This type exists specifically to be the
/// engine's default, preserving history, not to be a general-purpose bypass.
pub struct AlwaysConfirmSink;

impl ConfirmSink for AlwaysConfirmSink {
    fn confirm(&self, _request: &ConfirmRequest) -> Confirmation {
        Confirmation::Proceed
    }
}

/// Fails closed: always answers `Reject`. This is the sink a non-interactive
/// caller (the daemon loop, a Cockpit-spawned process with no confirm
/// round-trip wired up, anything running unattended) should use for
/// `ConfirmSink` -- "there is no one to ask" must never silently become
/// "yes" (Stage 0 requirement: non-interactive default must not be a quiet
/// approval). A real interactive sink (a TTY prompt, a Cockpit dialog
/// round-trip) is what Stage A adds; until a call site has one of those
/// wired up, this is the correct choice for anything unattended.
pub struct AlwaysRejectConfirmSink;

impl ConfirmSink for AlwaysRejectConfirmSink {
    fn confirm(&self, _request: &ConfirmRequest) -> Confirmation {
        Confirmation::Reject
    }
}

/// Test double: returns a fixed, caller-chosen answer and records every
/// request it was asked about, so a test can assert both "what got asked"
/// and "what happened when the answer was `Reject`".
pub struct RecordingConfirmSink {
    answer: Confirmation,
    requests: Mutex<Vec<ConfirmRequest>>,
}

impl RecordingConfirmSink {
    pub fn proceeding() -> Self {
        Self { answer: Confirmation::Proceed, requests: Mutex::new(Vec::new()) }
    }

    pub fn rejecting() -> Self {
        Self { answer: Confirmation::Reject, requests: Mutex::new(Vec::new()) }
    }

    pub fn requests(&self) -> Vec<ConfirmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ConfirmSink for RecordingConfirmSink {
    fn confirm(&self, request: &ConfirmRequest) -> Confirmation {
        self.requests.lock().unwrap().push(request.clone());
        self.answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(stage: &str, percent: Option<f64>) -> ProgressUpdate {
        ProgressUpdate {
            operation: "create".to_string(),
            stage: stage.to_string(),
            percent,
            message: format!("{stage} in progress"),
        }
    }

    #[test]
    fn null_progress_sink_does_nothing_and_never_panics() {
        let sink = NullProgressSink;
        sink.report(update("partition", Some(10.0)));
        sink.report(update("done", None));
    }

    #[test]
    fn text_progress_sink_writes_one_line_per_update_with_percent() {
        let sink = TextProgressSink::new(Vec::new());
        sink.report(update("partition", Some(50.0)));
        let out = String::from_utf8(sink.into_inner()).unwrap();
        assert_eq!(out, "[create] partition (50%): partition in progress\n");
    }

    #[test]
    fn text_progress_sink_omits_percent_when_unknown() {
        let sink = TextProgressSink::new(Vec::new());
        sink.report(update("filesystem", None));
        let out = String::from_utf8(sink.into_inner()).unwrap();
        assert_eq!(out, "[create] filesystem: filesystem in progress\n");
    }

    #[test]
    fn recording_progress_sink_captures_updates_in_order() {
        let sink = RecordingProgressSink::new();
        sink.report(update("partition", Some(0.0)));
        sink.report(update("array", Some(50.0)));
        sink.report(update("done", Some(100.0)));
        let stages: Vec<String> = sink.updates().into_iter().map(|u| u.stage).collect();
        assert_eq!(stages, vec!["partition", "array", "done"]);
    }

    fn request(irreversible: bool) -> ConfirmRequest {
        ConfirmRequest {
            operation: "create".to_string(),
            summary: "create group `default` (3 disks)".to_string(),
            irreversible,
        }
    }

    #[test]
    fn always_confirm_sink_always_proceeds() {
        let sink = AlwaysConfirmSink;
        assert_eq!(sink.confirm(&request(true)), Confirmation::Proceed);
        assert_eq!(sink.confirm(&request(false)), Confirmation::Proceed);
    }

    #[test]
    fn always_reject_confirm_sink_always_rejects() {
        let sink = AlwaysRejectConfirmSink;
        assert_eq!(sink.confirm(&request(true)), Confirmation::Reject);
    }

    #[test]
    fn recording_confirm_sink_proceeding_returns_proceed_and_records_the_request() {
        let sink = RecordingConfirmSink::proceeding();
        let req = request(true);
        assert_eq!(sink.confirm(&req), Confirmation::Proceed);
        assert_eq!(sink.requests(), vec![req]);
    }

    #[test]
    fn recording_confirm_sink_rejecting_returns_reject_and_records_the_request() {
        let sink = RecordingConfirmSink::rejecting();
        let req = request(true);
        assert_eq!(sink.confirm(&req), Confirmation::Reject);
        assert_eq!(sink.requests(), vec![req]);
    }
}
