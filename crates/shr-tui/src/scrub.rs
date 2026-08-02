//! The TUI's scrub start/cancel controls. Before this, the TUI's
//! only operator action was the `a` key (`wizard::AddDiskController`) --
//! `scrub_start`/`scrub_cancel` (`shr_orchestrate::OrchestrationEngine`)
//! were reachable from `shr-cli` and Cockpit but not from here, forcing a
//! TUI operator to drop to a shell to start or stop a scrub. This module
//! closes that gap.
//!
//! Mirrors `wizard::AddDiskController`'s shape and its file-header
//! constraints, adapted to scrub's much shorter action:
//!   1. No execution without an explicit request -- `confirm_start`/
//!      `confirm_cancel` require `can_confirm_start`/`can_confirm_cancel`,
//!      only reachable after `request_start`/`request_cancel`.
//!   2. This module holds NO refusal logic of its own. `scrub_start`
//!      already blocks on a degraded band or an expansion in
//!      progress / other background activity; `scrub_cancel`'s
//!      mdadm/Btrfs read-back tolerance lives entirely in
//!      `OrchestrationEngine`. `confirm_start`/`confirm_cancel` forward
//!      whatever the engine returns verbatim into `Step::Done`/
//!      `Step::Error` -- never re-implemented, never second-guessed, never
//!      swallowed.
//!   3. Cancel additionally requires the operator to type the target
//!      group's exact name (`can_confirm_cancel`), the same discipline as
//!      `AddDiskController::can_execute` -- aborting an in-flight scrub is
//!      not a bare-keypress action. Start only requires having reached
//!      `Step::ConfirmStart` via the explicit `request_start` call; it is
//!      the less dangerous of the two (nothing in-flight is aborted), but
//!      still shows the target group and still needs a distinct
//!      confirmation step, not a single raw keypress that immediately acts.
//!   4. A failed `confirm_start`/`confirm_cancel` transitions to
//!      `Step::Error`, never `Step::Done`.

use std::sync::Arc;

