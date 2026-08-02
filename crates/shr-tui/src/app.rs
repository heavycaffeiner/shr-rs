use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use shr_command::{build_fs_df, FsDfReport, StatusReport};

use crate::scrub::{ScrubState, Step as ScrubStep};
use crate::wizard::{ReplaceStep, ReplaceWizardState, Step, WizardState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Disks,
    Arrays,
    /// Every SHR group `state.toml` records -- previously invisible to
    /// the TUI entirely (it only ever read `report.arrays`/`report.disks`,
    /// never `report.groups`), so a host managing more than one group had
    /// no way to see that from the terminal.
    Groups,
    /// Per-band view, cross-referenced against `report.arrays[].sync`
    /// by `md_name` for live reshape/resync progress -- `GroupBandStatus`
    /// alone carries no live sync percentage.
    Bands,
    /// Per-group filesystem view (mount point, fs UUID, usable bytes,
    /// pending-resize flag).
    Fs,
    /// Recent kernel log lines (`journalctl -k`), the honest read-only
    /// substitute for a dedicated log store the current schema doesn't have.
    Logs,
}

impl Tab {
    pub const ALL: [Self; 7] = [
        Self::Dashboard,
        Self::Disks,
        Self::Arrays,
        Self::Groups,
        Self::Bands,
        Self::Fs,
        Self::Logs,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Dashboard => 0,
            Self::Disks => 1,
            Self::Arrays => 2,
            Self::Groups => 3,
            Self::Bands => 4,
            Self::Fs => 5,
            Self::Logs => 6,
        }
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One completed background inspection cycle: the live status report plus
/// recent kernel log lines. Bundled into one type so a single
/// `RefreshWorker` cycle produces both without a second background thread --
/// `journalctl -k -n 200` is cheap enough to run alongside `build_status` on
/// the same 2-second poll.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub report: StatusReport,
    pub logs: Vec<String>,
    /// Live Btrfs used/free figures for the FS tab, refreshed by
    /// `runtime.rs` on a decimated schedule (not every 2s poll -- see
    /// `runtime::FsUsageCache`'s doc comment) to avoid spawning `btrfs`/`df`
    /// subprocesses every cycle. `build_fs_df` is pure and infallible: a
    /// group with no live usage yet (or a runner failure) simply carries
    /// `None` per-field, which `render_fs_row` shows as `?` -- never a
    /// fabricated or stale-but-unlabeled number.
    pub fs_df: FsDfReport,
}

/// An intent flag for `main.rs` to act on -- `App` never performs IO itself
/// (mirrors `refresh_requested`/`RefreshWorker`'s split: this struct is pure
/// UI state, `main.rs` owns the actual `wizard::AddDiskController` and the
/// `Inspector`/`CommandRunner` it needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardAction {
    RunPreflight,
    RunPreview,
    /// The operator's explicit, deliberate action from `Step::
    /// ScrubCheckWarning` to override the pre-reshape scrub-freshness
    /// check and re-run the preview -- the TUI equivalent of the operator
    /// typing `shr-rs expand --skip-scrub-check`. Never fired implicitly.
    AcceptScrubCheckWarning,
    Execute,
}

/// Intent flags for `runtime.rs` to act on for the Replace Disk wizard
/// -- same IO-free split as `WizardAction`: `App` never performs IO
/// itself, only records what the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceAction {
    /// Both the old member and the new free disk have been picked
    /// (`ReplaceView::selected_old`/`selected_new` both `Some`) --
    /// `runtime.rs` builds a fresh `ReplaceDiskController`, calls
    /// `select()` then `run_preview()`. An in-process dry run against
    /// `DryRunRunner` (see `wizard::ReplaceDiskController::run_preview`'s
    /// doc comment) -- same cost class as `WizardAction::RunPreflight`/
    /// `RunPreview`, so no background thread here either.
    RunPreview,
    /// The real, irreversible call -- same background-thread treatment as
    /// `WizardAction::Execute`, since `ReplaceDiskController::execute` can
    /// run a real, potentially hours-long mdadm replace/copy.
    Execute,
}

/// Intent flags for `runtime.rs` to act on for the scrub start/cancel
/// controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubUiAction {
    /// The operator pressed `s` while no scrub is running on the target
    /// group (per the currently displayed `report.groups[].bands[].
    /// scrub_in_progress` -- already-fetched display data, not new IO, the
    /// same precedent `open_wizard` already sets by reading
    /// `self.report.groups.len()`). `runtime.rs` builds a fresh
    /// `ScrubController` and calls `request_start()`, a pure state
    /// transition with no IO of its own.
    RequestStart,
    /// Same as `RequestStart`, but the operator pressed `s` while a scrub
    /// IS running -- `runtime.rs` calls `request_cancel()` instead.
    RequestCancel,
    /// The operator confirmed (`ConfirmStart`: plain Enter; `ConfirmCancel`:
    /// typed group name + Enter) -- `runtime.rs` calls `confirm_start()`/
    /// `confirm_cancel()` synchronously, deliberately NOT on a background
    /// thread. `scrub_start`/`scrub_cancel` (`OrchestrationEngine`) are a
    /// handful of sysfs reads/writes per band plus one `btrfs scrub
    /// cancel` call -- bounded, sub-second work, not a multi-hour reshape
    /// like `AddDiskController`/`ReplaceDiskController::execute()`.
    /// `scrub::Step` (`crates/shr-tui/src/scrub.rs`), unlike `wizard::
    /// WizardState`/`ReplaceWizardState`, has no `Executing` variant at
    /// all -- that absence is this module's own confirmation that no
    /// background thread was ever meant to sit between `ConfirmStart`/
    /// `ConfirmCancel` and `Done`/`Error`.
    Confirm,
}

/// One row of the Add Disk wizard's disk picker. Carries `system_disk`
/// straight from `DiskStatus` (the `disk list` flag) so `handle_wizard_key`
/// can refuse selection and `ui.rs` can mark the row: the wizard
/// previously threw away this flag by collapsing `report.disks` into plain
/// `Vec<String>` names, which is exactly why the system disk was selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCandidate {
    pub name: String,
    pub system_disk: bool,
}

/// UI-only state for the Add Disk wizard: which disks are selected,
/// the confirmation text typed so far, and the last `WizardState` snapshot
/// `main.rs` reported back after driving the controller. `App` never calls
/// into `shr_command`/`shr_orchestrate` itself.
#[derive(Debug, Clone, Default)]
pub struct WizardView {
    pub group_name: String,
    pub candidate_disks: Vec<DiskCandidate>,
    pub cursor: usize,
    pub selected: Vec<String>,
    pub confirmation_input: String,
    pub controller_state: WizardState,
    /// Set when the operator pressed [Space] on the system disk row --
    /// `ui.rs` renders this next to the list, the same spot
    /// `wizard_preflight_text` shows its BLOCK/WARN lines. Cleared on the
    /// next `SelectDisks` keypress so it never lingers past the moment it's
    /// relevant.
    pub selection_blocked_reason: Option<String>,
    /// The operator's explicit override of `preflight_write_targets`'s
    /// `HasContent` blocker (the TUI equivalent of `shr-rs --force-content`
    /// on the CLI). Off by default, never inferred, never carried over from
    /// a previous wizard run -- `open_wizard` always rebuilds a fresh
    /// `WizardView` with this `false`. Only `handle_wizard_key`'s
    /// `Step::Preflight` arm (a deliberate, non-Enter `o` keypress, matching
    /// the `skip_scrub_check` bar) ever sets it `true`.
    pub force_content: bool,
    // `pub(crate)`, not private: `ui.rs`'s tests build `WizardView` literals
    // directly (see the rendering tests) via `..Default::default()`, which
    // requires every field to be visible from that module, not just this one.
    pub(crate) pending_action: Option<WizardAction>,
}

