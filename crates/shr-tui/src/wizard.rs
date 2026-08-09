//! The TUI's "Add Disk" workflow -- expand an existing
//! SHR group by adding one or more disks, from inside the terminal. Built on
//! the exact same Command API the CLI and Cockpit use
//! (`shr_command::preflight_create`, `shr_orchestrate::preview_expand`,
//! `OrchestrationEngine::expand`) -- this module holds no preflight/safety
//! logic of its own, only the step sequencing, mirroring
//! `cockpit/src/createGroup.ts`'s controller shape so the two frontends stay
//! provably in lockstep with the same design constraints (the design
//! Stage A):
//!   1. No execution without preview -- `execute()` requires `step ==
//!      Confirm`, only reachable after `run_preview()` succeeded.
//!
//!   (Constraints 2 and 3 have no direct analogue here: `superuser`/
//!   `--force-content` auto-append are Cockpit-specific spawn concerns. The
//!   TUI runs in-process as whatever user started it (already root for real
//!   hardware access), and `force_content` is still never assumed -- it's
//!   an explicit constructor argument the caller (the UI layer) must set
//!   from an explicit user action.)
//!
//!   4. `run_preflight`/`run_preview` never recompute "is this safe" --
//!      they store and forward exactly what `preflight_create`/
//!      `preview_expand` returned.
//!   5. `execute()` requires the operator to have typed the target group's
//!      exact name via `set_confirmation_text` -- see `can_execute`.
//!   6. A failed `run_preflight`/`run_preview`/`execute` transitions to
//!      `Step::Error`, never `Step::Done`.
//!   7. `run_preview`/`execute` never bypass `expand()`'s
//!      pre-reshape scrub-freshness check by default -- `skip_scrub_check`
//!      starts `false`, exactly like `shr-cli`'s `--skip-scrub-check` flag,
//!      and only `set_skip_scrub_check(true)` (an explicit operator action
//!      taken after seeing `Step::ScrubCheckWarning`) turns it on. The TUI
//!      must stay a superset of what the CLI allows, never a more
//!      permissive one in the dangerous direction.

use std::path::PathBuf;
use std::sync::Arc;

use shr_command::{preflight_create, AlwaysConfirmSink};
use shr_exec::{CommandRunner, DryRunRunner};
use shr_inspect::{resolve_disk_refs, DiskRef, Inspector, ResolvedDisk, WritePreflight};
use shr_orchestrate::{preview_expand, ExpandRequest, OrchestrateError, OrchestrationEngine};
use shr_state::{ArrayState, StateStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    SelectDisks,
    Preflight,
    Preview,
    /// `run_preview()` hit `expand()`'s pre-reshape scrub-freshness
    /// check and the operator has not yet explicitly overridden it -- the
    /// TUI equivalent of `shr-cli`'s `--skip-scrub-check` not having been
    /// passed. Distinct from `Error`: this is a recoverable safety gate the
    /// operator can consciously bypass (`AddDiskController::
    /// set_skip_scrub_check` + re-running `run_preview()`), not a failure.
    ScrubCheckWarning,
    Confirm,
    Executing,
    Done,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct WizardState {
    pub step: Option<Step>,
    pub preflight: Option<WritePreflight>,
    pub preview_state: Option<ArrayState>,
    pub preview_commands: Vec<String>,
    pub confirmation_text: String,
    pub result: Option<ArrayState>,
    pub error_message: Option<String>,
    /// The warning text `expand()` returned when blocked on the
    /// pre-reshape scrub-freshness check -- set only on `Step::
    /// ScrubCheckWarning`, so the terminal UI can show it and ask the
    /// operator to explicitly opt in before anything proceeds.
    pub scrub_check_warning: Option<String>,
}

impl WizardState {
    pub fn step(&self) -> Step {
        self.step.unwrap_or(Step::SelectDisks)
    }
}

/// Drives one Add Disk run through preflight -> preview -> confirm ->
/// execute for a single target group. Holds no reference to the terminal;
/// tests drive it directly against a `StaticInspector` + tempdir
/// `StateStore`, the same fixtures `shr-orchestrate`'s own test suite uses.
pub struct AddDiskController<'a> {
    inspector: &'a dyn Inspector,
    store: Arc<StateStore>,
    mdadm_conf: PathBuf,
    fstab: PathBuf,
    system_disks: Vec<String>,
    group_name: Option<String>,
    selected_kernel_names: Vec<String>,
    force_content: bool,
    /// The operator's explicit, deliberate override of the
    /// pre-reshape scrub-freshness check -- the TUI equivalent of
    /// `shr-cli --skip-scrub-check`. Starts `false` (same default as the
    /// CLI's flag); only `set_skip_scrub_check(true)` can flip it, and only
    /// a caller who read `Step::ScrubCheckWarning`'s message and chose to
    /// proceed anyway is expected to call that.
    skip_scrub_check: bool,
    pub state: WizardState,
}