use shr_exec::CommandRunner;
use shr_orchestrate::OrchestrationEngine;
use shr_state::StateStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubAction {
    Start,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Idle,
    /// Operator pressed the scrub-start key (`request_start`) -- an
    /// explicit, distinct step naming the target group; nothing has run
    /// yet. Reachable only by calling `request_start`.
    ConfirmStart,
    /// Operator pressed the scrub-cancel key (`request_cancel`) -- as
    /// above, plus `can_confirm_cancel` additionally requires the typed
    /// group name to match before `confirm_cancel` will act.
    ConfirmCancel,
    Done,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct ScrubState {
    pub step: Option<Step>,
    pub action: Option<ScrubAction>,
    /// The group `request_start`/`request_cancel` captured as the target,
    /// carried in `ScrubState` (not just the controller) so a UI layer that
    /// only ever sees state snapshots -- the same split `wizard::
    /// WizardState`/`AddDiskController` use -- can still render "which
    /// group" without reaching into the controller itself.
    pub target_group: Option<String>,
    /// Typed by the operator during `Step::ConfirmCancel`; must equal
    /// `target_group` for `can_confirm_cancel` to allow `confirm_cancel`.
    /// Unused (and irrelevant) during `Step::ConfirmStart`.
    pub confirmation_text: String,
    pub error_message: Option<String>,
}

impl ScrubState {
    pub fn step(&self) -> Step {
        self.step.unwrap_or(Step::Idle)
    }
}

/// Drives one scrub start-or-cancel request for a single target group.
/// Holds no reference to the terminal or to a `CommandRunner` -- tests (and
/// `runtime.rs`) pass the runner only at the moment of `confirm_start`/
/// `confirm_cancel`, the same split `AddDiskController::execute` uses so a
/// long-running call can be handed to a background thread without the
/// controller itself needing a `'static` runner reference.
pub struct ScrubController {
    store: Arc<StateStore>,
    group_name: Option<String>,
    pub state: ScrubState,
}

impl ScrubController {
    pub fn new(store: Arc<StateStore>, group_name: Option<String>) -> Self {
        Self { store, group_name, state: ScrubState::default() }
    }

    pub fn group_name(&self) -> Option<&str> {
        self.group_name.as_deref()
    }

    /// Idle -> ConfirmStart. The operator's explicit request to start a
    /// scrub -- opens a distinct confirm step naming the target group;
    /// calls nothing yet.
    pub fn request_start(&mut self) {
        self.state = ScrubState {
            step: Some(Step::ConfirmStart),
            action: Some(ScrubAction::Start),
            target_group: self.group_name.clone(),
            ..ScrubState::default()
        };
    }

    /// Idle -> ConfirmCancel. The operator's explicit request to cancel a
    /// scrub -- opens a distinct confirm step; `confirm_cancel` will still
    /// refuse until `set_confirmation_text` matches the target group's
    /// exact name (`can_confirm_cancel`).
    pub fn request_cancel(&mut self) {
        self.state = ScrubState {
            step: Some(Step::ConfirmCancel),
            action: Some(ScrubAction::Cancel),
            target_group: self.group_name.clone(),
            ..ScrubState::default()
        };
    }

    /// Back out of a pending confirm step (or clear a shown Done/Error
    /// result) without acting -- e.g. the operator's Esc.
    pub fn reset(&mut self) {
        self.state = ScrubState::default();
    }

    pub fn set_confirmation_text(&mut self, text: impl Into<String>) {
        self.state.confirmation_text = text.into();
    }

    /// Constraint 1 + the "still explicit" half of constraint 3: reachable
    /// only from `Step::ConfirmStart`, which only `request_start` sets --
    /// no typed name required (start is not destructive to anything
    /// in-flight the way cancel is).
    pub fn can_confirm_start(&self) -> bool {
        self.state.step() == Step::ConfirmStart
    }

    /// Constraint 1 + 3 combined: reachable only from `Step::ConfirmCancel`
    /// AND with a typed confirmation matching the target group's exact
    /// name -- mirrors `AddDiskController::can_execute` exactly.
    pub fn can_confirm_cancel(&self) -> bool {
        let Some(name) = &self.group_name else { return false };
        self.state.step() == Step::ConfirmCancel
            && !self.state.confirmation_text.is_empty()
            && self.state.confirmation_text == *name
    }

    /// ConfirmStart -> Done/Error: the real call. `runner` is expected to
    /// be a real `SystemRunner` in production; tests pass a fake. Holds no
    /// refusal logic of its own (module header, constraint 2) -- forwards
    /// `OrchestrationEngine::scrub_start`'s `Ok`/`Err` verbatim.
    pub fn confirm_start(&mut self, runner: &dyn CommandRunner) -> bool {
        if !self.can_confirm_start() {
            return false;
        }
        let engine = OrchestrationEngine::new(runner, self.store.clone());
        match engine.scrub_start(self.group_name.as_deref()) {
            Ok(()) => self.state.step = Some(Step::Done),
            Err(e) => {
                self.state.step = Some(Step::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
        true
    }

    /// ConfirmCancel -> Done/Error: the real call, gated on
    /// `can_confirm_cancel`'s typed-name match. Holds no refusal logic of
    /// its own (module header, constraint 2) -- forwards
    /// `OrchestrationEngine::scrub_cancel`'s `Ok`/`Err` verbatim, including
    /// its read-back-tolerant failure text when it does fail.
    pub fn confirm_cancel(&mut self, runner: &dyn CommandRunner) -> bool {
        if !self.can_confirm_cancel() {
            return false;
        }
        let engine = OrchestrationEngine::new(runner, self.store.clone());
        match engine.scrub_cancel(self.group_name.as_deref()) {
            Ok(()) => self.state.step = Some(Step::Done),
            Err(e) => {
                self.state.step = Some(Step::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
        true
    }
}
