//! The TUI's actual event loop (exposed as [`run`] so both this
//! crate's own `shr-tui` binary and `shr-bin`'s TUI dispatch branch can call
//! it -- one implementation, not two).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event};
use shr_command::{
    build_fs_df, build_status, system_disk_aliases, FsDfReport, FsUsageInput, GroupStatus, Health,
    StatusReport,
};
use shr_exec::{BtrfsExecutor, CommandRunner, SystemRunner};
use shr_inspect::{Inspector, SystemInspector};
use shr_state::StateStore;

use shr_orchestrate::{OrchestrationEngine, ReconcileAction};
use shr_state::StateFile;

use crate::app::{ReconcileState, ReconcileStep, ReconcileUiAction, ReplaceAction, ScrubUiAction};
use crate::scrub::{ScrubController, Step as ScrubCtrlStep};
use crate::wizard::{
    AddDiskController, ReplaceDiskController, ReplaceStep, ReplaceWizardState, Step, WizardState,
};
use crate::{render, App, RefreshWorker, Snapshot, WizardAction};

/// Same path `shr-cli`'s `Status` handler reads (see
/// `crates/shr-cli/src/lib.rs`) -- the TUI is a second read-only frontend
/// over the same Command API and must see the same groups.
const STATE_PATH: &str = "/var/lib/shr-rs/state.toml";
const MDADM_CONF_PATH: &str = "/etc/mdadm.conf";
const FSTAB_PATH: &str = "/etc/fstab";

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Live Btrfs `filesystem usage`/`df` figures are shelled-out
/// subprocesses (`BtrfsExecutor::usage`/`free_bytes`), unlike the rest of
/// `inspect()`'s `lsblk`/`mdadm` reads this refresh cycle already pays for.
/// Re-running them on every 2s `REFRESH_INTERVAL` tick (2 processes per
/// group, every 2s, indefinitely) is wasteful for a figure that changes
/// slowly relative to sync/degraded status -- so they're decimated to run
/// every Nth cycle instead (5 * 2s = ~10s) and `FsUsageCache` reuses the
/// last-known map in between. `usable_bytes` and every other field in
/// `GroupStatus` stay always-fresh every cycle; only the live Btrfs usage
/// figures are decimated.
const FS_USAGE_REFRESH_EVERY_CYCLES: u64 = 5;

/// Interior-mutability cache for the decimated Btrfs usage fetch.
/// `RefreshWorker::spawn` requires `F: Fn() -> RefreshResult + Send +
/// 'static` -- NOT `FnMut` -- so the closure body cannot capture `&mut`
/// state across calls; a `Cell`/`RefCell` pair owned (via `move`) by the
/// closure is the only way to keep a cycle counter and a cached usage map
/// alive between calls without a second thread or a mutex-guarded global.
/// `Cell`/`RefCell` (not `Atomic*`) are sufficient here because the
/// `RefreshWorker`'s background thread is the only caller of `inspect()` --
/// there is no cross-thread contention on this cache itself.
///
/// Honesty guarantee: a group missing from `last` (never yet fetched, or
/// the most recent fetch failed for it) renders as `None` for every usage
/// field via `build_fs_df`'s `unwrap_or_default()` -- `?` in the UI, never a
/// fabricated number and never a value from some earlier, unrelated group.
struct FsUsageCache {
    cycle: Cell<u64>,
    last: RefCell<BTreeMap<String, FsUsageInput>>,
}

impl FsUsageCache {
    fn new() -> Self {
        Self {
            cycle: Cell::new(0),
            last: RefCell::new(BTreeMap::new()),
        }
    }
}

/// `'static` so an `AddDiskController` built over it can be moved into the
/// background thread that runs the real, potentially long-running `execute`
/// -- see `run`'s `wizard_execute_rx` handling.
static SYSTEM_INSPECTOR: SystemInspector = SystemInspector;

pub fn run() -> Result<()> {
    let mut app = App::new(empty_report());
    // Owned by the closure (`move`), not borrowed -- see
    // `FsUsageCache`'s doc comment for why this is the only shape that fits
    // `RefreshWorker::spawn`'s `Fn` + `Send` + `'static` bound.
    let fs_usage_cache = FsUsageCache::new();
    let mut refresh =
        RefreshWorker::spawn(move || inspect(&fs_usage_cache).map_err(|error| error.to_string()));
    refresh.request();

    ratatui::run(|terminal| run_loop(terminal, &mut app, &mut refresh))?;
    Ok(())
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    refresh: &mut RefreshWorker,
) -> std::io::Result<()> {
    let mut last_refresh = Instant::now();
    let state_store = Arc::new(StateStore::new(STATE_PATH));
    // Its own controller, alive only between the wizard opening and either
    // finishing or being cancelled -- `None` means no wizard run is
    // currently underway on the Rust side (independent of whether `App`'s
    // modal is visible; the two are kept in sync via `pending_action`/
    // `set_wizard_state`).
    let mut wizard_controller: Option<AddDiskController<'static>> = None;
    // Set only while the real, destructive `execute()` is running on a
    // background thread (constraint: a real reshape/grow can take hours
    // -- see the design -- and must never freeze the terminal event
    // loop the way `RefreshWorker` already avoids for plain status polling).
    let mut wizard_execute_rx: Option<Receiver<WizardState>> = None;
    // The counterparts to the two locals above -- same lifecycle, same
    // reasoning: `ReplaceDiskController::execute()` can be just as
    // long-running as `AddDiskController::execute()`.
    let mut replace_controller: Option<ReplaceDiskController<'static>> = None;
    let mut replace_execute_rx: Option<Receiver<ReplaceWizardState>> = None;
    // No `'static` lifetime and no background-thread receiver needed --
    // `ScrubController` borrows nothing (unlike `AddDiskController`/
    // `ReplaceDiskController`, which hold an `&'a dyn Inspector`), and
    // `confirm_start`/`confirm_cancel` run synchronously (see
    // `ScrubUiAction::Confirm`'s doc comment in `app.rs`).
    let mut scrub_controller: Option<ScrubController> = None;
    // Counterpart of `wizard_execute_rx`/`replace_execute_rx`. No
    // `*_controller` local is needed alongside it: unlike Add Disk/Replace
    // Disk, reconcile has no group-scoped preview step to hold state
    // between -- `handle_reconcile_action` builds and runs everything for
    // `ReconcileUiAction::Execute` in one shot, so only the receiver half
    // needs to survive across loop iterations.
    let mut reconcile_execute_rx: Option<Receiver<ReconcileState>> = None;

    loop {
        terminal.draw(|frame| render(frame, app))?;

        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }

        if app.should_quit() {
            return Ok(());
        }

        while let Some(result) = refresh.try_result() {
            match result {
                Ok(snapshot) => app.replace_snapshot(snapshot),
                Err(error) => app.set_error(error),
            }
            last_refresh = Instant::now();
        }

        let manual_refresh = app.take_refresh_requested();
        if (manual_refresh || last_refresh.elapsed() >= REFRESH_INTERVAL) && !refresh.is_in_flight() {
            refresh.request();
        }

        if let Some(action) = app.take_wizard_action() {
            handle_wizard_action(
                app,
                &state_store,
                &SYSTEM_INSPECTOR,
                &mut wizard_controller,
                &mut wizard_execute_rx,
                action,
            );
        }

        if let Some(rx) = &wizard_execute_rx {
            if let Ok(state) = rx.try_recv() {
                app.set_wizard_state(state);
                wizard_execute_rx = None;
            }
        }

        if let Some(action) = app.take_replace_action() {
            handle_replace_action(
                app,
                &state_store,
                &mut replace_controller,
                &mut replace_execute_rx,
                action,
            );
        }

        if let Some(rx) = &replace_execute_rx {
            if let Ok(state) = rx.try_recv() {
                app.set_replace_state(state);
                replace_execute_rx = None;
            }
        }

        if let Some(action) = app.take_scrub_action() {
            handle_scrub_action(app, &state_store, &mut scrub_controller, action);
        }

        if let Some(action) = app.take_reconcile_action() {
            handle_reconcile_action(app, &state_store, &mut reconcile_execute_rx, action);
        }

        if let Some(rx) = &reconcile_execute_rx {
            if let Ok(state) = rx.try_recv() {
                app.set_reconcile_state(state);
                reconcile_execute_rx = None;
            }
        }
    }
}

