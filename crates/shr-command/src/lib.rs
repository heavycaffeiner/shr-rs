//! `shr-command` — the Command API both the CLI and the TUI call. Business
//! logic lives here (and below, in shr-core/shr-inspect), never in a frontend.
//!
//! For the read-only vertical slice this exposes two operations, both of which
//! produce serializable reports (the JSON contract the Cockpit plugin consumes)
//! and have ASCII renderers:
//! - [`build_status`] — inspect disks + mdadm arrays.
//! - [`build_plan_report`] — dry-run `plan_initial` for a proposed disk set.
//!
//! It also exposes write preflight helpers, run before any executor in
//! `shr-exec` touches a device:
//! - [`preflight_create`] — stable-id + OS-disk gates for a proposed set.

pub mod ops;
pub mod render;
pub mod report;
pub mod sink;
pub mod ui_mode;

use thiserror::Error;

pub use ops::{
    build_fs_df, build_plan_report, build_status, preflight_create, system_disk_aliases,
};
pub use report::{
    ArrayStatus, BandReport, DiskStatus, FsDfReport, FsUsageInput, GroupBandStatus, GroupDfStatus,
    GroupStatus, Health, MetricsReport, PlanReport, ScrubOutcome, ScrubSummary, SmartState,
    SmartSummary, StatusReport, SyncSummary,
};
pub use sink::{
    AlwaysConfirmSink, AlwaysRejectConfirmSink, Confirmation, ConfirmRequest, ConfirmSink,
    NullProgressSink, ProgressSink, ProgressUpdate, RecordingConfirmSink, RecordingProgressSink,
    TextProgressSink,
};
pub use ui_mode::{can_prompt_operator, detect_ui_mode, is_interactive_terminal, UiMode};

#[derive(Debug, Error)]
pub enum ShrError {
    #[error(transparent)]
    Inspect(#[from] shr_inspect::InspectError),
    #[error(transparent)]
    Plan(#[from] shr_core::PlanError),
}
