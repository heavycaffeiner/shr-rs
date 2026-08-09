//! TUI scrub start/cancel controls (`shr_tui::scrub::ScrubController`).
//! Mirrors `crates/shr-tui/src/wizard.rs`'s own unit-test shape and its
//! "prove the gate gates, prove a real call reaches the engine, prove a
//! real engine refusal surfaces verbatim" evidentiary bar -- not
//! `shr-orchestrate`'s own ~56-test suite's job to re-prove `scrub_start`/
//! `scrub_cancel`'s internal correctness, only that this
//! controller forwards them faithfully.

use shr_exec::{CommandOutput, CommandRunner, ExecError};
use shr_state::{
    ArrayState, StateBand, StateDisk, StateExpansion, StateFile, StateFilesystem, StatePartition, StateStore,
};
use shr_tui::scrub::{ScrubAction, ScrubController, Step};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

fn healthy_band() -> StateBand {
    StateBand {
        index: 0,
        level: "raid1".to_string(),
        md_name: "md0".to_string(),
        md_uuid: Some("a1b2c3d4:e5f6a7b8:c9d0e1f2:a3b4c5d6".to_string()),
        member_partitions: vec!["11111111-1111-1111-1111-111111111111".to_string()],
        usable_bytes: 4_000_000_000_000,
        resize_pending: false,
        last_smart_reallocated: None,
        last_scrub: None,
        scrub_in_progress: false,
        pending_member_removal: None,
        reshape_priority: None,
    }
}

fn base_state(name: &str, expansion: StateExpansion) -> ArrayState {
    ArrayState {
        name: name.to_string(),
        mode: "shr".to_string(),
        created_at: "2026-07-30T00:00:00Z".to_string(),
        layout_version: 1,
        disks: vec![StateDisk {
            id: "ata-EXISTING1".to_string(),
            size_bytes: 4_000_000_000_000,
            serial: None,
            model: None,
            added_at: "2026-07-30T00:00:00Z".to_string(),
            partitions: vec![StatePartition {
                part_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
                offset_bytes: 0,
                size_bytes: 4_000_000_000_000,
                band_index: 0,
            }],
        }],
        bands: vec![healthy_band()],
        filesystem: StateFilesystem {
            fs_uuid: Some("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d".to_string()),
            mount_point: "/mnt/shr_data".to_string(),
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
        },
        expansion,
    }
}

fn seed_healthy_group(store: &Arc<StateStore>, name: &str) {
    store
        .save(&StateFile::new(vec![base_state(name, StateExpansion::default())]))
        .unwrap();
}

fn seed_group_with_expansion_in_progress(store: &Arc<StateStore>, name: &str) {
    let expansion = StateExpansion {
        in_progress: true,
        ..StateExpansion::default()
    };
    store
        .save(&StateFile::new(vec![base_state(name, expansion)]))
        .unwrap();
}

/// Records every command it was asked to run and answers nothing -- proves
/// only *whether* the controller reached the engine, never engine
/// correctness (that's `shr-orchestrate`'s job).
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

/// Answers exactly what `MdadmExecutor`/`BtrfsExecutor` read during a
/// healthy, idle `scrub_start`/`scrub_cancel`: 0 degraded, `sync_action`
/// `idle`, every write and `btrfs scrub status` succeeding with output
/// that `parse_btrfs_scrub_status` reads as "not running, 0 errors".
struct HealthyRunner;

impl CommandRunner for HealthyRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let stdout = if program == "cat" {
            let path = args.first().copied().unwrap_or("");
            if path.ends_with("/degraded") {
                "0"
            } else if path.ends_with("/sync_action") {
                "idle"
            } else if path.ends_with("/mismatch_cnt") {
                "0"
            } else {
                ""
            }
        } else {
            ""
        };
        Ok(CommandOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }
    fn is_dry_run(&self) -> bool {
        false
    }
}

/// Fails every command -- deterministically drives `scrub_cancel` into its
/// "genuine failure, not tolerated" branch (an `Err` from BOTH the
/// write and the read-back, on both mdadm and btrfs).
struct AlwaysFailingRunner;

impl CommandRunner for AlwaysFailingRunner {
    fn run(&self, _program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
        Err(ExecError::Io(std::io::Error::other("no such tool in this test")))
    }
    fn is_dry_run(&self) -> bool {
        false
    }
}