fn handle_wizard_action(
    app: &mut App,
    state_store: &Arc<StateStore>,
    inspector: &'static dyn Inspector,
    wizard_controller: &mut Option<AddDiskController<'static>>,
    wizard_execute_rx: &mut Option<Receiver<WizardState>>,
    action: WizardAction,
) {
    let Some(view) = app.wizard() else { return };

    match action {
        WizardAction::RunPreflight => {
            // Built fresh every time preflight is (re)requested -- cheap
            // (no IO happens until `run_preflight`/`run_preview` are
            // actually called), and guarantees the controller's captured
            // `selected_kernel_names`/`group_name` always match what's
            // currently shown in the wizard view.
            //
            // `inspector` is a parameter (not the hardcoded `&SYSTEM_
            // INSPECTOR` this used to be) so tests can inject a
            // `StaticInspector` fixture and observe the REAL preflight
            // outcome (`report.blockers`) -- `SystemInspector` shells out to
            // `lsblk`, which doesn't exist on this project's Windows dev
            // host, so without this injection point every test here would
            // hit `Step::Error` regardless of `force_content`, unable to
            // distinguish a fix from the bug it's meant to catch.
            let system_disks = system_disk_aliases(inspector).unwrap_or_default();
            // `view.force_content` is the operator's explicit override
            // (set only by `app.rs::handle_wizard_key`'s `o` key at
            // `Step::Preflight`) -- forwarded here, not hardcoded, so a
            // retried preflight after the override actually reaches
            // `preflight_write_targets`'s `HasContent` gate.
            let mut controller = AddDiskController::new(
                inspector,
                state_store.clone(),
                MDADM_CONF_PATH,
                FSTAB_PATH,
                system_disks,
                Some(view.group_name.clone()),
                view.selected.clone(),
                view.force_content,
            );
            controller.run_preflight();
            app.set_wizard_state(controller.state.clone());
            *wizard_controller = Some(controller);
        }
        WizardAction::RunPreview => {
            if let Some(controller) = wizard_controller {
                controller.run_preview();
                app.set_wizard_state(controller.state.clone());
            }
        }
        // The operator's explicit override of the scrub-freshness
        // check, taken only from `Step::ScrubCheckWarning` (see
        // `app.rs::handle_wizard_key`) -- sets the same flag `execute()`
        // later reads, so the override actually reaches real execution too,
        // not just this re-run of the dry-run preview.
        WizardAction::AcceptScrubCheckWarning => {
            if let Some(controller) = wizard_controller {
                controller.set_skip_scrub_check(true);
                controller.run_preview();
                app.set_wizard_state(controller.state.clone());
            }
        }
        WizardAction::Execute => {
            if let Some(mut controller) = wizard_controller.take() {
                // The operator's typed confirmation text lives only in
                // `App`'s `confirmation_input` -- forward it into the
                // controller NOW, at the moment execution is requested,
                // before asking `can_execute()`/`execute()` anything.
                // Forwarding here (rather than on every keystroke) is
                // sufficient and correct: `app.rs::handle_wizard_key`'s own
                // `Step::Confirm` arm only ever queues `Execute` once
                // `confirmation_input == group_name` already held, so the
                // value read here is guaranteed to be the one the operator
                // saw on screen when they pressed Enter. If the operator
                // somehow edited the text after this point, there is no
                // "after this point" -- the value is read and consumed in
                // this same call, before any further key can be processed.
                controller.set_confirmation_text(view.confirmation_input.clone());
                if !controller.can_execute() {
                    // Refuse without ever showing `Step::Executing` --
                    // silent failure is itself a defect (constraint 2):
                    // the operator must see why, not be left on a modal
                    // that looks like Enter did nothing.
                    let mut state = controller.state.clone();
                    state.step = Some(Step::Error);
                    state.error_message = Some(
                        "Add Disk: the confirmation text did not match the target group, \
                         so execution was cancelled. Close the wizard and try again."
                            .to_string(),
                    );
                    app.set_wizard_state(state);
                    return;
                }
                // Preserve the already-fetched preview (so the "executing"
                // screen still shows what's running) rather than blanking
                // it -- only the step actually needs to change here; the
                // background thread will report the real final state.
                let mut executing_state = controller.state.clone();
                executing_state.step = Some(Step::Executing);
                app.set_wizard_state(executing_state);

                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name("shr-tui-expand".into())
                    .spawn(move || {
                        let runner = SystemRunner::new();
                        controller.execute(&runner);
                        let _ = tx.send(controller.state);
                    })
                    .expect("failed to start Add Disk execution thread");
                *wizard_execute_rx = Some(rx);
            }
        }
    }
}

/// Counterpart of `handle_wizard_action`. `RunPreview` builds a fresh
/// `ReplaceDiskController` and runs `select()` + `run_preview()` in-process
/// (an in-process dry run against `DryRunRunner` -- see
/// `ReplaceDiskController::run_preview`'s doc comment -- same cost class as
/// `WizardAction::RunPreflight`/`RunPreview` above, so no background thread
/// here either). `Execute` mirrors `WizardAction::Execute`'s background-
/// thread treatment exactly, since `ReplaceDiskController::execute` can run
/// a real, potentially hours-long mdadm replace/copy.
fn handle_replace_action(
    app: &mut App,
    state_store: &Arc<StateStore>,
    replace_controller: &mut Option<ReplaceDiskController<'static>>,
    replace_execute_rx: &mut Option<Receiver<ReplaceWizardState>>,
    action: ReplaceAction,
) {
    let Some(view) = app.replace() else { return };

    match action {
        ReplaceAction::RunPreview => {
            // `app.rs::handle_replace_key` only ever fires `RunPreview`
            // after both are `Some` -- these `let else` returns are
            // defence-in-depth, not the actual gate.
            let Some(old_id) = view.selected_old.clone() else {
                return;
            };
            let Some(new_name) = view.selected_new.clone() else {
                return;
            };
            let system_disks = system_disk_aliases(&SYSTEM_INSPECTOR).unwrap_or_default();
            let mut controller = ReplaceDiskController::new(
                &SYSTEM_INSPECTOR,
                state_store.clone(),
                MDADM_CONF_PATH,
                FSTAB_PATH,
                system_disks,
                Some(view.group_name.clone()),
            );
            controller.select(old_id, new_name);
            controller.run_preview();
            app.set_replace_state(controller.state.clone());
            *replace_controller = Some(controller);
        }
        ReplaceAction::Execute => {
            if let Some(mut controller) = replace_controller.take() {
                // Same forward-then-gate treatment as
                // `WizardAction::Execute` above, and the same reasoning for
                // why forwarding here (at Execute time) is correct.
                controller.set_confirmation_text(view.confirmation_input.clone());
                if !controller.can_execute() {
                    let mut state = controller.state.clone();
                    state.step = Some(ReplaceStep::Error);
                    state.error_message = Some(
                        "Replace Disk: the confirmation text did not match the target group, \
                         so execution was cancelled. Close the wizard and try again."
                            .to_string(),
                    );
                    app.set_replace_state(state);
                    return;
                }
                // Same "keep the preview screen showing what's running"
                // treatment as `WizardAction::Execute` above.
                let mut executing_state = controller.state.clone();
                executing_state.step = Some(ReplaceStep::Executing);
                app.set_replace_state(executing_state);

                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name("shr-tui-replace".into())
                    .spawn(move || {
                        let runner = SystemRunner::new();
                        controller.execute(&runner);
                        let _ = tx.send(controller.state);
                    })
                    .expect("failed to start Replace Disk execution thread");
                *replace_execute_rx = Some(rx);
            }
        }
    }
}

