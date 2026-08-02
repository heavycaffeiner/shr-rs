//! Read-only terminal frontend for the shared SHR-RS status report.

mod app;
mod refresh;
mod runtime;
pub mod scrub;
mod ui;
pub mod wizard;

pub use app::{App, DiskCandidate, Snapshot, Tab, WizardAction, WizardView};
pub use refresh::RefreshWorker;
pub use runtime::run;
pub use ui::{array_needs_attention, render};