#[test]
fn request_start_opens_an_explicit_confirm_step_naming_the_target_group() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));

    assert_eq!(ctrl.state.step(), Step::Idle);
    ctrl.request_start();

    assert_eq!(ctrl.state.step(), Step::ConfirmStart);
    assert_eq!(ctrl.state.action, Some(ScrubAction::Start));
    assert_eq!(ctrl.state.target_group.as_deref(), Some("shr1"));
}

#[test]
fn confirm_start_is_refused_before_request_start_was_called() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));
    let runner = RecordingRunner::new();

    assert!(!ctrl.can_confirm_start());
    assert!(
        !ctrl.confirm_start(&runner),
        "confirm_start() must refuse and do nothing"
    );
    assert!(
        runner.commands.lock().unwrap().is_empty(),
        "no engine call must have happened"
    );
}

#[test]
fn request_cancel_opens_an_explicit_confirm_step_naming_the_target_group() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));

    ctrl.request_cancel();

    assert_eq!(ctrl.state.step(), Step::ConfirmCancel);
    assert_eq!(ctrl.state.action, Some(ScrubAction::Cancel));
    assert_eq!(ctrl.state.target_group.as_deref(), Some("shr1"));
}

#[test]
fn confirm_cancel_is_refused_until_the_operator_types_the_exact_group_name() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));
    let runner = RecordingRunner::new();

    ctrl.request_cancel();
    assert!(
        !ctrl.can_confirm_cancel(),
        "a bare keypress reaching ConfirmCancel must not be enough"
    );
    assert!(!ctrl.confirm_cancel(&runner));
    assert!(runner.commands.lock().unwrap().is_empty());

    ctrl.set_confirmation_text("not-shr1");
    assert!(!ctrl.can_confirm_cancel(), "a wrong name must not be enough");
    assert!(!ctrl.confirm_cancel(&runner));
    assert!(runner.commands.lock().unwrap().is_empty());

    ctrl.set_confirmation_text("shr1");
    assert!(
        ctrl.can_confirm_cancel(),
        "the exact group name must unlock confirm_cancel"
    );
}

#[test]
fn confirm_start_surfaces_the_engines_own_refusal_verbatim_never_reaching_done() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_group_with_expansion_in_progress(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));
    let runner = HealthyRunner;

    ctrl.request_start();
    assert!(ctrl.confirm_start(&runner));

    assert_eq!(
        ctrl.state.step(),
        Step::Error,
        "the engine's expansion-in-progress refusal must land on Error, never Done"
    );
    let msg = ctrl
        .state
        .error_message
        .as_ref()
        .expect("error message must be set");
    assert!(
        msg.contains("expansion in progress") && msg.contains("scrub is blocked"),
        "must be the engine's own message verbatim, not a re-derived one: {msg}"
    );
}

#[test]
fn confirm_start_on_a_healthy_group_reaches_done_via_the_real_engine() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));
    let runner = HealthyRunner;

    ctrl.request_start();
    assert!(ctrl.confirm_start(&runner));

    assert_eq!(ctrl.state.step(), Step::Done);
    assert!(ctrl.state.error_message.is_none());
}

#[test]
fn confirm_cancel_on_a_healthy_group_reaches_done_via_the_real_engine() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));
    let runner = HealthyRunner;

    ctrl.request_cancel();
    ctrl.set_confirmation_text("shr1");
    assert!(ctrl.confirm_cancel(&runner));

    assert_eq!(ctrl.state.step(), Step::Done);
    assert!(ctrl.state.error_message.is_none());
}

#[test]
fn confirm_cancel_surfaces_a_genuine_engine_failure_verbatim_never_reaching_done() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));
    let runner = AlwaysFailingRunner;

    ctrl.request_cancel();
    ctrl.set_confirmation_text("shr1");
    assert!(ctrl.confirm_cancel(&runner));

    assert_eq!(
        ctrl.state.step(),
        Step::Error,
        "a genuine failure on every channel must land on Error"
    );
    let msg = ctrl
        .state
        .error_message
        .as_ref()
        .expect("error message must be set");
    assert!(
        msg.contains("scrub cancel did not fully stop everything"),
        "must be the engine's own aggregated failure text, not a re-derived one: {msg}"
    );
}

#[test]
fn reset_clears_a_pending_confirm_back_to_idle() {
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    seed_healthy_group(&store, "shr1");
    let mut ctrl = ScrubController::new(store, Some("shr1".to_string()));

    ctrl.request_start();
    assert_ne!(ctrl.state.step(), Step::Idle);
    ctrl.reset();

    assert_eq!(ctrl.state.step(), Step::Idle);
    assert!(ctrl.state.target_group.is_none());
}