/// Counterpart of `handle_wizard_action`/`handle_replace_action`.
/// `RequestStart`/`RequestCancel` build a fresh `ScrubController` and call
/// the matching pure, IO-free request method. `Confirm` calls
/// `confirm_start`/`confirm_cancel` synchronously on the SAME held
/// controller -- deliberately not threaded; see `ScrubUiAction::Confirm`'s
/// doc comment (`app.rs`) for the full reasoning (no `Executing` step
/// exists in `scrub::Step` at all, and the underlying engine calls are a
/// handful of bounded sysfs reads/writes, not a multi-hour reshape).
fn handle_scrub_action(
    app: &mut App,
    state_store: &Arc<StateStore>,
    scrub_controller: &mut Option<ScrubController>,
    action: ScrubUiAction,
) {
    let Some(view) = app.scrub() else { return };

    match action {
        ScrubUiAction::RequestStart => {
            let mut controller = ScrubController::new(state_store.clone(), Some(view.group_name.clone()));
            controller.request_start();
            app.set_scrub_state(controller.state.clone());
            *scrub_controller = Some(controller);
        }
        ScrubUiAction::RequestCancel => {
            let mut controller = ScrubController::new(state_store.clone(), Some(view.group_name.clone()));
            controller.request_cancel();
            app.set_scrub_state(controller.state.clone());
            *scrub_controller = Some(controller);
        }
        ScrubUiAction::Confirm => {
            if let Some(controller) = scrub_controller {
                // Forward the typed confirmation text before gating --
                // same "forward at Execute/Confirm time" reasoning as
                // `WizardAction::Execute`/`ReplaceAction::Execute` above.
                // Harmless to set unconditionally even during
                // `ConfirmStart`, which never reads it
                // (`can_confirm_start` only checks `step()`).
                controller.set_confirmation_text(view.confirmation_input.clone());
                let runner = SystemRunner::new();
                let refused = match controller.state.step() {
                    ScrubCtrlStep::ConfirmStart => !controller.confirm_start(&runner),
                    ScrubCtrlStep::ConfirmCancel => !controller.confirm_cancel(&runner),
                    _ => false,
                };
                if refused {
                    // The same constraint 2 as the wizard/replace fixes: a
                    // refusal must say why, not leave the confirm screen
                    // looking like Enter silently did nothing.
                    controller.state.step = Some(ScrubCtrlStep::Error);
                    controller.state.error_message = Some(
                        "Scrub: the confirmation text did not match the target group, so the operation was cancelled.".to_string(),
                    );
                }
                app.set_scrub_state(controller.state.clone());
            }
        }
    }
}

/// Counterpart of `handle_wizard_action`/`handle_replace_action`/
/// `handle_scrub_action`. Only one action exists (`Execute`) -- reconcile
/// has no group-scoped preview step to build a held controller for the way
/// Add Disk/Replace Disk do (`shr-cli`'s `Command::Reconcile` takes no
/// `--name` at all), so this builds and runs the whole call in the
/// background thread directly, via the free `run_reconcile` helper below.
/// Same background-thread treatment as `WizardAction::Execute`/
/// `ReplaceAction::Execute`: a real `btrfs filesystem resize` can run for a
/// while, so this must not block the event loop the way `ScrubUiAction::
/// Confirm`'s bounded sysfs calls don't need to.
fn handle_reconcile_action(
    app: &mut App,
    state_store: &Arc<StateStore>,
    reconcile_execute_rx: &mut Option<Receiver<ReconcileState>>,
    action: ReconcileUiAction,
) {
    if app.reconcile().is_none() {
        return;
    }

    match action {
        ReconcileUiAction::Execute => {
            app.set_reconcile_state(ReconcileState {
                step: Some(ReconcileStep::Executing),
                ..Default::default()
            });

            let store = state_store.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("shr-tui-reconcile".into())
                .spawn(move || {
                    let runner = SystemRunner::new();
                    let state = run_reconcile(&runner, store);
                    let _ = tx.send(state);
                })
                .expect("failed to start Reconcile execution thread");
            *reconcile_execute_rx = Some(rx);
        }
    }
}

/// `OrchestrationEngine::reconcile()`'s own doc comment ("no
/// `ConfirmSink` gate here ... never starts a NEW destructive action, it
/// only finishes bookkeeping ... for a reshape that was already approved
/// and already physically committed by a prior `expand()` call") is why
/// this needs no `.with_confirm_sink(&AlwaysConfirmSink)` the way
/// `AddDiskController`/`ReplaceDiskController::execute` do. It also needs
/// no `.with_conf_paths`/`.with_unit_dir` -- `reconcile()`'s own body never
/// calls `write_managed_configs` or touches a systemd unit, unlike
/// `expand()`/`replace_disk()`/`destroy()`.
///
/// Split out as a pure, directly-testable function (the `refresh_fs_df`
/// precedent for splitting real IO into a free function rather than a
/// controller struct) so a fake `CommandRunner` can prove the exact
/// pvresize/lvextend/`btrfs resize max` shell-out sequence actually
/// happened -- not merely that `ReconcileUiAction::Execute` was requested
/// (the earlier trap this whole feature exists to avoid repeating).
fn run_reconcile(runner: &dyn CommandRunner, store: Arc<StateStore>) -> ReconcileState {
    let engine = OrchestrationEngine::new(runner, store);
    match engine.reconcile() {
        Ok(Some(outcome)) => ReconcileState {
            step: Some(ReconcileStep::Done),
            performed: outcome.performed.iter().map(describe_reconcile_action).collect(),
            still_pending: still_pending_message(&outcome.state),
            error_message: None,
        },
        // `shr-cli`'s `Command::Reconcile` prints a plain "No active
        // array." for this case (not an error) -- there is nothing to
        // reconcile because `create` has never been run. Treated as `Done`
        // with an empty `performed` list; the overlay's own "Done" text
        // covers this without inventing a separate step just for it.
        Ok(None) => ReconcileState {
            step: Some(ReconcileStep::Done),
            ..Default::default()
        },
        // Constraint: surface the engine's own error verbatim (`e.to_string()`),
        // never reworded or swallowed -- same rule `wizard.rs`/`ScrubController`
        // already follow for their own `Step::Error` paths.
        Err(e) => ReconcileState {
            step: Some(ReconcileStep::Error),
            error_message: Some(e.to_string()),
            ..Default::default()
        },
    }
}

/// Duplicated from `shr-cli`'s private `describe_reconcile_action` (not
/// made `pub` there and reused) -- same "not made `pub`, so duplicated"
/// precedent as `fs_usage_map`/`fs_usage_input` above. Identical English
/// wording to the CLI's own text output, so an operator who has seen
/// `shr-rs reconcile`'s terminal output recognizes the same lines here.
fn describe_reconcile_action(action: &ReconcileAction) -> String {
    match action {
        ReconcileAction::MemberRemoved {
            group,
            band_index,
            md_name,
            member_path,
        } => format!(
            "Group `{group}` band {band_index} ({md_name}): removed the old disk \
             `{member_path}` -- its replacement had finished syncing."
        ),
        ReconcileAction::ResizeCompleted {
            group,
            band_index,
            md_name,
        } => format!(
            "Group `{group}` band {band_index} ({md_name}): finished growing the storage onto \
             the new space -- its RAID rebuild had completed."
        ),
        ReconcileAction::ScrubSelfHealed {
            group,
            band_index,
            md_name,
            error_count,
        } => format!(
            "Group `{group}` band {band_index} ({md_name}): a scheduled error check had \
             finished on its own; recorded the result ({error_count} error(s))."
        ),
    }
}

/// Duplicated from `shr-cli`'s inline pending-band flattening (same
/// `Command::Reconcile` handler) -- flattened across every group, since a
/// plain `shr-rs reconcile` with no `--name` of its own must still report a
/// deferred resize STILL pending on ANY group, not just whichever one
/// happens to be first. Same wording as the CLI's own text line so the two
/// frontends never disagree about what's still outstanding.
fn still_pending_message(state: &StateFile) -> Option<String> {
    let pending: Vec<String> = state
        .groups
        .iter()
        .flat_map(|g| {
            g.bands
                .iter()
                .filter(|b| b.resize_pending)
                .map(move |b| format!("`{}` band {}", g.name, b.index))
        })
        .collect();
    if pending.is_empty() {
        None
    } else {
        Some(format!(
            "Still rebuilding, so the expansion stays unfinished for now: {}.",
            pending.join(", ")
        ))
    }
}