impl WizardView {
    pub fn step(&self) -> Step {
        self.controller_state.step()
    }
}

/// One row of the Replace Disk wizard's OLD-member picker -- the
/// group's current members, identified the way `ReplaceDiskController::
/// select`'s `old_disk_id` parameter requires: a `StateDisk::id` (an
/// `ata-...` by-id string), never a kernel name. `display` is the best
/// human-readable label available -- the matching kernel name plus the id
/// when `report.disks` still has that disk live, or just the bare id when
/// it doesn't (e.g. a failed disk already dropped from `lsblk`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceOldCandidate {
    pub id: String,
    pub display: String,
}

/// UI-only state for the Replace Disk wizard: which member is being
/// retired, which free disk replaces it, the confirmation text typed so
/// far, and the last `ReplaceWizardState` snapshot `runtime.rs` reported
/// back after driving `wizard::ReplaceDiskController`. Same split as
/// `WizardView`/`AddDiskController`: `App` never calls into
/// `shr_orchestrate` itself, only records intent (`pending_action`) for
/// `runtime.rs` to act on.
#[derive(Debug, Clone, Default)]
pub struct ReplaceView {
    pub group_name: String,
    /// The group's current members. See `ReplaceOldCandidate`'s doc comment.
    pub old_candidates: Vec<ReplaceOldCandidate>,
    /// Free disks (`DiskStatus.arrays.is_empty()`), reusing `DiskCandidate`
    /// -- same shape the Add Disk wizard already uses, including the
    /// `system_disk` flag so the new-disk picker refuses the system disk
    /// exactly the same way (a system disk is typically not an mdadm
    /// member, so it would otherwise show up here as "free").
    pub new_candidates: Vec<DiskCandidate>,
    pub old_cursor: usize,
    pub new_cursor: usize,
    /// `false` while picking the OLD member, `true` while picking the NEW
    /// free disk -- both happen before `ReplaceDiskController::select` is
    /// ever called; there is no controller-level step for "half selected".
    pub picking_new: bool,
    pub selected_old: Option<String>,
    pub selected_new: Option<String>,
    pub confirmation_input: String,
    pub controller_state: ReplaceWizardState,
    /// Same discipline as `WizardView::selection_blocked_reason`: set
    /// when the operator tried to pick the system disk as the replacement,
    /// cleared on the next keypress in the picking step.
    pub selection_blocked_reason: Option<String>,
    pub(crate) pending_action: Option<ReplaceAction>,
}

impl ReplaceView {
    pub fn step(&self) -> ReplaceStep {
        self.controller_state.step()
    }
}

/// UI-only state for the scrub start/cancel controls: the target
/// group and the last `scrub::ScrubState` snapshot `runtime.rs` reported
/// back after driving `scrub::ScrubController`. Same IO-free split as
/// `WizardView`/`ReplaceView`.
#[derive(Debug, Clone, Default)]
pub struct ScrubView {
    pub group_name: String,
    /// Typed by the operator during `scrub::Step::ConfirmCancel`; unused
    /// (and not rendered) during `ConfirmStart`, which needs no typed name
    /// -- mirrors `scrub::ScrubController::can_confirm_cancel`'s own gate.
    pub confirmation_input: String,
    pub controller_state: ScrubState,
    pub(crate) pending_action: Option<ScrubUiAction>,
}

impl ScrubView {
    pub fn step(&self) -> ScrubStep {
        self.controller_state.step()
    }
}

/// The reconcile control -- `shr-rs reconcile` finishes any LVM/Btrfs
/// resize a previous `expand` had to defer while its mdadm reshape was
/// still running, the documented remedy for the `resize_pending` warning
/// badge already rendered on the Groups/Bands/Fs tabs (`ui.rs`) with no way
/// to act on it before this.
///
/// No dedicated `reconcile.rs` controller module exists (unlike `scrub.rs`/
/// `wizard.rs`) -- `lib.rs` is not one of this task's owned files, so a new
/// `mod reconcile;` declaration cannot be added there. These types live
/// directly in `app.rs` (pure UI state, same shape scrub/wizard use) and the
/// actual `OrchestrationEngine::reconcile()` call lives directly in
/// `runtime.rs`, mirroring `refresh_fs_df`'s split of IO into a directly
/// testable free function rather than a controller struct.
///
/// Unlike `WizardState`/`ReplaceWizardState`/`ScrubState`, this step
/// sequence has no request/decision step at all (no Start-vs-Cancel branch
/// the way scrub has) -- `shr-cli`'s `Command::Reconcile` takes no `--name`,
/// it is not scoped to a group, so opening the modal goes straight to
/// `Confirm`: the operator's one explicit gate before the real call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileStep {
    /// Modal is open, nothing has run yet -- reachable directly from
    /// `open_reconcile`. Reconcile is idempotent and non-destructive
    /// (`OrchestrationEngine::reconcile`'s own doc comment: it never starts
    /// a new destructive action, only finishes bookkeeping for a reshape a
    /// prior `expand()` already committed), so this needs no typed-group-
    /// name gate the way `ReplaceStep::Confirm`/`Step::Confirm` do -- a
    /// single distinct confirm step (this one) is the right bar, not a bare
    /// keypress that immediately acts, and not the stronger irreversible-op
    /// gate either.
    Confirm,
    /// A background thread (`runtime.rs`, same treatment as
    /// `WizardAction::Execute`/`ReplaceAction::Execute`) is running the
    /// real `OrchestrationEngine::reconcile()` call -- can include a real
    /// `btrfs filesystem resize`, not bounded/sub-second the way scrub
    /// start/cancel are.
    Executing,
    Done,
    Error,
}

/// One performed reconcile action, described with the exact English wording
/// `shr-cli`'s own `describe_reconcile_action` uses (duplicated in
/// `runtime.rs` for the same reason `fs_usage_map`/`fs_usage_input` are
/// already duplicated there: `shr-tui` must not depend on `shr-cli`).
/// Rendered verbatim by `ui.rs` -- no rewording, no safety logic in this
/// layer (constraint 4).
pub type ReconcileActionLine = String;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileState {
    pub step: Option<ReconcileStep>,
    /// What `reconcile()` actually did, one pre-formatted line per action --
    /// empty when nothing was pending. Only meaningful once `step() ==
    /// ReconcileStep::Done`.
    pub performed: Vec<ReconcileActionLine>,
    /// Set when at least one band is still `resize_pending` after this run
    /// (its mdadm reshape had not finished yet) -- forwarded verbatim from
    /// the engine's own state, never synthesized.
    pub still_pending: Option<String>,
    pub error_message: Option<String>,
}

impl ReconcileState {
    pub fn step(&self) -> ReconcileStep {
        self.step.unwrap_or(ReconcileStep::Confirm)
    }
}

/// Intent flag for `runtime.rs` to act on for the reconcile control --
/// same IO-free split as `WizardAction`/`ReplaceAction`/`ScrubUiAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileUiAction {
    /// The operator confirmed at `ReconcileStep::Confirm` (Enter) --
    /// `runtime.rs` spawns a background thread that builds a real
    /// `SystemRunner` and calls `OrchestrationEngine::reconcile()`, the same
    /// background-thread treatment `WizardAction::Execute`/`ReplaceAction::
    /// Execute` get and for the same reason (a real, potentially slow
    /// `btrfs filesystem resize`).
    Execute,
}

/// UI-only state for the reconcile control: the last `ReconcileState`
/// snapshot `runtime.rs` reported back after driving the background
/// reconcile call. Same split as `WizardView`/`ReplaceView`/`ScrubView`.
#[derive(Debug, Clone, Default)]
pub struct ReconcileView {
    pub controller_state: ReconcileState,
    pub(crate) pending_action: Option<ReconcileUiAction>,
}