impl<'a> AddDiskController<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inspector: &'a dyn Inspector,
        store: Arc<StateStore>,
        mdadm_conf: impl Into<PathBuf>,
        fstab: impl Into<PathBuf>,
        system_disks: Vec<String>,
        group_name: Option<String>,
        selected_kernel_names: Vec<String>,
        force_content: bool,
    ) -> Self {
        Self {
            inspector,
            store,
            mdadm_conf: mdadm_conf.into(),
            fstab: fstab.into(),
            system_disks,
            group_name,
            selected_kernel_names,
            force_content,
            skip_scrub_check: false,
            state: WizardState::default(),
        }
    }

    /// The operator's explicit action to bypass the pre-reshape
    /// scrub-freshness check, after having actually seen its warning on
    /// `Step::ScrubCheckWarning` -- mirrors `shr-cli`'s `--skip-scrub-check`
    /// flag exactly, including staying off unless a caller deliberately
    /// turns it on. Takes effect on the NEXT `run_preview()` (and, once
    /// preview succeeds with it set, the following `execute()` too, since
    /// both build their `ExpandRequest` from this same field).
    pub fn set_skip_scrub_check(&mut self, skip: bool) {
        self.skip_scrub_check = skip;
    }

    fn resolve_selected(&self) -> Result<Vec<shr_inspect::ResolvedDisk>, OrchestrateError> {
        let lsblk = self.inspector.block_devices().map_err(to_orchestrate_error)?;
        let by_id = self.inspector.by_id_index().map_err(to_orchestrate_error)?;
        let refs: Vec<DiskRef> = self
            .selected_kernel_names
            .iter()
            .map(|n| DiskRef::Path(n.clone()))
            .collect();
        resolve_disk_refs(&refs, &lsblk, &by_id).map_err(|e| OrchestrateError::Validation(e.to_string()))
    }

    /// Step 1 -> 2. Constraint 4: trusts `WritePreflight.ok` verbatim.
    pub fn run_preflight(&mut self) {
        match preflight_create(self.inspector, &self.selected_kernel_names, self.force_content) {
            Ok(report) => {
                self.state.step = Some(if report.ok { Step::Preview } else { Step::Preflight });
                self.state.preflight = Some(report);
            }
            Err(e) => {
                self.state.step = Some(Step::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
    }

    /// Step 2 -> 3 (constraint 1). Only meaningful once `run_preflight`
    /// reported `ok: true`; a blocked preflight leaves the wizard on
    /// `Step::Preflight` with nothing here to call.
    ///
    /// `skip_scrub_check` is `self.skip_scrub_check`, NOT a hardcoded
    /// `true` -- the previous version bypassed `expand()`'s pre-reshape
    /// scrub check unconditionally and silently, making the TUI a MORE
    /// permissive surface than `shr-cli` (which enforces the check unless
    /// `--skip-scrub-check` is passed) in the dangerous direction. If
    /// `expand()` blocks on exactly that check, this lands on
    /// `Step::ScrubCheckWarning` (not `Step::Error`) with the warning text
    /// preserved for the UI to show; a caller must explicitly call
    /// `set_skip_scrub_check(true)` and call this again to proceed.
    pub fn run_preview(&mut self) {
        let outcome = self.resolve_selected().and_then(|new_disks| {
            let req = ExpandRequest {
                name: self.group_name.clone(),
                new_disks,
                system_disks: self.system_disks.clone(),
                skip_scrub_check: self.skip_scrub_check,
            };
            preview_expand(self.store.clone(), req)
        });
        match outcome {
            Ok((state, commands)) => {
                self.state.step = Some(Step::Confirm);
                self.state.preview_state = Some(state);
                self.state.preview_commands = commands;
                self.state.scrub_check_warning = None;
            }
            Err(e) if !self.skip_scrub_check && is_scrub_check_warning(&e) => {
                self.state.step = Some(Step::ScrubCheckWarning);
                self.state.scrub_check_warning = Some(e.to_string());
            }
            Err(e) => {
                self.state.step = Some(Step::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
    }

    pub fn set_confirmation_text(&mut self, text: impl Into<String>) {
        self.state.confirmation_text = text.into();
    }

    /// Constraint 1 + 5 combined: a completed preview AND a confirmation
    /// text matching the target group's exact name are both required.
    pub fn can_execute(&self) -> bool {
        let Some(name) = &self.group_name else {
            return false;
        };
        self.state.step() == Step::Confirm
            && self.state.preview_state.is_some()
            && !self.state.confirmation_text.is_empty()
            && self.state.confirmation_text == *name
    }

    /// Step 3 -> 4: the real, irreversible call. `runner` is expected to be
    /// a real `SystemRunner` in production; tests pass a fake. Explicitly
    /// wires `AlwaysConfirmSink` -- the operator's typed confirmation
    /// (`can_execute`) already IS the approval; this is the TUI's own
    /// explicit opt-in, exactly mirroring `shr-cli`'s identical call site.
    pub fn execute(&mut self, runner: &dyn CommandRunner) -> bool {
        if !self.can_execute() {
            return false;
        }
        self.state.step = Some(Step::Executing);
        let outcome = self.resolve_selected().and_then(|new_disks| {
            let req = ExpandRequest {
                name: self.group_name.clone(),
                new_disks,
                system_disks: self.system_disks.clone(),
                // Same field `run_preview()` used to reach `Step::
                // Confirm` in the first place -- never a hardcoded bypass.
                // `can_execute()` only allows this call from `Step::Confirm`,
                // which is only reachable with a scrub-freshness outcome
                // this exact flag value already accounts for (either the
                // check genuinely passed, or the operator explicitly
                // overrode it via `set_skip_scrub_check`).
                skip_scrub_check: self.skip_scrub_check,
            };
            let engine = OrchestrationEngine::new(runner, self.store.clone())
                .with_conf_paths(&self.mdadm_conf, &self.fstab)
                .with_confirm_sink(&AlwaysConfirmSink);
            engine.expand(req)
        });
        match outcome {
            Ok(state) => {
                self.state.step = Some(Step::Done);
                self.state.result = Some(state);
            }
            Err(e) => {
                // Constraint 6: a failed expand lands on `Step::Error`,
                // never `Step::Done` -- the two branches above are mutually
                // exclusive, so there is no path that shows success for a
                // call that actually failed.
                self.state.step = Some(Step::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
        true
    }
}

/// TUI equivalent of `shr-cli`'s `disk replace` (`OrchestrationEngine::
/// replace_disk`) -- lets an operator swap out a failed/failing member
/// disk without dropping to a shell. Same two-frontend-lockstep goal as
/// `AddDiskController`'s file header, but `replace_disk` itself has no
/// `preflight_*`/`preview_*` pair in `shr-orchestrate` yet (`shr-cli`'s own
/// `Disk::Replace` handler documents this as a Stage C gap: "no --dry-run
/// preview yet"). So `run_preview` below builds the preview itself, the
/// exact same way `shr_orchestrate::preview_expand_against` does: call the
/// real `replace_disk` through an in-process `DryRunRunner` and capture
/// what it *would* have run. This module still holds no safety logic of
/// its own -- it only decides WHEN to call the real, unmodified
/// `replace_disk`, never re-implements what it checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceStep {
    /// Waiting for `select()` (the old member to retire + the new disk to
    /// take its place). Also the step a failed `run_preview()` call cannot
    /// leave from without a fresh `select()` -- there is no separate
    /// "ready to preview" step like `AddDiskController`'s `Preflight`/
    /// `Preview` split, since replace has no preflight stage to reach it
    /// through.
    Select,
    Confirm,
    Executing,
    Done,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct ReplaceWizardState {
    pub step: Option<ReplaceStep>,
    pub preview_state: Option<ArrayState>,
    pub preview_commands: Vec<String>,
    pub confirmation_text: String,
    pub result: Option<ArrayState>,
    pub error_message: Option<String>,
}

impl ReplaceWizardState {
    pub fn step(&self) -> ReplaceStep {
        self.step.unwrap_or(ReplaceStep::Select)
    }
}

/// Drives one Replace Disk run through select -> preview -> confirm ->
/// execute for a single target group. Same fixture/testing shape as
/// `AddDiskController`: no reference to the terminal, driven directly
/// against a `StaticInspector` + tempdir `StateStore` in tests.
pub struct ReplaceDiskController<'a> {
    inspector: &'a dyn Inspector,
    store: Arc<StateStore>,
    mdadm_conf: PathBuf,
    fstab: PathBuf,
    system_disks: Vec<String>,
    group_name: Option<String>,
    /// The existing group member (`StateDisk::id`, e.g. an `ata-...`
    /// by-id path -- NOT a kernel name like `sda`) being retired. Set only
    /// by `select()`.
    old_disk_id: Option<String>,
    /// The free disk's kernel name (e.g. `sdb`) taking `old_disk_id`'s
    /// place. Resolved to a `ResolvedDisk` fresh on every `run_preview`/
    /// `execute` call, same as `AddDiskController::resolve_selected` --
    /// never cached, so a stale resolution can't silently outlive a
    /// changed system.
    new_disk_kernel_name: Option<String>,
    pub state: ReplaceWizardState,
}

impl<'a> ReplaceDiskController<'a> {
    pub fn new(
        inspector: &'a dyn Inspector,
        store: Arc<StateStore>,
        mdadm_conf: impl Into<PathBuf>,
        fstab: impl Into<PathBuf>,
        system_disks: Vec<String>,
        group_name: Option<String>,
    ) -> Self {
        Self {
            inspector,
            store,
            mdadm_conf: mdadm_conf.into(),
            fstab: fstab.into(),
            system_disks,
            group_name,
            old_disk_id: None,
            new_disk_kernel_name: None,
            state: ReplaceWizardState::default(),
        }
    }

    /// Step 1: the operator's choice of which member to retire and which
    /// free disk replaces it. Recomputes nothing and validates nothing --
    /// same "no safety logic of its own" discipline as the rest of this
    /// module; `run_preview()` is what actually asks `replace_disk`.
    pub fn select(&mut self, old_disk_id: impl Into<String>, new_disk_kernel_name: impl Into<String>) {
        self.old_disk_id = Some(old_disk_id.into());
        self.new_disk_kernel_name = Some(new_disk_kernel_name.into());
    }

    fn resolve_new_disk(&self) -> Result<ResolvedDisk, OrchestrateError> {
        let kernel_name = self
            .new_disk_kernel_name
            .as_ref()
            .ok_or_else(|| OrchestrateError::Validation("no replacement disk selected".to_string()))?;
        let lsblk = self.inspector.block_devices().map_err(to_orchestrate_error)?;
        let by_id = self.inspector.by_id_index().map_err(to_orchestrate_error)?;
        let refs = vec![DiskRef::Path(kernel_name.clone())];
        let mut resolved = resolve_disk_refs(&refs, &lsblk, &by_id)
            .map_err(|e| OrchestrateError::Validation(e.to_string()))?;
        resolved
            .pop()
            .ok_or_else(|| OrchestrateError::Validation(format!("could not resolve `{kernel_name}`")))
    }

    /// Step 1 -> 2. Builds the preview by calling the real, unmodified
    /// `replace_disk` against an in-process `DryRunRunner`, then reports
    /// back exactly the commands it recorded -- same shape as
    /// `shr_orchestrate::preview_expand_against` (constraint 4: never
    /// recompute "is this safe").
    ///
    /// Deliberately does NOT call `.with_conf_paths(&self.mdadm_conf,
    /// &self.fstab)` here, unlike `execute()` below -- matching
    /// `preview_expand_against`, which also leaves
    /// `OrchestrationEngine::new`'s sandboxed default in place. To be
    /// precise about WHY, since a wrong reason here would be worse than
    /// none: what actually keeps this preview side-effect-free is
    /// `engine.rs`'s own `if !self.runner.is_dry_run()` guard around
    /// `store.save` + `write_managed_configs` -- under `DryRunRunner`
    /// neither runs, so even the real `/etc/...` paths would not be
    /// written. Omitting `with_conf_paths` is defence in depth against
    /// that guard being narrowed later, not the thing preventing a write
    /// today.
    ///
    /// Known limitation (not fixed here -- out of this file's ownership):
    /// `replace_disk`'s live-status validation read (its degraded-band
    /// check, `MdadmExecutor::degraded_count`) short-circuits to `Ok(0)`
    /// under `is_dry_run()`, i.e. this preview always sees "nothing else
    /// is degraded" regardless of the real system's state. Unlike
    /// `expand()`, `replace_disk` has no `status_runner`-equivalent
    /// override to answer that read from the real system instead (a later
    /// fix added exactly that to `expand()`; `replace_disk` never got the same
    /// treatment). A block that only a live read would catch can therefore
    /// still surface for the first time at `execute()` -- which is exactly
    /// why constraint 6 (`execute()` failure lands on `Step::Error`, never
    /// `Step::Done`) is what actually protects the operator here, not this
    /// preview by itself.
    pub fn run_preview(&mut self) {
        let outcome = self.resolve_new_disk().and_then(|new_disk| {
            let old_id = self
                .old_disk_id
                .clone()
                .ok_or_else(|| OrchestrateError::Validation("no disk selected to replace".to_string()))?;
            let dry_runner = DryRunRunner::new();
            let engine = OrchestrationEngine::new(&dry_runner, self.store.clone());
            engine
                .replace_disk(self.group_name.as_deref(), &old_id, &new_disk, &self.system_disks)
                .map(|state| (state, dry_runner.get_recorded()))
        });
        match outcome {
            Ok((state, commands)) => {
                self.state.step = Some(ReplaceStep::Confirm);
                self.state.preview_state = Some(state);
                self.state.preview_commands = commands;
            }
            Err(e) => {
                self.state.step = Some(ReplaceStep::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
    }

    pub fn set_confirmation_text(&mut self, text: impl Into<String>) {
        self.state.confirmation_text = text.into();
    }

    /// Same constraint 1 + 5 combination as `AddDiskController::
    /// can_execute`: a completed preview AND a confirmation text matching
    /// the target group's exact name are both required. Refuses
    /// unconditionally with no `group_name` set, same as the expand
    /// wizard -- there is nothing for the operator's typed text to match.
    pub fn can_execute(&self) -> bool {
        let Some(name) = &self.group_name else {
            return false;
        };
        self.state.step() == ReplaceStep::Confirm
            && self.state.preview_state.is_some()
            && !self.state.confirmation_text.is_empty()
            && self.state.confirmation_text == *name
    }

    /// Step 3 -> 4: the real, irreversible call, same shape as
    /// `AddDiskController::execute` -- real `CommandRunner`, real
    /// `/etc/...` conf paths, `AlwaysConfirmSink` because the operator's
    /// typed confirmation (`can_execute`) already IS the approval.
    pub fn execute(&mut self, runner: &dyn CommandRunner) -> bool {
        if !self.can_execute() {
            return false;
        }
        self.state.step = Some(ReplaceStep::Executing);
        let outcome = self.resolve_new_disk().and_then(|new_disk| {
            // `can_execute()` requires `Step::Confirm`, only reachable via
            // a successful `run_preview()`, which itself requires
            // `old_disk_id` to be set -- so this is never actually reached
            // with `None` from a caller that respected `can_execute()`.
            let old_id = self
                .old_disk_id
                .clone()
                .ok_or_else(|| OrchestrateError::Validation("no disk selected to replace".to_string()))?;
            let engine = OrchestrationEngine::new(runner, self.store.clone())
                .with_conf_paths(&self.mdadm_conf, &self.fstab)
                .with_confirm_sink(&AlwaysConfirmSink);
            engine.replace_disk(self.group_name.as_deref(), &old_id, &new_disk, &self.system_disks)
        });
        match outcome {
            Ok(state) => {
                self.state.step = Some(ReplaceStep::Done);
                self.state.result = Some(state);
            }
            Err(e) => {
                // Constraint 6: a failed replace lands on `Step::Error`,
                // never `Step::Done`.
                self.state.step = Some(ReplaceStep::Error);
                self.state.error_message = Some(e.to_string());
            }
        }
        true
    }
}

fn to_orchestrate_error(e: shr_inspect::InspectError) -> OrchestrateError {
    OrchestrateError::Validation(e.to_string())
}

/// Whether `e` is specifically `expand()`'s pre-reshape scrub-freshness
/// block (`crates/shr-orchestrate/src/engine.rs`'s "run a scrub first, or
/// pass --skip-scrub-check to expand anyway" message), as opposed to some
/// other `Validation` error (degraded band, disk already in use, ...) that
/// `set_skip_scrub_check` would do nothing to fix. Matched by the same
/// `--skip-scrub-check` substring `shr-cli` tells the operator to pass --
/// the one part of that message this crate is allowed to depend on without
/// duplicating the actual freshness logic here (this module holds no
/// safety logic of its own, per the file header).
fn is_scrub_check_warning(e: &OrchestrateError) -> bool {
    matches!(e, OrchestrateError::Validation(msg) if msg.contains("--skip-scrub-check"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shr_exec::{CommandOutput, ExecError};
    use shr_inspect::StaticInspector;
    use shr_state::StateFile;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Three disks: sda/sdc already form the existing RAID1 band (mdadm RAID1
    // needs >=2 members -- a single-member "RAID1" is not a real array shape
    // shr-core's planner will accept), sdb is the new disk being added.
    const LSBLK_THREE_DISKS: &str = r#"{"blockdevices":[
        {"name":"sda","size":"4000000000000","type":"disk","model":"Existing","serial":"S0","rota":true,"tran":"sata","partuuid":null,"fstype":null,"mountpoint":null,"pttype":null},
        {"name":"sdb","size":"4000000000000","type":"disk","model":"New Disk","serial":"S1","rota":true,"tran":"sata","partuuid":null,"fstype":null,"mountpoint":null,"pttype":null},
        {"name":"sdc","size":"4000000000000","type":"disk","model":"Existing","serial":"S2","rota":true,"tran":"sata","partuuid":null,"fstype":null,"mountpoint":null,"pttype":null}
    ]}"#;

    fn inspector_with_new_disk() -> StaticInspector {
        let mut by_id = shr_inspect::ByIdIndex::empty();
        by_id.insert("sda", "ata-EXISTING1");
        by_id.insert("sdb", "ata-NEWDISK");
        by_id.insert("sdc", "ata-EXISTING2");
        StaticInspector::from_raw(LSBLK_THREE_DISKS, "", Default::default())
            .unwrap()
            .with_by_id(by_id)
    }

    /// A `CommandRunner` that never fails and records nothing interesting --
    /// enough to prove *whether* `execute()` reached the engine, not the
    /// engine's own execution correctness (that's `shr-orchestrate`'s job).
    struct RecordingRunner {
        commands: Mutex<Vec<String>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
            self.commands
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    /// Seed a real, planner-consistent group (band geometry computed by the
    /// actual `shr_core::plan_initial`, not hand-picked numbers) so
    /// `run_preview`'s `plan_expansion` re-validation against this state
    /// doesn't reject it as "band geometry would change" -- `expand()`
    /// recomputes a `LayoutSnapshot` from whatever is in `state.toml` and
    /// insists it matches what the planner would derive fresh.
    fn seed_group(store: &Arc<StateStore>, name: &str) {
        let core_disks = vec![
            shr_core::Disk::new("ata-EXISTING1", 4_000_000_000_000),
            shr_core::Disk::new("ata-EXISTING2", 4_000_000_000_000),
        ];
        let input = shr_core::PlannerInput::new(core_disks, shr_core::RedundancyMode::Shr);
        let reserved_head = input.reserved_head;
        let plan = shr_core::plan_initial(&input).unwrap();
        let band = &plan.bands[0];
        let part_uuids = [
            "11111111-1111-1111-1111-111111111111",
            "44444444-4444-4444-4444-444444444444",
        ];

        let disks = band
            .members()
            .iter()
            .zip(part_uuids)
            .map(|(disk_id, part_uuid)| shr_state::StateDisk {
                id: disk_id.as_str().to_string(),
                size_bytes: 4_000_000_000_000,
                serial: None,
                model: None,
                added_at: "2026-07-26T00:00:00Z".to_string(),
                partitions: vec![shr_state::StatePartition {
                    part_uuid: part_uuid.to_string(),
                    offset_bytes: reserved_head + band.offset(),
                    size_bytes: band.size(),
                    band_index: band.band_index(),
                }],
            })
            .collect();

        let level = match band.level() {
            shr_core::RaidLevel::Raid1 => "raid1",
            shr_core::RaidLevel::Raid5 => "raid5",
            shr_core::RaidLevel::Raid6 => "raid6",
        };
        let state = ArrayState {
            name: name.to_string(),
            mode: "shr".to_string(),
            created_at: "2026-07-26T00:00:00Z".to_string(),
            layout_version: 1,
            disks,
            bands: vec![shr_state::StateBand {
                index: band.band_index(),
                level: level.to_string(),
                md_name: "md0".to_string(),
                md_uuid: Some("a1b2c3d4:e5f6a7b8:c9d0e1f2:a3b4c5d6".to_string()),
                member_partitions: part_uuids.iter().map(|u| u.to_string()).collect(),
                usable_bytes: band.usable_bytes(),
                resize_pending: false,
                last_smart_reallocated: None,
                last_scrub: None,
                scrub_in_progress: false,
                pending_member_removal: None,
                reshape_priority: None,
            }],
            filesystem: shr_state::StateFilesystem {
                fs_uuid: Some("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d".to_string()),
                mount_point: "/mnt/shr_data".to_string(),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
            },
            expansion: shr_state::StateExpansion::default(),
        };
        store.save(&StateFile::new(vec![state])).unwrap();
    }

    fn controller(inspector: &StaticInspector, store: Arc<StateStore>) -> AddDiskController<'_> {
        AddDiskController::new(
            inspector,
            store,
            "mdadm.conf",
            "fstab",
            vec!["sda".to_string()],
            Some("shr1".to_string()),
            vec!["sdb".to_string()],
            false,
        )
    }

    #[test]
    fn execute_is_refused_before_any_preflight_or_preview_ran() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);
        let runner = RecordingRunner::new();

        assert!(!wizard.can_execute());
        assert!(!wizard.execute(&runner), "execute() must refuse and do nothing");
        assert!(wizard.state.result.is_none());
        assert!(
            runner.commands.lock().unwrap().is_empty(),
            "no engine call must have happened"
        );
    }

    #[test]
    fn execute_is_refused_when_confirmation_text_does_not_match_the_group_name() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);
        let runner = RecordingRunner::new();

        wizard.run_preflight();
        assert_eq!(wizard.state.step(), Step::Preview);
        // `seed_group` never recorded a scrub: this must not proceed
        // to `Confirm` without the operator explicitly overriding the gate.
        wizard.set_skip_scrub_check(true);
        wizard.run_preview();
        assert_eq!(wizard.state.step(), Step::Confirm);
        assert!(!wizard.state.preview_commands.is_empty());

        wizard.set_confirmation_text("not-shr1");
        assert!(!wizard.can_execute());
        assert!(!wizard.execute(&runner));
        assert!(runner.commands.lock().unwrap().is_empty());

        wizard.set_confirmation_text("shr1");
        assert!(wizard.can_execute());
    }

    /// `seed_group` never
    /// records a scrub, so `expand()`'s earlier check must block. The pre-fix
    /// wizard hardcoded `skip_scrub_check: true` in both `run_preview` and
    /// `execute`, so this scenario silently sailed straight to `Step::
    /// Confirm` with no warning at all -- exactly the "TUI is a more
    /// permissive superset of the CLI, in the dangerous direction" defect
    /// the coordinator flagged. The fix must land on `Step::
    /// ScrubCheckWarning` instead, with the message preserved, and must
    /// NOT reach `Step::Confirm` (so `can_execute()`/`execute()` cannot be
    /// used to proceed) until the operator explicitly opts in.
    #[test]
    fn run_preview_surfaces_the_e19_scrub_warning_instead_of_silently_bypassing_it() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);
        let runner = RecordingRunner::new();

        wizard.run_preflight();
        assert_eq!(wizard.state.step(), Step::Preview);
        wizard.run_preview();

        assert_eq!(
            wizard.state.step(),
            Step::ScrubCheckWarning,
            "must stop at a distinct, visible warning step, not silently reach Confirm"
        );
        let warning = wizard
            .state
            .scrub_check_warning
            .as_ref()
            .expect("warning text must be set");
        assert!(warning.contains("--skip-scrub-check"), "{warning}");
        assert!(
            wizard.state.preview_state.is_none(),
            "must not have produced a preview to confirm"
        );
        assert!(
            wizard.state.error_message.is_none(),
            "this is a warning, not Step::Error"
        );

        // The safety gate must actually gate: nothing executable yet.
        assert!(!wizard.can_execute());
        wizard.set_confirmation_text("shr1");
        assert!(
            !wizard.can_execute(),
            "typing the name alone must not bypass the scrub warning"
        );
        assert!(
            !wizard.execute(&runner),
            "execute() must refuse from Step::ScrubCheckWarning"
        );
        assert!(
            runner.commands.lock().unwrap().is_empty(),
            "no engine call must have happened"
        );
    }

    /// The explicit override (`set_skip_scrub_check(true)`, the TUI
    /// equivalent of `--skip-scrub-check`) must actually let the wizard
    /// proceed past the warning to a real `Confirm` -- proving the gate is
    /// recoverable by a deliberate operator action, not a dead end.
    #[test]
    fn set_skip_scrub_check_then_rerunning_preview_reaches_confirm() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);

        wizard.run_preflight();
        wizard.run_preview();
        assert_eq!(wizard.state.step(), Step::ScrubCheckWarning);

        wizard.set_skip_scrub_check(true);
        wizard.run_preview();

        assert_eq!(wizard.state.step(), Step::Confirm);
        assert!(wizard.state.preview_state.is_some());
        assert!(
            wizard.state.scrub_check_warning.is_none(),
            "the stale warning must be cleared on success"
        );
    }

    /// Proves the wizard actually delegates to the real engine once every
    /// gate is satisfied -- NOT that `engine.expand()` itself succeeds
    /// end-to-end (that's `shr-orchestrate`'s own ~56-test suite's job,
    /// which needs a much richer canned-response runner than this file's
    /// minimal `RecordingRunner` to get all the way to a real success).
    /// `RecordingRunner` answers every read with empty output, so this run
    /// is expected to fail partway through real execution -- what matters
    /// here is that `execute()` reached the engine at all (a command was
    /// recorded) once `can_execute()` was true, proving constraint 1 is
    /// satisfied from the "everything is in order" side, not just the
    /// "something is missing" side the other tests in this file cover.
    #[test]
    fn once_confirmed_execute_calls_through_to_the_real_engine() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);
        let runner = RecordingRunner::new();

        wizard.run_preflight();
        // `seed_group` never recorded a scrub -- override the gate explicitly.
        wizard.set_skip_scrub_check(true);
        wizard.run_preview();
        wizard.set_confirmation_text("shr1");
        assert!(wizard.can_execute());
        assert!(wizard.execute(&runner), "execute() must have attempted the call");

        assert!(
            !runner.commands.lock().unwrap().is_empty(),
            "the real engine call must have run"
        );
        assert_ne!(
            wizard.state.step(),
            Step::Confirm,
            "state must have moved past confirm one way or another"
        );
    }

    /// Constraint 6, proven with a deterministic failure: a runner that
    /// fails every command (so `expand()` fails immediately at its
    /// prerequisite `ensure_supported()` checks, before touching anything)
    /// must land the wizard on `Step::Error`, never `Step::Done`.
    #[test]
    fn a_failing_runner_lands_execute_on_error_never_done() {
        struct AlwaysFailingRunner;
        impl CommandRunner for AlwaysFailingRunner {
            fn run(&self, _program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
                Err(ExecError::Io(std::io::Error::other("no such tool in this test")))
            }
            fn is_dry_run(&self) -> bool {
                false
            }
        }

        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);

        wizard.run_preflight();
        // `seed_group` never recorded a scrub -- override the gate explicitly so
        // this reaches `execute()` at all (otherwise `can_execute()` would
        // refuse from `Step::ScrubCheckWarning`, which is a different,
        // already-covered refusal path, not the one this test targets).
        wizard.set_skip_scrub_check(true);
        wizard.run_preview();
        wizard.set_confirmation_text("shr1");
        assert!(wizard.execute(&AlwaysFailingRunner));

        assert_eq!(wizard.state.step(), Step::Error);
        assert!(wizard.state.result.is_none());
        assert!(wizard.state.error_message.is_some());
    }

    #[test]
    fn a_preflight_error_lands_on_error_never_preview_or_confirm() {
        // No lsblk fixture at all -- `by_id_index`/`block_devices` default
        // empty via `StaticInspector::default()`, so the requested kernel
        // name "sdb" simply won't resolve; `preflight_create` itself
        // doesn't error on an unresolved name (it reports a `NotFound`
        // blocker), so this specifically exercises the "backend returned
        // ok: false" path landing on `Step::Preflight`, not `Step::Error`.
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = StaticInspector::default();
        let mut wizard = controller(&inspector, store);

        wizard.run_preflight();
        assert_eq!(wizard.state.step(), Step::Preflight);
        assert!(!wizard.state.preflight.as_ref().unwrap().ok);
    }

    // TDD red step -- `ReplaceDiskController`/`ReplaceStep` do not
    // exist yet. `group_name` is `Option<String>` (not the dead agent's
    // sketch's bare `String`) to mirror `AddDiskController` and
    // `replace_disk`'s own `Option<&str>` -- and `can_execute`/`execute`
    // take no group-name argument, since the controller already owns one.
    #[test]
    fn replace_execute_is_refused_before_select_or_without_matching_confirmation() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut ctrl = ReplaceDiskController::new(
            &inspector,
            store,
            "mdadm.conf",
            "fstab",
            vec!["sda".to_string()],
            Some("shr1".to_string()),
        );
        let runner = RecordingRunner::new();

        assert_eq!(ctrl.state.step(), ReplaceStep::Select);
        assert!(!ctrl.can_execute(), "must refuse before select()/run_preview()");
        assert!(!ctrl.execute(&runner));
        assert!(runner.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn expanding_a_nonexistent_group_fails_the_preview_not_silently_succeeds() {
        let dir = tempdir().unwrap();
        // No group seeded at all -- `preview_expand` must fail with
        // NoActiveArray, landing the wizard on Step::Error.
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        let inspector = inspector_with_new_disk();
        let mut wizard = controller(&inspector, store);

        wizard.run_preflight();
        assert_eq!(wizard.state.step(), Step::Preview);
        wizard.run_preview();

        assert_eq!(wizard.state.step(), Step::Error);
        assert!(wizard.state.error_message.is_some());
        assert!(wizard.state.preview_state.is_none());
    }

    // -- ReplaceDiskController -----------------------------------

    fn replace_controller(inspector: &StaticInspector, store: Arc<StateStore>) -> ReplaceDiskController<'_> {
        ReplaceDiskController::new(
            inspector,
            store,
            "mdadm.conf",
            "fstab",
            vec!["sda".to_string()],
            Some("shr1".to_string()),
        )
    }

    /// Full gate sequence: `select()` -> `run_preview()` reaches `Confirm`
    /// with a non-empty preview, matching confirmation text unlocks
    /// `can_execute()`, and `execute()` actually calls through to the real
    /// engine once every gate is satisfied.
    ///
    /// Does NOT assert `Step::Done` -- like `AddDiskController`'s own
    /// `once_confirmed_execute_calls_through_to_the_real_engine`,
    /// `RecordingRunner` answers every read with empty output, and
    /// verified empirically here (`cargo test -- --nocapture` while this
    /// test carried a temporary `eprintln!`) that `replace_disk` actually
    /// fails partway through real execution on this fixture: `mdadm
    /// .degraded_count` reads `/sys/block/md0/md/degraded` through the
    /// runner, gets `""` back, and fails to parse it as a `u32`, landing
    /// on `Step::Error` with message "could not parse degraded count from
    /// /sys/block/md0/md/degraded". What this test actually proves is
    /// that `execute()` reached the real engine at all (a command was
    /// recorded) once `can_execute()` was true -- constraint 1 satisfied
    /// from the "everything is in order" side. A true `Step::Done` requires
    /// `shr-orchestrate`'s own richer canned-response runner, not this
    /// file's minimal fixtures.
    #[test]
    fn replace_full_flow_calls_through_to_the_real_engine_once_every_gate_is_satisfied() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut ctrl = replace_controller(&inspector, store);
        let runner = RecordingRunner::new();

        ctrl.select("ata-EXISTING1", "sdb");
        assert_eq!(
            ctrl.state.step(),
            ReplaceStep::Select,
            "select() alone must not advance the step"
        );
        assert!(!ctrl.can_execute());

        ctrl.run_preview();
        assert_eq!(ctrl.state.step(), ReplaceStep::Confirm);
        assert!(!ctrl.state.preview_commands.is_empty());
        assert!(ctrl.state.preview_state.is_some());
        assert!(
            runner.commands.lock().unwrap().is_empty(),
            "preview must never touch the real runner"
        );

        ctrl.set_confirmation_text("not-shr1");
        assert!(
            !ctrl.can_execute(),
            "mismatched confirmation text must refuse execute"
        );
        assert!(!ctrl.execute(&runner));
        assert!(runner.commands.lock().unwrap().is_empty());

        ctrl.set_confirmation_text("shr1");
        assert!(ctrl.can_execute());
        assert!(ctrl.execute(&runner), "execute() must have attempted the call");
        assert!(
            !runner.commands.lock().unwrap().is_empty(),
            "the real engine call must have run"
        );
        assert_ne!(
            ctrl.state.step(),
            ReplaceStep::Confirm,
            "state must have moved past confirm one way or another"
        );
    }

    /// `run_preview()` without `select()` first must fail cleanly (no
    /// disk chosen to resolve/replace), landing on `Step::Error`, never
    /// `Step::Confirm`.
    #[test]
    fn replace_run_preview_without_select_lands_on_error() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut ctrl = replace_controller(&inspector, store);

        ctrl.run_preview();

        assert_eq!(ctrl.state.step(), ReplaceStep::Error);
        assert!(ctrl.state.error_message.is_some());
        assert!(ctrl.state.preview_state.is_none());
    }

    /// Replacing with a disk that isn't actually a free/resolvable disk
    /// (never in this fixture's `lsblk`) fails the preview instead of
    /// silently succeeding -- same "backend error must surface, not be
    /// swallowed" discipline as `AddDiskController`'s equivalent test.
    #[test]
    fn replace_run_preview_with_unresolvable_new_disk_lands_on_error() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut ctrl = replace_controller(&inspector, store);

        ctrl.select("ata-EXISTING1", "sdz-does-not-exist");
        ctrl.run_preview();

        assert_eq!(ctrl.state.step(), ReplaceStep::Error);
        assert!(ctrl.state.error_message.is_some());
        assert!(ctrl.state.preview_state.is_none());
    }

    /// Constraint 6's replace analogue: a runner that fails every command
    /// must land `execute()` on `Step::Error`, never `Step::Done`, even
    /// after every earlier gate (select, preview, matching confirmation)
    /// was satisfied.
    #[test]
    fn replace_a_failing_runner_lands_execute_on_error_never_done() {
        struct AlwaysFailingRunner;
        impl CommandRunner for AlwaysFailingRunner {
            fn run(&self, _program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
                Err(ExecError::Io(std::io::Error::other("no such tool in this test")))
            }
            fn is_dry_run(&self) -> bool {
                false
            }
        }

        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut ctrl = replace_controller(&inspector, store);

        ctrl.select("ata-EXISTING1", "sdb");
        ctrl.run_preview();
        assert_eq!(ctrl.state.step(), ReplaceStep::Confirm);
        ctrl.set_confirmation_text("shr1");
        assert!(ctrl.execute(&AlwaysFailingRunner));

        assert_eq!(ctrl.state.step(), ReplaceStep::Error);
        assert!(ctrl.state.result.is_none());
        assert!(ctrl.state.error_message.is_some());
    }

    /// Replacing a disk that isn't a member of the target group must fail
    /// the preview cleanly (the real engine's own check), not silently
    /// pretend the wizard's job is done.
    #[test]
    fn replace_of_a_disk_not_in_the_group_fails_the_preview() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_group(&store, "shr1");
        let inspector = inspector_with_new_disk();
        let mut ctrl = replace_controller(&inspector, store);

        ctrl.select("ata-NOT-A-MEMBER", "sdb");
        ctrl.run_preview();

        assert_eq!(ctrl.state.step(), ReplaceStep::Error);
        assert!(ctrl
            .state
            .error_message
            .as_ref()
            .unwrap()
            .contains("not a member"));
    }
}