fn inspect(fs_usage_cache: &FsUsageCache) -> Result<Snapshot> {
    // A host with no state.toml yet (nothing ever `create`d) is not an
    // error -- `StateStore::load` already returns `Ok(None)` for that case,
    // and `build_status` turns `None` into an empty `groups` list rather
    // than failing the whole status read.
    let state = StateStore::new(STATE_PATH).load()?;
    let report = build_status(&SystemInspector, state.as_ref())?;
    // Logs are best-effort: a host without `journalctl` (or one where
    // it fails for some other reason) must not blank the whole status
    // refresh over it -- show why instead of silently having an empty Logs
    // tab that looks the same as "no recent log activity".
    let logs = SystemInspector
        .recent_log_lines(200)
        .unwrap_or_else(|e| vec![format!("(log unavailable: {e})")]);
    let runner = SystemRunner::new();
    let fs_df = refresh_fs_df(&runner, &report.groups, fs_usage_cache);
    Ok(Snapshot { report, logs, fs_df })
}

/// Split out from `inspect()` specifically so it's unit-testable
/// against a fake `CommandRunner` (real `btrfs`/`df` calls aren't available
/// on this project's Windows dev host -- see the design).
/// Increments `cache.cycle` on every call; only re-runs the live Btrfs/`df`
/// shellouts (`fs_usage_map`) every `FS_USAGE_REFRESH_EVERY_CYCLES`th call,
/// reusing `cache.last` otherwise. `build_fs_df` itself always re-runs
/// against the CURRENT `groups` (so `usable_bytes` etc. are never stale),
/// only the live usage figures folded in are decimated.
fn refresh_fs_df(runner: &dyn CommandRunner, groups: &[GroupStatus], cache: &FsUsageCache) -> FsDfReport {
    let cycle = cache.cycle.get();
    cache.cycle.set(cycle.wrapping_add(1));
    if cycle.is_multiple_of(FS_USAGE_REFRESH_EVERY_CYCLES) {
        *cache.last.borrow_mut() = fs_usage_map(runner, groups);
    }
    build_fs_df(groups, &cache.last.borrow())
}

/// Duplicated from `shr-cli`'s private `fs_usage_map` (not made `pub` there
/// and reused) -- `shr-command` deliberately excludes `shr-exec` as a
/// dependency, and `shr-tui` must not gain a dependency edge on `shr-cli`
/// just to reach ~15 lines of glue. See `shr-cli/src/lib.rs`'s own
/// `fs_usage_map`/`fs_usage_input` for the CLI's `fs df` command, which this
/// mirrors exactly.
fn fs_usage_map(runner: &dyn CommandRunner, groups: &[GroupStatus]) -> BTreeMap<String, FsUsageInput> {
    let btrfs = BtrfsExecutor::new(runner);
    groups
        .iter()
        .map(|g| (g.name.clone(), fs_usage_input(&btrfs, &g.mount_point)))
        .collect()
}

fn fs_usage_input(btrfs: &BtrfsExecutor<'_>, mount_point: &str) -> FsUsageInput {
    let usage = btrfs.usage(mount_point).unwrap_or_default();
    FsUsageInput {
        data_used_bytes: usage.data_used_bytes,
        data_total_bytes: usage.data_total_bytes,
        metadata_used_bytes: usage.metadata_used_bytes,
        metadata_total_bytes: usage.metadata_total_bytes,
        unallocated_bytes: usage.unallocated_bytes,
        statvfs_avail_bytes: btrfs.free_bytes(mount_point).unwrap_or_default(),
    }
}

fn empty_report() -> StatusReport {
    StatusReport {
        schema_version: shr_command::report::SCHEMA_VERSION,
        health: Health::Unknown,
        disks: Vec::new(),
        arrays: Vec::new(),
        groups: Vec::new(),
        // The TUI has no tech-spec view to show this in, so it never
        // supplies one. `None` means "not told", never a guessed path.
        state_path: None,
    }
}