impl ReconcileView {
    pub fn step(&self) -> ReconcileStep {
        self.controller_state.step()
    }
}

#[derive(Debug, Clone)]
pub struct App {
    report: StatusReport,
    logs: Vec<String>,
    fs_df: FsDfReport,
    tab: Tab,
    should_quit: bool,
    refresh_requested: bool,
    error: Option<String>,
    wizard: Option<WizardView>,
    replace: Option<ReplaceView>,
    scrub: Option<ScrubView>,
    reconcile: Option<ReconcileView>,
}

impl App {
    pub fn new(report: StatusReport) -> Self {
        // No live Btrfs usage has been fetched yet at construction time
        // -- `build_fs_df` against an empty usage map is the honest starting
        // point (every optional field `None` -> `?`), not a zeroed-out or
        // guessed figure.
        let fs_df = build_fs_df(&report.groups, &std::collections::BTreeMap::new());
        Self {
            report,
            logs: Vec::new(),
            fs_df,
            tab: Tab::Dashboard,
            should_quit: false,
            refresh_requested: false,
            error: None,
            wizard: None,
            replace: None,
            scrub: None,
            reconcile: None,
        }
    }

    pub fn report(&self) -> &StatusReport {
        &self.report
    }

    pub fn fs_df(&self) -> &FsDfReport {
        &self.fs_df
    }

