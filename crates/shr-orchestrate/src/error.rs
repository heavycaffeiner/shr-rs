use shr_exec::ExecError;
use shr_state::StateError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrchestrateError {
    #[error("Execution error: {0}")]
    Exec(#[from] ExecError),

    #[error("State error: {0}")]
    State(#[from] StateError),

    #[error("Planner error: {0}")]
    Planner(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("{source}; automatic rollback was incomplete: {failures:?}")]
    Rollback {
        #[source]
        source: Box<OrchestrateError>,
        failures: Vec<String>,
    },

    // This precondition is shared by SEVEN operations (destroy,
    // expand, recompress, replace_disk, scrub_cancel, scrub_start,
    // scrub_status), not just "expand or resume" -- name the actual state
    // (no group recorded in state.toml), not any one operation, so the
    // text stays true regardless of which of the seven callers hit it.
    #[error("no storage group exists on this host yet")]
    NoActiveArray,

    /// A `ConfirmSink` answered `Reject` for a `create`/`expand` request.
    /// Always returned BEFORE any destructive command is issued -- see
    /// the design Stage 0 requirement 2 and the
    /// `confirm_reject_*` tests in `tests/sink_wiring.rs`.
    #[error("operation rejected: {0}")]
    Rejected(String),
}