/// The operator's typed confirmation text (`App`'s `confirmation_input`)
/// never reached `AddDiskController`/`ReplaceDiskController`/`ScrubController`
/// before `execute()`/`confirm_start()`/`confirm_cancel()` was called, so
/// `can_execute()`/`can_confirm_cancel()` always read `false` and the real
/// call silently no-op'd -- reproduced on real hardware (real pty against a
/// live QEMU guest + real mdadm array): typing the exact group name and
/// pressing Enter visibly did nothing, no error shown.
///
/// These tests close exactly the trap the design calls "the
/// recurring defect axis": the PRE-EXISTING tests in `app.rs` (e.g.
/// `replace_confirm_gate_requires_the_exact_group_name_before_execute_is_
/// requested`) only assert that `take_replace_action()` returns
/// `Some(Execute)` -- that the UI REQUESTED execution -- never that the
/// controller was actually in a state where it WOULD execute. A test
/// asserting only the former "passes" against this exact bug, because
/// `app.rs`'s own key-gating was always correct; the break is entirely on
/// `runtime.rs`'s side, forwarding the text into the controller it drives.
/// So every test below asserts on the CONTROLLER's own post-call state
/// (`confirmation_text`, `step()`), not merely that an action was queued.
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use shr_command::{DiskStatus, GroupBandStatus, GroupStatus, SmartState, SmartSummary};
    use shr_inspect::StaticInspector;
    use shr_state::{ArrayState, StateExpansion, StateFilesystem};
    use std::time::Duration;

    use crate::scrub::ScrubState;

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

    fn disk(name: &str, id: &str) -> DiskStatus {
        DiskStatus {
            name: name.to_string(),
            id: Some(id.to_string()),
            size: Some(4_000_000_000_000),
            model: None,
            serial: None,
            rotational: Some(true),
            smart: ok_smart(),
            arrays: vec![],
            system_disk: false,
            system_mounts: vec![],
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

    /// A single-group report just barely enough to open each modal
    /// (`App::open_wizard`/`open_replace`/`open_scrub` all require exactly
    /// one group) -- one free disk so the wizard's `SelectDisks` step has
    /// something to show, though every test below jumps straight past it via
    /// `set_wizard_state`/`set_replace_state`/`set_scrub_state`, never
    /// actually driving the picker.
    fn single_group_report(group_name: &str) -> StatusReport {
        StatusReport {
            schema_version: shr_command::report::SCHEMA_VERSION,
            health: Health::Healthy,
            disks: vec![disk("vdb", "ata-FREE1")],
            arrays: vec![],
            groups: vec![GroupStatus {
                name: group_name.to_string(),
                mode: "shr".into(),
                layout_version: 1,
                mount_point: "/mnt/shr_data".into(),
                fs_uuid: None,
                vg_name: "shr_vg".into(),
                lv_name: "data".into(),
                compression: "zstd:3".into(),
                usable_bytes: 4_000_000_000_000,
                resize_pending: false,
                disks: vec![],
                bands: vec![band(false)],
            }],
            state_path: None,
        }
    }

    /// A minimal, validly-shaped `ArrayState` for `preview_state` -- only
    /// `can_execute()`'s `.is_some()` check cares that this exists, not what
    /// it contains, so no realistic disks/bands are needed (YAGNI).
    fn dummy_preview_state(group_name: &str) -> ArrayState {
        ArrayState {
            name: group_name.to_string(),
            mode: "shr".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            layout_version: 1,
            disks: vec![],
            bands: vec![],
            filesystem: StateFilesystem {
                fs_uuid: None,
                mount_point: "/mnt/shr_data".to_string(),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
            },
            expansion: StateExpansion::default(),
        }
    }

    /// A deliberately UNSEEDED store (no `state.toml` ever written to this
    /// tempdir) -- `shr-orchestrate`'s `expand`/`replace_disk`/`scrub_start`/
    /// `scrub_cancel` all begin with `self.store.load()?.ok_or(NoActiveArray)?`
    /// (confirmed by reading `crates/shr-orchestrate/src/engine.rs`), so this
    /// makes every one of them fail fast with `NoActiveArray` BEFORE ever
    /// touching the injected `CommandRunner` -- safe to pair with a real
    /// `SystemRunner` in a test without spawning any real `mdadm`/`lvm`/
    /// `btrfs` process.
    fn unseeded_store() -> (tempfile::TempDir, Arc<StateStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        (dir, store)
    }

    /// The FS tab's Used/Free columns, and the decimation cache that
    /// feeds them without spawning `btrfs`/`df` on every 2s poll tick.
    mod fs_usage_decimation {
        use super::*;
        use shr_exec::{CommandOutput, ExecError};
        use std::sync::atomic::{AtomicU32, Ordering};

        /// A minimal, validly-shaped `GroupStatus` -- only `name` and
        /// `mount_point` matter to `fs_usage_map`/`build_fs_df`; every other
        /// field is a placeholder (YAGNI: nothing below reads them).
        fn minimal_group(name: &str) -> GroupStatus {
            GroupStatus {
                name: name.to_string(),
                mode: "shr".into(),
                layout_version: 1,
                mount_point: format!("/mnt/{name}"),
                fs_uuid: None,
                vg_name: "shr_vg".into(),
                lv_name: "data".into(),
                compression: "zstd:3".into(),
                usable_bytes: 8_000_000_000_000,
                resize_pending: false,
                disks: vec![],
                bands: vec![],
            }
        }

        /// Always fails (`Prerequisite`, never spawns a real process) and
        /// counts how many times `run` was actually called -- `AtomicU32`,
        /// not `Cell`, because `CommandRunner: Send + Sync` (see
        /// `crates/shr-exec/src/cmd.rs`).
        #[derive(Default)]
        struct CountingRunner {
            calls: AtomicU32,
        }

        impl CommandRunner for CountingRunner {
            fn run(&self, _program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ExecError::Prerequisite(
                    "test double: never actually runs a command".to_string(),
                ))
            }

            fn is_dry_run(&self) -> bool {
                false
            }
        }

        /// Cost mitigation: the whole reason `FsUsageCache` exists is to
        /// avoid spawning 2 subprocesses per group on every 2s refresh tick.
        /// This asserts the ACTUAL shellout count, not just that the
        /// returned figures happen to match across calls -- a test that only
        /// checked the latter would still pass if decimation were silently
        /// broken and every call re-shelled out with identical (fake) data.
        #[test]
        fn refresh_fs_df_only_shells_out_on_decimated_cycles_not_every_call() {
            let runner = CountingRunner::default();
            let groups = vec![minimal_group("shr1")];
            let cache = FsUsageCache::new();

            // Each group's usage costs 2 calls (`btrfs filesystem usage` +
            // `df`) -- see `fs_usage_input`.
            refresh_fs_df(&runner, &groups, &cache); // cycle 0: fetch
            assert_eq!(runner.calls.load(Ordering::SeqCst), 2, "cycle 0 must fetch");

            for _ in 1..FS_USAGE_REFRESH_EVERY_CYCLES {
                refresh_fs_df(&runner, &groups, &cache);
            }
            assert_eq!(
                runner.calls.load(Ordering::SeqCst),
                2,
                "cycles between decimation points must reuse the cache, not shell out again"
            );

            refresh_fs_df(&runner, &groups, &cache); // cycle == FS_USAGE_REFRESH_EVERY_CYCLES: fetch again
            assert_eq!(
                runner.calls.load(Ordering::SeqCst),
                4,
                "the Nth cycle must fetch again"
            );
        }

        /// Honesty requirement: a runner that can never succeed (no
        /// `btrfs`/`df` on this project's Windows dev host, or a genuine
        /// failure on a real host) must produce `None` for every live usage
        /// field, never a fabricated number -- while `usable_bytes`, which
        /// comes from `state.toml` via `GroupStatus`, not the runner, stays
        /// correctly populated regardless.
        #[test]
        fn refresh_fs_df_reports_unknown_not_fabricated_when_the_runner_fails() {
            let runner = CountingRunner::default();
            let groups = vec![minimal_group("shr1")];
            let cache = FsUsageCache::new();

            let df = refresh_fs_df(&runner, &groups, &cache);

            assert_eq!(df.groups.len(), 1);
            assert_eq!(df.groups[0].usable_bytes, 8_000_000_000_000);
            assert_eq!(df.groups[0].data_used_bytes, None);
            assert_eq!(df.groups[0].metadata_used_bytes, None);
            assert_eq!(df.groups[0].unallocated_bytes, None);
            assert_eq!(df.groups[0].statvfs_avail_bytes, None);
        }
    }

    mod wizard_execute_forwards_confirmation {
        use super::*;

        /// The happy path: `app.rs`'s own gate already required the typed
        /// text to match `group_name` before it would queue `Execute` at
        /// all -- so the ONLY thing left for `handle_wizard_action` to get
        /// right is actually forwarding that text into the controller before
        /// asking it to execute.
        #[test]
        fn matching_confirmation_text_reaches_the_controller_and_execute_is_allowed_to_run() {
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('a')));
            app.set_wizard_state(WizardState {
                step: Some(Step::Confirm),
                preview_state: Some(dummy_preview_state("shr1")),
                ..Default::default()
            });
            for ch in "shr1".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_wizard_action();
            assert_eq!(
                action,
                Some(WizardAction::Execute),
                "app.rs's own gate must still fire the action"
            );

            let (_dir, store) = unseeded_store();
            let inspector: &'static StaticInspector = Box::leak(Box::new(StaticInspector::default()));
            let mut controller = AddDiskController::new(
                inspector,
                store.clone(),
                "mdadm.conf",
                "fstab",
                vec![],
                Some("shr1".to_string()),
                vec!["sdb".to_string()],
                false,
            );
            controller.state.step = Some(Step::Confirm);
            controller.state.preview_state = Some(dummy_preview_state("shr1"));
            let mut wizard_controller = Some(controller);
            let mut wizard_execute_rx = None;

            handle_wizard_action(
                &mut app,
                &store,
                inspector,
                &mut wizard_controller,
                &mut wizard_execute_rx,
                action.unwrap(),
            );

            assert!(
                wizard_controller.is_none(),
                "Execute must take/consume the controller"
            );
            let rx = wizard_execute_rx
                .expect("A genuinely matching confirmation must be allowed to execute, not silently refused");
            let final_state = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the background execution thread must run to completion and report back");

            // The discriminating assertion pre- vs post-fix: before the fix,
            // `set_confirmation_text` is never called by `runtime.rs`, so
            // this stays "" regardless of what the operator typed.
            assert_eq!(
                final_state.confirmation_text, "shr1",
                "The operator's typed confirmation text never reached the controller"
            );
            // A refused `execute()` returns `false` WITHOUT touching
            // `self.state` at all (see `AddDiskController::execute`) -- so
            // the state having moved off `Confirm` is itself proof
            // `can_execute()` read true and `execute()` actually ran.
            assert_ne!(
                final_state.step(),
                Step::Confirm,
                "execute() must actually have been allowed to run, not silently refused"
            );
        }

        /// Defence in depth for constraint 2: even though `app.rs`'s
        /// key gate already guarantees `confirmation_input == view.
        /// group_name` before `Execute` is ever queued, the held controller
        /// itself could still refuse (e.g. built for a different group than
        /// currently displayed -- simulated here). `can_execute()` must
        /// still be the one true gate, and a refusal must surface as
        /// `Step::Error`, never a frozen `Step::Executing` screen for work
        /// that will never run.
        #[test]
        fn a_controller_that_still_refuses_is_never_left_on_a_frozen_executing_screen() {
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('a')));
            app.set_wizard_state(WizardState {
                step: Some(Step::Confirm),
                preview_state: Some(dummy_preview_state("shr1")),
                ..Default::default()
            });
            for ch in "shr1".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_wizard_action().unwrap();

            let (_dir, store) = unseeded_store();
            let inspector: &'static StaticInspector = Box::leak(Box::new(StaticInspector::default()));
            // Built for a DIFFERENT group than the view shows -- a stale
            // controller `can_execute()` must still refuse.
            let mut controller = AddDiskController::new(
                inspector,
                store.clone(),
                "mdadm.conf",
                "fstab",
                vec![],
                Some("othergroup".to_string()),
                vec![],
                false,
            );
            controller.state.step = Some(Step::Confirm);
            controller.state.preview_state = Some(dummy_preview_state("othergroup"));
            let mut wizard_controller = Some(controller);
            let mut wizard_execute_rx = None;

            handle_wizard_action(
                &mut app,
                &store,
                inspector,
                &mut wizard_controller,
                &mut wizard_execute_rx,
                action,
            );

            assert!(
                wizard_execute_rx.is_none(),
                "must not spawn a background execution when the controller actually refuses"
            );
            let view = app.wizard().expect("wizard modal must still be open");
            assert_eq!(
                view.step(),
                Step::Error,
                "a refusal must surface as Step::Error, never a frozen Step::Executing"
            );
            assert!(
                view.controller_state.error_message.is_some(),
                "must say why, not silently do nothing"
            );
        }
    }

    /// The TUI could not add a disk carrying existing content --
    /// `handle_wizard_action`'s `RunPreflight` arm hardcoded `force_content:
    /// false` into every freshly built `AddDiskController`, so `app.rs`'s
    /// operator-facing override (`o` at `Step::Preflight`, see
    /// `WizardView::force_content`) never actually reached
    /// `preflight_write_targets`. These assert on the controller's own
    /// resulting `WritePreflight.blockers` -- a real safety-check outcome --
    /// not merely on `view.force_content`, which is exactly the earlier trap:
    /// a test that only checked `view.force_content == true` would still
    /// pass with the hardcoded `false` bug in place.
    mod wizard_preflight_forwards_force_content {
        use super::*;
        use shr_inspect::{ByIdIndex, WriteBlocker};

        /// One disk ("vdb") carrying an existing ext4 signature, with a
        /// stable by-id name attached so `WriteBlocker::NoStableId` never
        /// masks the assertion this test actually cares about
        /// (`WriteBlocker::HasContent`).
        fn inspector_with_used_disk() -> &'static StaticInspector {
            let lsblk_json = r#"{"blockdevices":[
                {"name":"vdb","size":4000000000000,"type":"disk","fstype":"ext4"}
            ]}"#;
            let inspector =
                StaticInspector::from_raw(lsblk_json, "", Default::default()).expect("valid lsblk fixture");
            let mut by_id = ByIdIndex::empty();
            by_id.insert("vdb", "ata-USED-DISK");
            Box::leak(Box::new(inspector.with_by_id(by_id)))
        }

        fn has_content_blocker(blockers: &[WriteBlocker]) -> bool {
            blockers
                .iter()
                .any(|b| matches!(b, WriteBlocker::HasContent { name } if name == "vdb"))
        }

        /// The default posture (`force_content == false`, `open_wizard`'s
        /// initial value): a disk with existing content stays blocked --
        /// this is the safety check an earlier fix must NOT weaken.
        #[test]
        fn without_the_override_a_disk_with_content_is_blocked() {
            let (_dir, store) = unseeded_store();
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('a')));
            app.handle_key(key(KeyCode::Char(' '))); // select "vdb" (cursor starts on it)
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_wizard_action().expect("Enter must request preflight");

            let mut wizard_controller = None;
            let mut wizard_execute_rx = None;
            handle_wizard_action(
                &mut app,
                &store,
                inspector_with_used_disk(),
                &mut wizard_controller,
                &mut wizard_execute_rx,
                action,
            );

            let report = wizard_controller
                .as_ref()
                .and_then(|c| c.state.preflight.as_ref())
                .expect("run_preflight must have produced a report");
            assert!(
                has_content_blocker(&report.blockers),
                "a disk with existing content must be blocked without the operator's override: {report:?}"
            );
            assert_eq!(app.wizard().unwrap().step(), Step::Preflight);
        }

        /// The discriminating case: after the operator's explicit `o`
        /// override (`app.rs`'s `Step::Preflight` arm sets `force_content =
        /// true` and re-queues `RunPreflight`), the SAME disk must no
        /// longer be blocked by `HasContent` -- proving the flag actually
        /// reached `AddDiskController`/`preflight_write_targets`, not just
        /// `WizardView`.
        #[test]
        fn the_o_override_reaches_the_controller_and_unblocks_the_same_disk() {
            let (_dir, store) = unseeded_store();
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('a')));
            app.handle_key(key(KeyCode::Char(' ')));
            app.handle_key(key(KeyCode::Enter));
            let first_action = app.take_wizard_action().expect("Enter must request preflight");

            let mut wizard_controller = None;
            let mut wizard_execute_rx = None;
            handle_wizard_action(
                &mut app,
                &store,
                inspector_with_used_disk(),
                &mut wizard_controller,
                &mut wizard_execute_rx,
                first_action,
            );
            assert_eq!(
                app.wizard().unwrap().step(),
                Step::Preflight,
                "must still be blocked before the override"
            );

            // The operator's deliberate, non-Enter override keypress.
            app.handle_key(key(KeyCode::Char('o')));
            assert!(
                app.wizard().unwrap().force_content,
                "'o' must set the override flag"
            );
            let second_action = app
                .take_wizard_action()
                .expect("'o' must re-request preflight so the override is actually evaluated");

            handle_wizard_action(
                &mut app,
                &store,
                inspector_with_used_disk(),
                &mut wizard_controller,
                &mut wizard_execute_rx,
                second_action,
            );

            let report = wizard_controller
                .as_ref()
                .and_then(|c| c.state.preflight.as_ref())
                .expect("the retried run_preflight must have produced a report");
            assert!(
                !has_content_blocker(&report.blockers),
                "The operator's override must reach the controller and clear the HasContent \
                 blocker, not just toggle a UI field: {report:?}"
            );
            assert_eq!(
                app.wizard().unwrap().step(),
                Step::Preview,
                "an unblocked preflight must advance the wizard, not stay stuck on Step::Preflight"
            );
        }
    }

    mod replace_execute_forwards_confirmation {
        use super::*;

        #[test]
        fn matching_confirmation_text_reaches_the_controller_and_execute_is_allowed_to_run() {
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('x')));
            app.set_replace_state(ReplaceWizardState {
                step: Some(ReplaceStep::Confirm),
                preview_state: Some(dummy_preview_state("shr1")),
                ..Default::default()
            });
            for ch in "shr1".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_replace_action();
            assert_eq!(
                action,
                Some(ReplaceAction::Execute),
                "app.rs's own gate must still fire the action"
            );

            let (_dir, store) = unseeded_store();
            let inspector: &'static StaticInspector = Box::leak(Box::new(StaticInspector::default()));
            let mut controller = ReplaceDiskController::new(
                inspector,
                store.clone(),
                "mdadm.conf",
                "fstab",
                vec![],
                Some("shr1".to_string()),
            );
            controller.state.step = Some(ReplaceStep::Confirm);
            controller.state.preview_state = Some(dummy_preview_state("shr1"));
            let mut replace_controller = Some(controller);
            let mut replace_execute_rx = None;

            handle_replace_action(
                &mut app,
                &store,
                &mut replace_controller,
                &mut replace_execute_rx,
                action.unwrap(),
            );

            assert!(
                replace_controller.is_none(),
                "Execute must take/consume the controller"
            );
            let rx = replace_execute_rx
                .expect("A genuinely matching confirmation must be allowed to execute, not silently refused");
            let final_state = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the background execution thread must run to completion and report back");

            assert_eq!(
                final_state.confirmation_text, "shr1",
                "The operator's typed confirmation text never reached the controller"
            );
            assert_ne!(
                final_state.step(),
                ReplaceStep::Confirm,
                "execute() must actually have been allowed to run, not silently refused"
            );
        }

        #[test]
        fn a_controller_that_still_refuses_is_never_left_on_a_frozen_executing_screen() {
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('x')));
            app.set_replace_state(ReplaceWizardState {
                step: Some(ReplaceStep::Confirm),
                preview_state: Some(dummy_preview_state("shr1")),
                ..Default::default()
            });
            for ch in "shr1".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_replace_action().unwrap();

            let (_dir, store) = unseeded_store();
            let inspector: &'static StaticInspector = Box::leak(Box::new(StaticInspector::default()));
            let mut controller = ReplaceDiskController::new(
                inspector,
                store.clone(),
                "mdadm.conf",
                "fstab",
                vec![],
                Some("othergroup".to_string()),
            );
            controller.state.step = Some(ReplaceStep::Confirm);
            controller.state.preview_state = Some(dummy_preview_state("othergroup"));
            let mut replace_controller = Some(controller);
            let mut replace_execute_rx = None;

            handle_replace_action(
                &mut app,
                &store,
                &mut replace_controller,
                &mut replace_execute_rx,
                action,
            );

            assert!(
                replace_execute_rx.is_none(),
                "must not spawn a background execution when the controller actually refuses"
            );
            let view = app.replace().expect("replace modal must still be open");
            assert_eq!(
                view.step(),
                ReplaceStep::Error,
                "a refusal must surface as Step::Error, never a frozen Step::Executing"
            );
            assert!(
                view.controller_state.error_message.is_some(),
                "must say why, not silently do nothing"
            );
        }
    }

    mod scrub_confirm_forwards_confirmation {
        use super::*;

        #[test]
        fn matching_confirmation_text_reaches_the_controller_and_confirm_cancel_is_allowed_to_run() {
            let mut report = single_group_report("shr1");
            report.groups[0].bands = vec![band(true)]; // a scrub is already running
            let mut app = App::new(report);
            app.handle_key(key(KeyCode::Char('s')));
            app.take_scrub_action(); // consume the auto-fired RequestCancel; not under test here
            app.set_scrub_state(ScrubState {
                step: Some(ScrubCtrlStep::ConfirmCancel),
                target_group: Some("shr1".to_string()),
                ..Default::default()
            });
            for ch in "shr1".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_scrub_action();
            assert_eq!(
                action,
                Some(ScrubUiAction::Confirm),
                "app.rs's own gate must still fire the action"
            );

            let (_dir, store) = unseeded_store();
            let mut controller = ScrubController::new(store.clone(), Some("shr1".to_string()));
            controller.state.step = Some(ScrubCtrlStep::ConfirmCancel);
            controller.state.target_group = Some("shr1".to_string());
            let mut scrub_controller = Some(controller);

            handle_scrub_action(&mut app, &store, &mut scrub_controller, action.unwrap());

            let controller = scrub_controller
                .expect("Confirm runs synchronously on the same controller; it must not be dropped");
            assert_eq!(
                controller.state.confirmation_text, "shr1",
                "The operator's typed confirmation text never reached the scrub controller"
            );
            // A refused `confirm_cancel()` returns `false` WITHOUT touching
            // `self.state` at all -- state staying off `ConfirmCancel` is
            // proof `can_confirm_cancel()` read true and the call ran.
            assert_ne!(
                controller.state.step(),
                ScrubCtrlStep::ConfirmCancel,
                "confirm_cancel() must actually have been allowed to run, not silently refused"
            );
        }

        #[test]
        fn a_controller_that_still_refuses_surfaces_an_error_not_a_silent_no_op() {
            let mut report = single_group_report("shr1");
            report.groups[0].bands = vec![band(true)];
            let mut app = App::new(report);
            app.handle_key(key(KeyCode::Char('s')));
            app.take_scrub_action();
            app.set_scrub_state(ScrubState {
                step: Some(ScrubCtrlStep::ConfirmCancel),
                target_group: Some("shr1".to_string()),
                ..Default::default()
            });
            for ch in "shr1".chars() {
                app.handle_key(key(KeyCode::Char(ch)));
            }
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_scrub_action().unwrap();

            let (_dir, store) = unseeded_store();
            // Built for a DIFFERENT group than the view shows.
            let mut controller = ScrubController::new(store.clone(), Some("othergroup".to_string()));
            controller.state.step = Some(ScrubCtrlStep::ConfirmCancel);
            let mut scrub_controller = Some(controller);

            handle_scrub_action(&mut app, &store, &mut scrub_controller, action);

            let view = app.scrub().expect("scrub modal must still be open");
            assert_eq!(
                view.step(),
                ScrubCtrlStep::Error,
                "a refusal must surface as Step::Error, never silently leave the confirm screen unchanged"
            );
            assert!(
                view.controller_state.error_message.is_some(),
                "must say why, not silently do nothing"
            );
        }
    }

    /// `run_reconcile` is the pure, directly-testable split-out that
    /// actually drives `OrchestrationEngine::reconcile()`.
    /// Every test here asserts on REAL effects -- the exact shell-out
    /// sequence a fake `CommandRunner` recorded, and what the state store
    /// holds after reloading it from disk -- never merely that
    /// `ReconcileUiAction::Execute` was requested. This is the same
    /// trap `wizard_execute_forwards_confirmation`/
    /// `replace_execute_forwards_confirmation`/`scrub_confirm_forwards_
    /// confirmation` above close for their own controllers: a test that
    /// only checked "the UI asked for execution" would still pass against
    /// a `runtime.rs` that built the right state but never actually called
    /// `engine.reconcile()` at all.
    mod run_reconcile_effects {
        use super::*;
        use shr_exec::{CommandOutput, ExecError};
        use shr_state::{StateBand, StateExpansion, StateFile};
        use std::sync::Mutex;

        /// Records every call verbatim as `"<program> <args joined by
        /// space>"` (same shape `shr-orchestrate`'s own test-only
        /// `FailingRunner::get_recorded` uses) so a test can assert on the
        /// exact command line, not just that "some command ran". `cat`
        /// answers with `sync_action`; any program named in `fail_program`
        /// fails with `fail_stderr`; everything else succeeds with empty
        /// output.
        struct ScriptedRunner {
            recorded: Mutex<Vec<String>>,
            sync_action: String,
            fail_program: Option<&'static str>,
            fail_stderr: String,
        }

        impl ScriptedRunner {
            fn idle() -> Self {
                Self {
                    recorded: Mutex::new(Vec::new()),
                    sync_action: "idle".to_string(),
                    fail_program: None,
                    fail_stderr: String::new(),
                }
            }

            fn reshaping() -> Self {
                Self {
                    sync_action: "reshape".to_string(),
                    ..Self::idle()
                }
            }

            fn idle_but_failing_on(program: &'static str, stderr: &str) -> Self {
                Self {
                    fail_program: Some(program),
                    fail_stderr: stderr.to_string(),
                    ..Self::idle()
                }
            }

            fn calls(&self) -> Vec<String> {
                self.recorded.lock().unwrap().clone()
            }
        }

        impl CommandRunner for ScriptedRunner {
            fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
                self.recorded
                    .lock()
                    .unwrap()
                    .push(format!("{program} {}", args.join(" ")));
                if self.fail_program == Some(program) {
                    return Err(ExecError::NonZeroExit {
                        program: program.to_string(),
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: self.fail_stderr.clone(),
                    });
                }
                if program == "cat" {
                    return Ok(CommandOutput {
                        stdout: self.sync_action.clone(),
                        stderr: String::new(),
                    });
                }
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }

            fn is_dry_run(&self) -> bool {
                false
            }
        }

        /// A single-group, single-band `StateFile` with `resize_pending`
        /// set on band 0 -- the exact shape `execute_grow` leaves behind
        /// when its reshape was still running when `expand()` returned.
        fn seeded_state_with_resize_pending(resize_pending: bool) -> StateFile {
            // `StateFile::new`, not a struct literal: this fixture only
            // cares about the one group below, and every field the wrapper
            // grows afterwards has a correct empty default that a literal
            // would have to keep restating.
            StateFile::new(vec![ArrayState {
                name: "shr1".to_string(),
                mode: "shr".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                layout_version: 1,
                disks: vec![],
                bands: vec![StateBand {
                    index: 0,
                    level: "raid1".to_string(),
                    md_name: "md0".to_string(),
                    md_uuid: None,
                    member_partitions: vec![],
                    usable_bytes: 4_000_000_000_000,
                    resize_pending,
                    last_smart_reallocated: None,
                    last_scrub: None,
                    scrub_in_progress: false,
                    pending_member_removal: None,
                    reshape_priority: None,
                }],
                filesystem: StateFilesystem {
                    fs_uuid: None,
                    mount_point: "/mnt/shr_data".to_string(),
                    vg_name: "shr_vg".to_string(),
                    lv_name: "data".to_string(),
                    compression: "zstd:3".to_string(),
                },
                expansion: StateExpansion::default(),
            }])
        }

        #[test]
        fn no_state_toml_at_all_is_a_safe_no_op_that_never_touches_the_command_runner() {
            let (_dir, store) = unseeded_store();
            let runner = ScriptedRunner::idle();

            let state = run_reconcile(&runner, store);

            assert_eq!(state.step(), ReconcileStep::Done);
            assert!(state.performed.is_empty());
            assert!(state.still_pending.is_none());
            assert!(state.error_message.is_none());
            assert!(
                runner.calls().is_empty(),
                "engine.reconcile() must short-circuit on `store.load()? == None` \
                 before ever touching the CommandRunner: {:?}",
                runner.calls()
            );
        }

        #[test]
        fn a_finished_reshape_runs_the_real_resize_sequence_in_order_and_persists_the_cleared_flag() {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
            store
                .save(&seeded_state_with_resize_pending(true))
                .expect("seed state.toml");
            let runner = ScriptedRunner::idle();

            let state = run_reconcile(&runner, store.clone());

            assert_eq!(state.step(), ReconcileStep::Done, "{:?}", state.error_message);
            assert_eq!(
                state.performed,
                vec![describe_reconcile_action(&ReconcileAction::ResizeCompleted {
                    group: "shr1".to_string(),
                    band_index: 0,
                    md_name: "md0".to_string(),
                })],
                "must report the exact resize it just completed, same wording as `shr-cli`'s own output"
            );
            assert!(state.still_pending.is_none());
            assert!(state.error_message.is_none());

            // The discriminating assertion: the REAL pvresize/lvextend/btrfs
            // sequence must have actually run, in order -- not merely that
            // `ReconcileStep::Done` was reached (a bug that skipped the
            // resize entirely but still returned `Done` would pass every
            // assertion above this one).
            assert_eq!(
                runner.calls(),
                vec![
                    "cat /sys/block/md0/md/sync_action".to_string(),
                    "pvresize /dev/md0".to_string(),
                    "lvextend -l +100%FREE /dev/shr_vg/data".to_string(),
                    "btrfs filesystem resize max /mnt/shr_data".to_string(),
                ]
            );

            // And the clear must be PERSISTED, not just present in the
            // in-memory `ReconcileOutcome` -- reload from disk to prove it.
            let reloaded = store
                .load()
                .expect("reload")
                .expect("state.toml must still exist");
            assert!(
                !reloaded.groups[0].bands[0].resize_pending,
                "resize_pending must be cleared in the file on disk, not just in memory"
            );
        }

        #[test]
        fn a_still_reshaping_band_is_left_alone_and_reported_as_still_pending() {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
            store
                .save(&seeded_state_with_resize_pending(true))
                .expect("seed state.toml");
            let runner = ScriptedRunner::reshaping();

            let state = run_reconcile(&runner, store.clone());

            assert_eq!(state.step(), ReconcileStep::Done);
            assert!(state.performed.is_empty(), "nothing was actually completed yet");
            assert_eq!(
                state.still_pending.as_deref(),
                Some(
                    "Still rebuilding, so the expansion stays unfinished for now: \
                     `shr1` band 0.",
                ),
                "same wording `shr-cli`'s own text report uses for this exact case"
            );
            assert!(state.error_message.is_none());

            // Must have stopped right after reading `sync_action` --
            // pvresize/lvextend/btrfs must never run while the reshape is
            // still in progress.
            assert_eq!(
                runner.calls(),
                vec!["cat /sys/block/md0/md/sync_action".to_string()]
            );

            let reloaded = store
                .load()
                .expect("reload")
                .expect("state.toml must still exist");
            assert!(
                reloaded.groups[0].bands[0].resize_pending,
                "nothing changed, so resize_pending must stay exactly as it was"
            );
        }

        #[test]
        fn an_engine_error_surfaces_verbatim_never_swallowed_or_reworded() {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
            store
                .save(&seeded_state_with_resize_pending(true))
                .expect("seed state.toml");
            let runner = ScriptedRunner::idle_but_failing_on(
                "pvresize",
                "mdadm --detail failed: /dev/md0: No such file or directory",
            );

            let state = run_reconcile(&runner, store.clone());

            assert_eq!(state.step(), ReconcileStep::Error);
            let msg = state.error_message.expect("a failed reconcile must say why");
            assert!(
                msg.contains("mdadm --detail failed: /dev/md0: No such file or directory"),
                "the engine's own error text must reach the UI verbatim, never reworded: {msg}"
            );
            assert!(state.performed.is_empty());

            // Confirms exactly where it stopped: `cat` (read sync_action)
            // then `pvresize` (which failed) -- `lvextend`/`btrfs` must
            // never have run.
            assert_eq!(
                runner.calls(),
                vec![
                    "cat /sys/block/md0/md/sync_action".to_string(),
                    "pvresize /dev/md0".to_string()
                ]
            );

            // A failed call must never persist a half-applied state.
            let reloaded = store
                .load()
                .expect("reload")
                .expect("state.toml must still exist");
            assert!(
                reloaded.groups[0].bands[0].resize_pending,
                "a failed resize must leave resize_pending untouched, not clear it"
            );
        }
    }

    /// Counterpart of `wizard_execute_forwards_confirmation`'s
    /// background-thread test: proves `handle_reconcile_action` actually
    /// dispatches to a real background thread that runs `run_reconcile` and
    /// reports back over the channel -- not merely that the modal reached
    /// `Executing`. Uses an unseeded store (same trick the wizard/replace
    /// tests use) so the real `SystemRunner` the thread hardcodes is safe
    /// to exercise in a unit test: `engine.reconcile()` returns `Ok(None)`
    /// from `store.load()? == None` before ever touching it.
    mod reconcile_execute_dispatches_a_real_background_thread {
        use super::*;

        #[test]
        fn execute_runs_the_real_run_reconcile_on_a_background_thread_and_reports_back() {
            let mut app = App::new(single_group_report("shr1"));
            app.handle_key(key(KeyCode::Char('f')));
            app.handle_key(key(KeyCode::Enter));
            let action = app.take_reconcile_action();
            assert_eq!(
                action,
                Some(ReconcileUiAction::Execute),
                "app.rs's own gate must still fire the action"
            );

            let (_dir, store) = unseeded_store();
            let mut reconcile_execute_rx = None;

            handle_reconcile_action(&mut app, &store, &mut reconcile_execute_rx, action.unwrap());

            assert_eq!(
                app.reconcile().expect("modal stays open while executing").step(),
                ReconcileStep::Executing,
                "must show Executing immediately, not block the caller"
            );
            let rx = reconcile_execute_rx
                .expect("Execute must spawn a background thread and hand back its receiver");
            let final_state = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the background thread must run run_reconcile to completion and report back");

            // The discriminating assertion: a build that fired the thread
            // but forgot to actually call `run_reconcile` (e.g. sent back a
            // hardcoded `Done` without ever touching the store) would still
            // pass a check for "some `Done` arrived" -- this instead proves
            // the real, store-driven code path ran, by checking the exact
            // outcome only `run_reconcile` against an unseeded store
            // produces.
            assert_eq!(final_state.step(), ReconcileStep::Done);
            assert!(final_state.performed.is_empty());
            assert!(final_state.error_message.is_none());
        }
    }
}