    pub fn logs(&self) -> &[String] {
        &self.logs
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn replace_report(&mut self, report: StatusReport) {
        self.report = report;
        self.error = None;
    }

    pub fn replace_snapshot(&mut self, snapshot: Snapshot) {
        self.report = snapshot.report;
        self.logs = snapshot.logs;
        self.fs_df = snapshot.fs_df;
        self.error = None;
    }

    /// Record a refresh failure without throwing away the last known-good
    /// report. A transient `smartctl`/`lsblk` problem must not blank the TUI.
    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    pub fn take_refresh_requested(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    pub fn wizard(&self) -> Option<&WizardView> {
        self.wizard.as_ref()
    }

    /// `main.rs` calls this after driving the controller in response to a
    /// `WizardAction` (or the background execute thread finishing) to push
    /// the result back into UI state.
    ///
    /// The background `execute()` thread (`runtime.rs`'s
    /// `WizardAction::Execute`) can finish after the wizard modal is gone --
    /// the real mdadm/LVM/Btrfs work already happened either way, so its
    /// outcome must not be silently dropped just because there's no modal
    /// left to write it into. Surfaced through the same error/message
    /// banner a refresh failure already uses, not a new channel.
    pub fn set_wizard_state(&mut self, state: WizardState) {
        match &mut self.wizard {
            Some(w) => w.controller_state = state,
            None => self.error = Some(wizard_result_message(&state)),
        }
    }

    pub fn take_wizard_action(&mut self) -> Option<WizardAction> {
        self.wizard.as_mut().and_then(|w| w.pending_action.take())
    }

    pub fn replace(&self) -> Option<&ReplaceView> {
        self.replace.as_ref()
    }

    /// `runtime.rs` calls this after driving `wizard::ReplaceDiskController`
    /// (or the background execute thread finishing) to push the result back
    /// into UI state. Same late-arrival handling as `set_wizard_state`:
    /// if the Replace modal is already gone, the real mdadm work already
    /// happened either way, so the outcome goes to the error/message banner
    /// instead of being dropped.
    pub fn set_replace_state(&mut self, state: ReplaceWizardState) {
        match &mut self.replace {
            Some(r) => r.controller_state = state,
            None => self.error = Some(replace_result_message(&state)),
        }
    }

    pub fn take_replace_action(&mut self) -> Option<ReplaceAction> {
        self.replace.as_mut().and_then(|r| r.pending_action.take())
    }

    pub fn scrub(&self) -> Option<&ScrubView> {
        self.scrub.as_ref()
    }

    /// `runtime.rs` calls this after driving `scrub::ScrubController` to push
    /// the result back into UI state. `confirm_start`/`confirm_cancel` run
    /// synchronously (see `ScrubUiAction::Confirm`'s doc comment), so unlike
    /// `set_wizard_state`/`set_replace_state` there is no background-thread
    /// late-arrival case here -- the modal is still open every time this is
    /// called in practice. The `None` branch is kept anyway, for the same
    /// reason: never silently drop a real result just because there's
    /// nowhere obvious to put it.
    pub fn set_scrub_state(&mut self, state: ScrubState) {
        match &mut self.scrub {
            Some(s) => s.controller_state = state,
            None => self.error = Some(scrub_result_message(&state)),
        }
    }

    pub fn take_scrub_action(&mut self) -> Option<ScrubUiAction> {
        self.scrub.as_mut().and_then(|s| s.pending_action.take())
    }

    pub fn reconcile(&self) -> Option<&ReconcileView> {
        self.reconcile.as_ref()
    }

    /// `runtime.rs` calls this after driving the background reconcile call
    /// to push the result back into UI state. Same late-arrival
    /// handling as `set_wizard_state`/`set_replace_state` (both of which
    /// also run their real work on a background thread) -- if the modal is
    /// already gone by the time the thread finishes, the real reconcile
    /// call already happened either way, so the outcome goes to the
    /// error/message banner instead of being dropped.
    pub fn set_reconcile_state(&mut self, state: ReconcileState) {
        match &mut self.reconcile {
            Some(r) => r.controller_state = state,
            None => self.error = Some(reconcile_result_message(&state)),
        }
    }

    pub fn take_reconcile_action(&mut self) -> Option<ReconcileUiAction> {
        self.reconcile.as_mut().and_then(|r| r.pending_action.take())
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        // At most one modal is ever open at a time (`open_wizard`/
        // `open_replace`/`open_scrub` all refuse to open over an existing
        // one) -- this chain is what enforces that on the input side: once
        // any one of them is open, every key goes to its own handler and
        // never reaches the top-level match below, so `a`/`x`/`s` and the
        // tab/nav keys can never fire while a modal is up.
        if self.wizard.is_some() {
            self.handle_wizard_key(key.code);
            return;
        }
        if self.replace.is_some() {
            self.handle_replace_key(key.code);
            return;
        }
        if self.scrub.is_some() {
            self.handle_scrub_key(key.code);
            return;
        }
        if self.reconcile.is_some() {
            self.handle_reconcile_key(key.code);
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('r') => self.refresh_requested = true,
            KeyCode::Char('a') => self.open_wizard(),
            // `x`: "eXchange" -- Replace Disk. `r` is already refresh;
            // `d` reads as "delete" to an operator's eye, which this is not.
            KeyCode::Char('x') => self.open_replace(),
            // `s`: Scrub start/cancel -- free, mnemonic, not already
            // bound to a top-level action.
            KeyCode::Char('s') => self.open_scrub(),
            // `f`: reconcile -- "finish" a deferred resize, matching
            // `Command::Reconcile`'s own doc comment ("Finish any LVM/Btrfs
            // resize..."). Free: not q/Esc/r/a/x/s/1-7/Tab/arrows/h/l.
            KeyCode::Char('f') => self.open_reconcile(),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.tab = self.tab.next(),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.tab = self.tab.previous(),
            KeyCode::Char('1') => self.tab = Tab::Dashboard,
            KeyCode::Char('2') => self.tab = Tab::Disks,
            KeyCode::Char('3') => self.tab = Tab::Arrays,
            KeyCode::Char('4') => self.tab = Tab::Groups,
            KeyCode::Char('5') => self.tab = Tab::Bands,
            KeyCode::Char('6') => self.tab = Tab::Fs,
            KeyCode::Char('7') => self.tab = Tab::Logs,
            _ => {}
        }
    }

    /// Pick the group to expand and the candidate disk list from the
    /// current report, then open the wizard. Simplification (documented,
    /// not silent): only auto-opens when exactly one group exists -- with
    /// several groups, `shr-rs expand --name <group>` already requires the
    /// operator to be explicit about which one (`resolve_group_index`'s doc
    /// comment), and building a group-picker sub-step here was cut from this
    /// pass; the CLI remains the way to expand a specific group when a host
    /// manages more than one. Reported via `self.error` (the existing
    /// status-panel banner), not silently ignored.
    fn open_wizard(&mut self) {
        if self.wizard.is_some() {
            return;
        }
        match self.report.groups.len() {
            1 => {
                let group_name = self.report.groups[0].name.clone();
                self.wizard = Some(WizardView {
                    group_name,
                    candidate_disks: self
                        .report
                        .disks
                        .iter()
                        .map(|d| DiskCandidate { name: d.name.clone(), system_disk: d.system_disk })
                        .collect(),
                    cursor: 0,
                    selected: Vec::new(),
                    confirmation_input: String::new(),
                    controller_state: WizardState::default(),
                    selection_blocked_reason: None,
                    force_content: false,
                    pending_action: None,
                });
                self.error = None;
            }
            0 => self.error = Some("Add Disk: no group to expand. Create a group first.".to_string()),
            _ => self.error = Some(
                "Add Disk: multiple groups exist. The TUI only auto-selects a single group -- \
                 use `shr-rs expand --name <group> --add <disk>` to target the one you want."
                    .to_string(),
            ),
        }
    }

    fn close_wizard(&mut self) {
        self.wizard = None;
    }

    /// Only called while `self.wizard.is_some()` -- consumes the key
    /// entirely (the caller must not also run normal tab-navigation
    /// handling for it), so almost every key is captured while the wizard
    /// is open except `Esc` (cancel/close).
    ///
    /// `Esc` cancels/closes on every step EXCEPT `Step::Executing` --
    /// that step means a background thread is mid-way through the real,
    /// potentially hours-long `AddDiskController::execute()` against real
    /// mdadm/LVM/Btrfs (`runtime.rs`'s `WizardAction::Execute`). Closing the
    /// modal there doesn't cancel that work; it only throws away the report
    /// of whether it succeeded. Refuse and say why instead.
    fn handle_wizard_key(&mut self, code: KeyCode) {
        if code == KeyCode::Esc {
            match self.wizard.as_ref().map(|w| w.step()) {
                Some(Step::Executing) => {
                    self.error = Some(
                        "Add Disk: the operation is running -- cannot close with Esc until it finishes."
                            .to_string(),
                    );
                }
                Some(_) => self.close_wizard(),
                None => {}
            }
            return;
        }
        let Some(wizard) = self.wizard.as_mut() else { return };

        match wizard.step() {
            Step::SelectDisks => {
                // Clear any stale refusal message on every keypress in
                // this step -- it must only ever describe the immediately
                // preceding action, not linger once the operator has moved
                // on (cursor moved, a different disk toggled, ...).
                wizard.selection_blocked_reason = None;
                match code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        wizard.cursor = wizard.cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if wizard.cursor + 1 < wizard.candidate_disks.len() {
                            wizard.cursor += 1;
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some(candidate) = wizard.candidate_disks.get(wizard.cursor) {
                            if candidate.system_disk {
                                // The backend's `WriteBlocker` already
                                // refuses the system disk unconditionally
                                // (see `system_disk_aliases` doc comment) --
                                // this doesn't hide a capability, it just
                                // stops offering, then one-step-later
                                // rejecting, the exact same disk. Mirrors
                                // Cockpit's `createGroupWizard.tsx` earlier fix.
                                wizard.selection_blocked_reason = Some(format!(
                                    "The system disk cannot be selected: /dev/{} -- the OS is running on this disk.",
                                    candidate.name
                                ));
                            } else {
                                let name = candidate.name.clone();
                                if let Some(pos) = wizard.selected.iter().position(|d| *d == name) {
                                    wizard.selected.remove(pos);
                                } else {
                                    wizard.selected.push(name);
                                }
                            }
                        }
                    }
                    KeyCode::Enter if !wizard.selected.is_empty() => {
                        wizard.pending_action = Some(WizardAction::RunPreflight);
                    }
                    _ => {}
                }
            }
            Step::Preview => {
                if code == KeyCode::Enter {
                    wizard.pending_action = Some(WizardAction::RunPreview);
                }
            }
            // Requires a distinct key from plain Enter (used
            // everywhere else to "continue") -- 'y' is a deliberate,
            // different action so an operator can't blow through this
            // safety gate on autopilot the same way they advance every
            // other step.
            Step::ScrubCheckWarning => {
                if code == KeyCode::Char('y') {
                    wizard.pending_action = Some(WizardAction::AcceptScrubCheckWarning);
                }
            }
            Step::Confirm => match code {
                KeyCode::Backspace => {
                    wizard.confirmation_input.pop();
                }
                KeyCode::Char(c) => wizard.confirmation_input.push(c),
                KeyCode::Enter if wizard.confirmation_input == wizard.group_name => {
                    wizard.pending_action = Some(WizardAction::Execute);
                }
                _ => {}
            },
            // The operator's explicit override of a `HasContent`
            // preflight blocker. `o` (not Enter, matching the
            // `skip_scrub_check` bar of requiring a distinct deliberate key)
            // sets `force_content` and re-fires `RunPreflight` unconditionally
            // -- no blocker-kind inspection here (that would be safety logic
            // living in the UI layer, which constraint 4 forbids); the
            // retried preflight is a cheap, side-effect-free read, and
            // `preflight_write_targets` alone decides whether the override
            // actually helps. `report.blockers` still lists every other
            // reason a disk was refused (system disk, no stable id, ...),
            // so a retry that doesn't help just shows the same blocked
            // screen again.
            Step::Preflight => {
                if code == KeyCode::Char('o') {
                    wizard.force_content = true;
                    wizard.pending_action = Some(WizardAction::RunPreflight);
                }
            }
            Step::Executing | Step::Done | Step::Error => {}
        }
    }

    /// Pick the group to replace a disk in, its current member list
    /// (`ReplaceOldCandidate`, keyed by `StateDisk::id`), and the free-disk
    /// list (any disk with an empty `arrays`), then open the Replace modal.
    /// Same single-group simplification as `open_wizard`, for the same
    /// reason -- documented via `self.error`, not silent; `shr-rs disk
    /// replace --name <group> --old <old> --new <new>` remains the way to
    /// target a specific group when a host manages more than one.
    fn open_replace(&mut self) {
        if self.replace.is_some() {
            return;
        }
        match self.report.groups.len() {
            1 => {
                let group = &self.report.groups[0];
                let group_name = group.name.clone();
                let old_candidates = group
                    .disks
                    .iter()
                    .map(|id| {
                        let display = self
                            .report
                            .disks
                            .iter()
                            .find(|d| d.id.as_deref() == Some(id.as_str()))
                            .map(|d| format!("{} ({id})", d.name))
                            .unwrap_or_else(|| id.clone());
                        ReplaceOldCandidate { id: id.clone(), display }
                    })
                    .collect();
                let new_candidates = self
                    .report
                    .disks
                    .iter()
                    .filter(|d| d.arrays.is_empty())
                    .map(|d| DiskCandidate { name: d.name.clone(), system_disk: d.system_disk })
                    .collect();
                self.replace = Some(ReplaceView {
                    group_name,
                    old_candidates,
                    new_candidates,
                    old_cursor: 0,
                    new_cursor: 0,
                    picking_new: false,
                    selected_old: None,
                    selected_new: None,
                    confirmation_input: String::new(),
                    controller_state: ReplaceWizardState::default(),
                    selection_blocked_reason: None,
                    pending_action: None,
                });
                self.error = None;
            }
            0 => self.error = Some("Replace Disk: no target group. Create a group first.".to_string()),
            _ => self.error = Some(
                "Replace Disk: multiple groups exist. The TUI only auto-selects a single group -- \
                 use `shr-rs disk replace --name <group> --old <old> --new <new>` to target the one you want."
                    .to_string(),
            ),
        }
    }

    fn close_replace(&mut self) {
        self.replace = None;
    }

    /// Only called while `self.replace.is_some()`. Same Esc-during-
    /// Executing refusal as `handle_wizard_key` -- `ReplaceStep::Executing`
    /// means `runtime.rs`'s background thread is mid-way through the real
    /// `ReplaceDiskController::execute()`.
    fn handle_replace_key(&mut self, code: KeyCode) {
        if code == KeyCode::Esc {
            match self.replace.as_ref().map(|r| r.step()) {
                Some(ReplaceStep::Executing) => {
                    self.error = Some(
                        "Replace Disk: the operation is running -- cannot close with Esc until it finishes."
                            .to_string(),
                    );
                }
                Some(_) => self.close_replace(),
                None => {}
            }
            return;
        }
        let Some(replace) = self.replace.as_mut() else { return };

        match replace.step() {
            ReplaceStep::Select => {
                // Same discipline as `handle_wizard_key`'s `SelectDisks`
                // arm: a refusal message must only ever describe the
                // immediately preceding action.
                replace.selection_blocked_reason = None;
                if !replace.picking_new {
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            replace.old_cursor = replace.old_cursor.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if replace.old_cursor + 1 < replace.old_candidates.len() {
                                replace.old_cursor += 1;
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(candidate) = replace.old_candidates.get(replace.old_cursor) {
                                replace.selected_old = Some(candidate.id.clone());
                                replace.picking_new = true;
                                replace.new_cursor = 0;
                            }
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            replace.new_cursor = replace.new_cursor.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if replace.new_cursor + 1 < replace.new_candidates.len() {
                                replace.new_cursor += 1;
                            }
                        }
                        // Back out to the old-member picker without closing
                        // the whole modal -- a plain wrong-list mis-key
                        // shouldn't cost the operator the modal itself.
                        KeyCode::Backspace => {
                            replace.picking_new = false;
                            replace.selected_old = None;
                        }
                        KeyCode::Enter => {
                            if let Some(candidate) = replace.new_candidates.get(replace.new_cursor) {
                                if candidate.system_disk {
                                    // Refuse the system disk here too --
                                    // never rely on the backend catching it
                                    // downstream when the picker can just as
                                    // well not offer it as a live selection.
                                    replace.selection_blocked_reason = Some(format!(
                                        "The system disk cannot be selected: /dev/{} -- the OS is running on this disk.",
                                        candidate.name
                                    ));
                                } else {
                                    replace.selected_new = Some(candidate.name.clone());
                                    if replace.selected_old.is_some() {
                                        replace.pending_action = Some(ReplaceAction::RunPreview);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            ReplaceStep::Confirm => match code {
                KeyCode::Backspace => {
                    replace.confirmation_input.pop();
                }
                KeyCode::Char(c) => replace.confirmation_input.push(c),
                KeyCode::Enter if replace.confirmation_input == replace.group_name => {
                    replace.pending_action = Some(ReplaceAction::Execute);
                }
                _ => {}
            },
            ReplaceStep::Executing | ReplaceStep::Done | ReplaceStep::Error => {}
        }
    }

    /// Decide Start vs Cancel from the currently displayed report data
    /// (`scrub_in_progress` on any band in the target group) -- reading
    /// already-fetched display data, not new IO, the same precedent
    /// `open_wizard`/`open_replace` already set. Same single-group
    /// simplification, same reason.
    fn open_scrub(&mut self) {
        if self.scrub.is_some() {
            return;
        }
        match self.report.groups.len() {
            1 => {
                let group = &self.report.groups[0];
                let in_progress = group.bands.iter().any(|b| b.scrub_in_progress);
                self.scrub = Some(ScrubView {
                    group_name: group.name.clone(),
                    confirmation_input: String::new(),
                    controller_state: ScrubState::default(),
                    pending_action: Some(if in_progress {
                        ScrubUiAction::RequestCancel
                    } else {
                        ScrubUiAction::RequestStart
                    }),
                });
                self.error = None;
            }
            0 => self.error = Some("Scrub: no target group. Create a group first.".to_string()),
            _ => self.error = Some(
                "Scrub: multiple groups exist. The TUI only auto-selects a single group -- \
                 use `shr-rs scrub start --name <group>` to target the one you want."
                    .to_string(),
            ),
        }
    }

    fn close_scrub(&mut self) {
        self.scrub = None;
    }

    /// Only called while `self.scrub.is_some()`. Unlike `handle_wizard_key`/
    /// `handle_replace_key`, `scrub::Step` has no `Executing` variant (see
    /// `ScrubUiAction::Confirm`'s doc comment) -- there is no in-flight
    /// background step to protect Esc from, so Esc always closes here.
    fn handle_scrub_key(&mut self, code: KeyCode) {
        if code == KeyCode::Esc {
            self.close_scrub();
            return;
        }
        let Some(scrub) = self.scrub.as_mut() else { return };

        match scrub.step() {
            ScrubStep::ConfirmStart => {
                if code == KeyCode::Enter {
                    scrub.pending_action = Some(ScrubUiAction::Confirm);
                }
            }
            ScrubStep::ConfirmCancel => match code {
                KeyCode::Backspace => {
                    scrub.confirmation_input.pop();
                }
                KeyCode::Char(c) => scrub.confirmation_input.push(c),
                KeyCode::Enter if scrub.confirmation_input == scrub.group_name => {
                    scrub.pending_action = Some(ScrubUiAction::Confirm);
                }
                _ => {}
            },
            ScrubStep::Idle | ScrubStep::Done | ScrubStep::Error => {}
        }
    }

    /// Opens directly at `ReconcileStep::Confirm`, unconditionally --
    /// unlike `open_wizard`/`open_replace`/`open_scrub`, reconcile is not
    /// scoped to a group (`shr-cli`'s `Command::Reconcile` takes no
    /// `--name`), so there is no group-count gate to apply here, and no
    /// group-picker data to read off `self.report` either. No
    /// `pending_action` is auto-fired (unlike `open_scrub`'s Start/Cancel
    /// decision) -- reconcile has nothing to decide, only to confirm.
    fn open_reconcile(&mut self) {
        if self.reconcile.is_some() {
            return;
        }
        self.reconcile = Some(ReconcileView::default());
        self.error = None;
    }

    fn close_reconcile(&mut self) {
        self.reconcile = None;
    }

    /// Only called while `self.reconcile.is_some()`. Same Esc-during-
    /// Executing refusal as `handle_wizard_key`/`handle_replace_key` --
    /// `ReconcileStep::Executing` means `runtime.rs`'s background thread is
    /// mid-way through the real `OrchestrationEngine::reconcile()` call.
    fn handle_reconcile_key(&mut self, code: KeyCode) {
        if code == KeyCode::Esc {
            match self.reconcile.as_ref().map(|r| r.step()) {
                Some(ReconcileStep::Executing) => {
                    self.error = Some(
                        "Reconcile: the operation is running -- cannot close with Esc until it finishes."
                            .to_string(),
                    );
                }
                Some(_) => self.close_reconcile(),
                None => {}
            }
            return;
        }
        let Some(reconcile) = self.reconcile.as_mut() else { return };

        match reconcile.step() {
            ReconcileStep::Confirm => {
                if code == KeyCode::Enter {
                    reconcile.pending_action = Some(ReconcileUiAction::Execute);
                }
            }
            ReconcileStep::Executing | ReconcileStep::Done | ReconcileStep::Error => {}
        }
    }
}

/// Describes a `WizardState` that landed with no wizard modal left to
/// receive it (the background `execute()` thread's real outcome, reported
/// after the operator closed the wizard some other way) -- built from the
/// same `Step`/`error_message`/`result` fields `ui.rs`'s own `Step::Done`/
/// `Step::Error` rendering already reads, so this says nothing the modal
/// itself wouldn't have.
fn wizard_result_message(state: &WizardState) -> String {
    match state.step() {
        Step::Done => match &state.result {
            Some(result) => format!(
                "Add Disk: the background operation finished after the wizard closed -- group `{}` now spans {} band(s).",
                result.name, result.bands.len()
            ),
            None => "Add Disk: the background operation finished after the wizard closed.".to_string(),
        },
        Step::Error => format!(
            "Add Disk: the background operation failed after the wizard closed -- {}",
            state.error_message.as_deref().unwrap_or("(reason unknown)")
        ),
        _ => "Add Disk: a background operation status arrived after the wizard closed.".to_string(),
    }
}

/// The `set_replace_state` counterpart to `wizard_result_message` -- same
/// reasoning, applied to `ReplaceWizardState`'s `Done`/`Error`/other steps.
fn replace_result_message(state: &ReplaceWizardState) -> String {
    match state.step() {
        ReplaceStep::Done => match &state.result {
            Some(result) => format!(
                "Replace Disk: the background operation finished after the wizard closed -- group `{}` now spans {} band(s).",
                result.name, result.bands.len()
            ),
            None => "Replace Disk: the background operation finished after the wizard closed.".to_string(),
        },
        ReplaceStep::Error => format!(
            "Replace Disk: the background operation failed after the wizard closed -- {}",
            state.error_message.as_deref().unwrap_or("(reason unknown)")
        ),
        _ => "Replace Disk: a background operation status arrived after the wizard closed.".to_string(),
    }
}

/// The `set_scrub_state` counterpart. In practice `confirm_start`/
/// `confirm_cancel` run synchronously (see `ScrubUiAction::Confirm`'s doc
/// comment), so this path is not expected to be exercised the way
/// `wizard_result_message`/`replace_result_message` are by their own
/// background threads -- kept for the same "never silently drop a real
/// result" reason, not because a late arrival is expected here.
fn scrub_result_message(state: &ScrubState) -> String {
    match state.step() {
        ScrubStep::Done => "Scrub: the operation finished after the window closed.".to_string(),
        ScrubStep::Error => format!(
            "Scrub: the operation failed after the window closed -- {}",
            state.error_message.as_deref().unwrap_or("(reason unknown)")
        ),
        _ => "Scrub: an operation status arrived after the window closed.".to_string(),
    }
}

/// The `set_reconcile_state` counterpart -- same "never silently drop a
/// real result" reasoning as `scrub_result_message`, its closest analog
/// (also not group-scoped). Reconcile's `Execute` runs on a background
/// thread (unlike scrub's synchronous confirm/cancel), so a late arrival
/// after the modal is closed is a real, reachable path here.
fn reconcile_result_message(state: &ReconcileState) -> String {
    match state.step() {
        ReconcileStep::Done => "Reconcile: the operation finished after the window closed.".to_string(),
        ReconcileStep::Error => format!(
            "Reconcile: the operation failed after the window closed -- {}",
            state.error_message.as_deref().unwrap_or("(reason unknown)")
        ),
        _ => "Reconcile: an operation status arrived after the window closed.".to_string(),
    }
}

// `App`'s side of the Replace Disk wizard and the scrub start/
// cancel controls is pure UI-state wiring, same as `add_disk_wizard` in
// `tests/app.rs` covers for the Add Disk wizard -- no IO, so it's tested
// directly here rather than through `runtime.rs`'s controller/thread
// plumbing.
//
// Lives inside `src/app.rs` (not the external `tests/app.rs` crate) because
// `ReplaceView`/`ScrubView`/`ReplaceAction`/`ScrubUiAction` are defined in
// this module, and this module (`mod app;` in `lib.rs`) is PRIVATE -- a
// `pub` struct inside a private module is only reachable from other modules
// INSIDE this crate (e.g. `ui.rs`, `runtime.rs`), never from an external
// crate like `tests/app.rs`, unless `lib.rs` re-exports it with `pub use`.
// `lib.rs` is out of this task's three owned files, so it cannot be edited
// to add that re-export -- these types can only be tested from inside the
// crate, which is exactly what this module is.
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use shr_command::{DiskStatus, GroupBandStatus, GroupStatus, Health, SmartState, SmartSummary};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ok_smart() -> SmartSummary {
        SmartSummary {
            state: SmartState::Ok,
            temperature_c: None,
            power_on_hours: None,
            pending_sectors: None,
            reallocated_sectors: None,
            uncorrectable_sectors: None,
            nvme_critical_warning: None,
        }
    }

    fn disk(name: &str, id: &str, arrays: Vec<String>, system_disk: bool) -> DiskStatus {
        DiskStatus {
            name: name.to_string(),
            id: Some(id.to_string()),
            size: Some(4_000_000_000_000),
            model: None,
            serial: None,
            rotational: Some(true),
            smart: ok_smart(),
            arrays,
            system_disk,
            system_mounts: if system_disk { vec!["/".to_string()] } else { vec![] },
        }
    }

    fn band(scrub_in_progress: bool) -> GroupBandStatus {
        GroupBandStatus {
            index: 0,
            level: "raid1".into(),
            md_name: "md0".into(),
            md_uuid: None,
            usable_bytes: 4_000_000_000_000,
            resize_pending: false,
            members: vec![],
            member_states: vec![],
            sync: None,
            last_scrub: None,
            scrub_in_progress,
            pending_member_removal: None,
        }
    }

    fn group(name: &str, disks: Vec<String>, bands: Vec<GroupBandStatus>) -> GroupStatus {
        GroupStatus {
            name: name.to_string(),
            mode: "shr".into(),
            layout_version: 1,
            mount_point: "/mnt/shr_data".into(),
            fs_uuid: None,
            vg_name: "shr_vg".into(),
            lv_name: "data".into(),
            compression: "zstd:3".into(),
            usable_bytes: 4_000_000_000_000,
            resize_pending: false,
            disks,
            bands,
        }
    }

    fn empty_report() -> StatusReport {
        StatusReport {
            schema_version: 2,
            health: Health::Healthy,
            disks: vec![],
            arrays: vec![],
            groups: vec![],
            state_path: None,
        }
    }

    /// One existing member ("vda"), one free disk ("vdb"), and one free-
    /// LOOKING-but-system disk ("vdc") -- `vdc` has an empty `arrays` list
    /// just like a genuinely free disk would, so it would show up in the
    /// new-disk picker unless `open_replace` (and `handle_replace_key`)
    /// explicitly account for `system_disk`, same concern as the Add
    /// Disk wizard.
    fn single_group_report_for_replace() -> StatusReport {
        let mut report = empty_report();
        report.disks = vec![
            disk("vda", "ata-EXISTING1", vec!["md0".to_string()], false),
            disk("vdb", "ata-FREE1", vec![], false),
            disk("vdc", "ata-SYSTEM1", vec![], true),
        ];
        report.groups = vec![group("shr1", vec!["ata-EXISTING1".to_string()], vec![band(false)])];
        report
    }

    mod replace_disk_wizard {
        use super::*;

        #[test]
        fn x_opens_replace_and_auto_selects_the_only_group() {
            let mut app = App::new(single_group_report_for_replace());
            assert!(app.replace().is_none());

            app.handle_key(key(KeyCode::Char('x')));

            let replace = app.replace().expect("replace modal should be open");
            assert_eq!(replace.group_name, "shr1");
            assert_eq!(replace.step(), ReplaceStep::Select);
            assert!(!replace.picking_new, "must start on the OLD-member picker, not the new-disk one");
            assert_eq!(replace.old_candidates.len(), 1);
            assert_eq!(replace.old_candidates[0].id, "ata-EXISTING1");
            // "vdc" is a system disk with an empty `arrays` list -- the
            // new-disk list is built from `arrays.is_empty()` alone and
            // still LISTS it (earlier precedent from the Add Disk wizard: shown
            // and marked, never silently hidden), only refusing it at
            // selection time. See the system-disk refusal test below for
            // that actual refusal.
            assert_eq!(replace.new_candidates.len(), 2, "both vdb and vdc (system) are free disks");
            assert!(app.error().is_none());
        }

        #[test]
        fn zero_or_multiple_groups_refuse_to_open_replace_and_report_why_not_silently() {
            let mut app = App::new(empty_report()); // zero groups
            app.handle_key(key(KeyCode::Char('x')));
            assert!(app.replace().is_none());
            assert!(app.error().is_some(), "must explain why, not silently do nothing");

            let mut report = single_group_report_for_replace();
            report.groups.push(report.groups[0].clone());
            report.groups[1].name = "shr2".into();
            let mut app = App::new(report);
            app.handle_key(key(KeyCode::Char('x')));
            assert!(app.replace().is_none());
            assert!(app.error().is_some());
        }

        #[test]
        fn picking_the_old_disk_then_the_new_disk_requests_run_preview_exactly_once() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));

            // Pick the only old candidate ("vda").
            app.handle_key(key(KeyCode::Enter));
            assert!(app.replace().unwrap().picking_new, "must move to the new-disk picker");
            assert_eq!(app.replace().unwrap().selected_old.as_deref(), Some("ata-EXISTING1"));
            assert!(app.take_replace_action().is_none(), "picking only the old disk must not fire yet");

            // Cursor starts on "vdb" (index 0), a genuinely free, non-system
            // disk -- pick it.
            app.handle_key(key(KeyCode::Enter));
            assert_eq!(app.replace().unwrap().selected_new.as_deref(), Some("vdb"));
            assert_eq!(app.take_replace_action(), Some(ReplaceAction::RunPreview));
            assert!(app.take_replace_action().is_none(), "the action must be consumed, not repeated");
        }

        /// The new-disk picker must refuse the system disk the same way
        /// the Add Disk wizard's `SelectDisks` step already does -- never
        /// silently let it through to `runtime.rs` just because the UI
        /// button wasn't visibly disabled.
        #[test]
        fn a_system_disk_cannot_be_picked_as_the_replacement() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));
            app.handle_key(key(KeyCode::Enter)); // pick "vda" as the old disk

            // Move the cursor from "vdb" (index 0) onto "vdc" (index 1), the
            // system disk.
            app.handle_key(key(KeyCode::Down));
            app.handle_key(key(KeyCode::Enter));

            assert!(
                app.replace().unwrap().selected_new.is_none(),
                "The system disk must never end up selected"
            );
            assert!(
                app.replace().unwrap().selection_blocked_reason.is_some(),
                "must say why the selection was refused, not silently do nothing"
            );
            assert!(app.take_replace_action().is_none(), "a refused pick must not request a preview");
        }

        #[test]
        fn replace_confirm_gate_requires_the_exact_group_name_before_execute_is_requested() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));
            app.set_replace_state(ReplaceWizardState { step: Some(ReplaceStep::Confirm), ..Default::default() });

            for ch in "shr".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            assert!(app.take_replace_action().is_none(), "a partial match must not confirm");

            app.handle_key(key(KeyCode::Char('1')));
            assert_eq!(app.replace().unwrap().confirmation_input, "shr1");
            app.handle_key(key(KeyCode::Enter));
            assert_eq!(app.take_replace_action(), Some(ReplaceAction::Execute));
        }

        #[test]
        fn esc_closes_replace_from_the_select_step_without_requesting_anything() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));
            app.handle_key(key(KeyCode::Esc));

            assert!(app.replace().is_none());
            assert!(app.take_replace_action().is_none());
            assert!(!app.should_quit(), "Esc while replace is open must close it, not quit the TUI");
        }

        /// The Replace-Disk counterpart: `ReplaceStep::Executing` means
        /// `runtime.rs`'s background thread is mid-way through the real
        /// `ReplaceDiskController::execute()`. Esc must refuse and say why,
        /// not abandon the modal while real mdadm work is in flight.
        #[test]
        fn esc_is_refused_while_replace_is_executing_and_says_why() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));
            app.set_replace_state(ReplaceWizardState { step: Some(ReplaceStep::Executing), ..Default::default() });

            app.handle_key(key(KeyCode::Esc));

            assert!(app.replace().is_some(), "Esc must not abandon an in-flight destructive operation");
            assert_eq!(app.replace().unwrap().step(), ReplaceStep::Executing);
            assert!(app.error().is_some(), "must say why Esc did nothing, not silently ignore it");
        }

        /// The second half, replace-disk side: a result can still arrive
        /// after the modal is gone (closed some other way) -- it must
        /// surface via the error/message banner, not vanish.
        #[test]
        fn a_late_arriving_replace_result_after_the_modal_is_gone_is_not_silently_dropped() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));
            app.set_replace_state(ReplaceWizardState { step: Some(ReplaceStep::Confirm), ..Default::default() });
            app.handle_key(key(KeyCode::Esc));
            assert!(app.replace().is_none());

            app.set_replace_state(ReplaceWizardState {
                step: Some(ReplaceStep::Error),
                error_message: Some("mdadm replace failed: device busy".to_string()),
                ..Default::default()
            });

            let msg = app.error().expect("A late result must not vanish silently");
            assert!(msg.contains("device busy"), "{msg}");
        }

        #[test]
        fn while_replace_is_open_a_and_s_and_digit_keys_do_not_leak_through() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('x')));

            // 'a' (Add Disk) and 's' (Scrub) must not open a second modal
            // while replace is already open -- only one modal at a time.
            app.handle_key(key(KeyCode::Char('a')));
            assert!(app.wizard().is_none(), "a second modal must never open over an existing one");
            app.handle_key(key(KeyCode::Char('s')));
            assert!(app.scrub().is_none());

            assert_eq!(app.tab(), Tab::Dashboard);
            app.handle_key(key(KeyCode::Char('4'))); // would normally jump to the Groups tab
            assert_eq!(app.tab(), Tab::Dashboard, "replace key handling must consume the key, not fall through");
            assert!(app.replace().is_some(), "replace itself must still be open");
        }
    }

    mod scrub_controls {
        use super::*;

        #[test]
        fn s_opens_scrub_and_requests_start_when_no_scrub_is_running() {
            let mut app = App::new(single_group_report_for_replace()); // band(false) == not running
            app.handle_key(key(KeyCode::Char('s')));

            let scrub = app.scrub().expect("scrub modal should be open");
            assert_eq!(scrub.group_name, "shr1");
            assert_eq!(app.take_scrub_action(), Some(ScrubUiAction::RequestStart));
        }

        #[test]
        fn s_opens_scrub_and_requests_cancel_when_a_scrub_is_running() {
            let mut report = single_group_report_for_replace();
            report.groups[0].bands = vec![band(true)]; // scrub_in_progress
            let mut app = App::new(report);
            app.handle_key(key(KeyCode::Char('s')));

            assert!(app.scrub().is_some());
            assert_eq!(app.take_scrub_action(), Some(ScrubUiAction::RequestCancel));
        }

        #[test]
        fn zero_or_multiple_groups_refuse_to_open_scrub_and_report_why_not_silently() {
            let mut app = App::new(empty_report());
            app.handle_key(key(KeyCode::Char('s')));
            assert!(app.scrub().is_none());
            assert!(app.error().is_some());

            let mut report = single_group_report_for_replace();
            report.groups.push(report.groups[0].clone());
            report.groups[1].name = "shr2".into();
            let mut app = App::new(report);
            app.handle_key(key(KeyCode::Char('s')));
            assert!(app.scrub().is_none());
            assert!(app.error().is_some());
        }

        #[test]
        fn enter_at_confirm_start_fires_the_confirm_action() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('s')));
            app.take_scrub_action(); // consume the auto-fired RequestStart
            app.set_scrub_state(ScrubState { step: Some(ScrubStep::ConfirmStart), ..Default::default() });

            app.handle_key(key(KeyCode::Enter));
            assert_eq!(app.take_scrub_action(), Some(ScrubUiAction::Confirm));
        }

        #[test]
        fn confirm_cancel_requires_the_exact_group_name_before_firing_confirm() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('s')));
            app.take_scrub_action(); // consume the auto-fired action
            app.set_scrub_state(ScrubState { step: Some(ScrubStep::ConfirmCancel), ..Default::default() });

            for ch in "shr".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            assert!(app.take_scrub_action().is_none(), "a partial match must not confirm cancelling a live scrub");

            app.handle_key(key(KeyCode::Char('1')));
            assert_eq!(app.scrub().unwrap().confirmation_input, "shr1");
            app.handle_key(key(KeyCode::Enter));
            assert_eq!(app.take_scrub_action(), Some(ScrubUiAction::Confirm));
        }

        /// Unlike the Add Disk/Replace Disk wizards, `scrub::Step` has no
        /// `Executing` variant at all (see `ScrubUiAction::Confirm`'s doc
        /// comment) -- so, unlike `esc_is_refused_while_executing_and_says_why`
        /// above, there is no step where Esc should ever be refused here.
        #[test]
        fn esc_always_closes_scrub_even_from_confirm_cancel() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('s')));
            app.set_scrub_state(ScrubState { step: Some(ScrubStep::ConfirmCancel), ..Default::default() });

            app.handle_key(key(KeyCode::Esc));

            assert!(app.scrub().is_none());
            assert!(!app.should_quit());
        }

        #[test]
        fn while_scrub_is_open_a_and_x_and_digit_keys_do_not_leak_through() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('s')));

            app.handle_key(key(KeyCode::Char('a')));
            assert!(app.wizard().is_none());
            app.handle_key(key(KeyCode::Char('x')));
            assert!(app.replace().is_none());

            assert_eq!(app.tab(), Tab::Dashboard);
            app.handle_key(key(KeyCode::Char('4')));
            assert_eq!(app.tab(), Tab::Dashboard, "scrub key handling must consume the key, not fall through");
            assert!(app.scrub().is_some());
        }
    }

    // `resize_pending` (rendered as a warning badge on the Groups/Bands/
    // Fs tabs -- `ui.rs`) had no way to act on it from the TUI before this.
    // `shr-rs reconcile` is the fix. Unlike Add Disk/Replace Disk/Scrub,
    // reconcile is NOT scoped to one group -- `shr-cli`'s `Command::
    // Reconcile` variant takes no `--name` at all, it walks every group's
    // pending bands in one call -- so `open_reconcile` has no group-count
    // gate the way `open_wizard`/`open_replace`/`open_scrub` do.
    mod reconcile_controls {
        use super::*;

        #[test]
        fn f_opens_reconcile_directly_at_confirm_with_no_group_gate() {
            // Zero groups -- unlike scrub/replace/wizard this must still
            // open, because reconcile does not target a specific group.
            let mut app = App::new(empty_report());
            app.handle_key(key(KeyCode::Char('f')));

            let reconcile = app.reconcile().expect("reconcile modal should open even with zero groups");
            assert_eq!(reconcile.step(), ReconcileStep::Confirm);
            assert!(app.error().is_none());
            // No `pending_action` fired on open -- unlike scrub's auto-fired
            // RequestStart/RequestCancel, reconcile has nothing to decide;
            // it waits for the operator's explicit Enter (constraint: a
            // single explicit confirm, not a bare keypress that immediately
            // acts).
            assert!(app.take_reconcile_action().is_none());
        }

        #[test]
        fn enter_at_confirm_fires_execute_exactly_once() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('f')));

            app.handle_key(key(KeyCode::Enter));
            assert_eq!(app.take_reconcile_action(), Some(ReconcileUiAction::Execute));
            assert!(app.take_reconcile_action().is_none(), "the action must be consumed, not repeated");
        }

        #[test]
        fn esc_closes_reconcile_from_confirm_without_requesting_anything() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('f')));
            app.handle_key(key(KeyCode::Esc));

            assert!(app.reconcile().is_none());
            assert!(app.take_reconcile_action().is_none());
            assert!(!app.should_quit(), "Esc while reconcile is open must close it, not quit the TUI");
        }

        /// The reconcile counterpart: `ReconcileStep::Executing` means
        /// `runtime.rs`'s background thread is mid-way through the real
        /// `OrchestrationEngine::reconcile()` call (constraint 5: same
        /// background-thread treatment as `ReplaceAction::Execute`, since a
        /// real `btrfs filesystem resize` can take a while). Esc must refuse
        /// and say why, not abandon the modal mid-run.
        #[test]
        fn esc_is_refused_while_reconcile_is_executing_and_says_why() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('f')));
            app.set_reconcile_state(ReconcileState { step: Some(ReconcileStep::Executing), ..Default::default() });

            app.handle_key(key(KeyCode::Esc));

            assert!(app.reconcile().is_some(), "Esc must not abandon an in-flight reconcile");
            assert_eq!(app.reconcile().unwrap().step(), ReconcileStep::Executing);
            assert!(app.error().is_some(), "must say why Esc did nothing, not silently ignore it");
        }

        /// The second half: a background result can still arrive after the
        /// modal is gone (closed some other way) -- it must surface via the
        /// error/message banner, not vanish.
        #[test]
        fn a_late_arriving_reconcile_result_after_the_modal_is_gone_is_not_silently_dropped() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('f')));
            app.handle_key(key(KeyCode::Esc)); // closes from Confirm (not Executing -- see the test above)
            assert!(app.reconcile().is_none());

            app.set_reconcile_state(ReconcileState {
                step: Some(ReconcileStep::Error),
                error_message: Some("mdadm --detail failed: /dev/md0: No such file or directory".to_string()),
                ..Default::default()
            });

            let msg = app.error().expect("A late result must not vanish silently");
            assert!(msg.contains("No such file or directory"), "{msg}");
        }

        #[test]
        fn while_reconcile_is_open_a_and_x_and_s_and_digit_keys_do_not_leak_through() {
            let mut app = App::new(single_group_report_for_replace());
            app.handle_key(key(KeyCode::Char('f')));

            app.handle_key(key(KeyCode::Char('a')));
            assert!(app.wizard().is_none());
            app.handle_key(key(KeyCode::Char('x')));
            assert!(app.replace().is_none());
            app.handle_key(key(KeyCode::Char('s')));
            assert!(app.scrub().is_none());

            assert_eq!(app.tab(), Tab::Dashboard);
            app.handle_key(key(KeyCode::Char('4')));
            assert_eq!(app.tab(), Tab::Dashboard, "reconcile key handling must consume the key, not fall through");
            assert!(app.reconcile().is_some());
        }
    }
}
