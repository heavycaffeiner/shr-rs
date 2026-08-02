//! Dry-run preview glue shared by every frontend that needs to show "what
//! would happen" before asking for real, irreversible confirmation --
//! `shr-cli`'s `--dry-run --json` (D13) and the TUI's Add Disk wizard
//! both call these instead of each hand-rolling their own `DryRunRunner` +
//! `OrchestrationEngine` wiring. Cockpit reaches the same behavior
//! indirectly, by spawning `shr-rs create/expand --dry-run --json`, which is
//! itself built on top of these functions -- so there is exactly one place
//! that decides what a dry-run preview looks like, not three.
//!
//! Deliberately thin: no new business logic lives here, just the "which
//! runner, which engine call, collect what it recorded" plumbing that was
//! previously duplicated inline in `shr-cli`'s `Create`/`Expand` handlers.

use std::sync::Arc;

use shr_exec::{CommandRunner, DryRunRunner};
use shr_state::{ArrayState, StateStore};

use crate::engine::{CreateRequest, ExpandRequest, OrchestrationEngine};
use crate::error::OrchestrateError;

/// Simulate `create` without touching any real disk. `ConfirmSink` is never
/// consulted (the engine skips it entirely under dry-run -- see
/// `OrchestrationEngine::create`'s doc comment), so no sink needs wiring
/// here at all.
pub fn preview_create(
    store: Arc<StateStore>,
    req: CreateRequest,
) -> Result<(ArrayState, Vec<String>), OrchestrateError> {
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, store);
    let state = engine.create(req)?;
    Ok((state, runner.get_recorded()))
}

/// Simulate `expand` without touching any real disk. Same no-`ConfirmSink`-
/// needed reasoning as `preview_create`. Delegates to
/// `preview_expand_against` with no status runner -- see that function's
/// doc comment for why production callers should call it directly instead.
pub fn preview_expand(
    store: Arc<StateStore>,
    req: ExpandRequest,
) -> Result<(ArrayState, Vec<String>), OrchestrateError> {
    preview_expand_against(store, req, None)
}

/// Same as `preview_expand`, but `expand()`'s live-status VALIDATION reads
/// (degraded/background-activity/scrub-running checks) are answered by
/// `status_runner` when given, instead of the internal `DryRunRunner` used
/// for everything else this preview does (real-VM repro).
///
/// `DryRunRunner` never executes anything for real -- exactly what a
/// preview needs for the MUTATING commands it collects (`mdadm --create`,
/// `mkfs`, ...) so nothing on the real array actually changes. But
/// `expand()`'s safety checks share that same runner today, and
/// `MdadmExecutor::sync_action`'s `is_dry_run()` shortcut fabricates a
/// fixed `"idle"` answer rather than reading anything -- so a scrub
/// genuinely running on the real array was invisible to this preview,
/// which fell through to a stale, misleading validation error (the "no
/// scrub completed yet") instead of the correct "a scrub is currently
/// running" one. Worse: `shr-cli`'s real (non-dry-run) `expand` handler
/// calls this preview FIRST to build the confirmation screen, so that
/// wrong error killed the whole command before the real, non-preview
/// `expand()` call (which reads the real system correctly) ever ran.
///
/// Production callers (`shr-cli`) MUST pass the real `SystemRunner` here so
/// this preview's blocking decisions match what the real `expand()` call
/// right after confirmation will decide -- anything else reintroduces the
/// Class of "what's shown doesn't match what happens" defect. Tests
/// pass `None` (this crate's own `preview_expand` tests have no real
/// system to read from; the fabricated-safe default is what they already
/// exercise).
pub fn preview_expand_against(
    store: Arc<StateStore>,
    req: ExpandRequest,
    status_runner: Option<&dyn CommandRunner>,
) -> Result<(ArrayState, Vec<String>), OrchestrateError> {
    let runner = DryRunRunner::new();
    let mut engine = OrchestrationEngine::new(&runner, store);
    if let Some(sr) = status_runner {
        engine = engine.with_status_runner(sr);
    }
    let state = engine.expand(req)?;
    Ok((state, runner.get_recorded()))
}

/// Simulate `destroy` without touching any real disk. Same no-
/// `ConfirmSink`-needed reasoning as `preview_create` (the engine skips it
/// entirely under dry-run). Delegates to `preview_destroy_against` with no
/// status runner -- see that function's doc comment for why production
/// callers should call it directly instead.
pub fn preview_destroy(
    store: Arc<StateStore>,
    name: Option<String>,
    zero_superblocks: bool,
) -> Result<Vec<String>, OrchestrateError> {
    preview_destroy_against(store, name, zero_superblocks, None)
}

/// Same as `preview_destroy`, but `destroy()`'s live-status VALIDATION read
/// (background-activity check) is answered by `status_runner` when given,
/// instead of the internal `DryRunRunner` used for everything else this
/// preview does -- same reasoning as `preview_expand_against`:
/// production callers (`shr-cli`) MUST pass the real `SystemRunner` here so
/// this preview's blocking decisions match what the real, non-preview
/// `destroy()` call right after confirmation will decide.
pub fn preview_destroy_against(
    store: Arc<StateStore>,
    name: Option<String>,
    zero_superblocks: bool,
    status_runner: Option<&dyn CommandRunner>,
) -> Result<Vec<String>, OrchestrateError> {
    let runner = DryRunRunner::new();
    let mut engine = OrchestrationEngine::new(&runner, store);
    if let Some(sr) = status_runner {
        engine = engine.with_status_runner(sr);
    }
    engine.destroy(name.as_deref(), zero_superblocks)?;
    Ok(runner.get_recorded())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shr_core::RedundancyMode;
    use shr_inspect::{DiskRef, ResolvedDisk};
    use shr_state::{StateBand, StateDisk, StateExpansion, StateFilesystem, StatePartition};
    use tempfile::tempdir;

    fn disk(id: &str, kernel: &str, size: u64) -> ResolvedDisk {
        ResolvedDisk {
            reference: DiskRef::Path(kernel.to_string()),
            kernel_name: kernel.to_string(),
            id: id.into(),
            size_bytes: size,
            serial: format!("SN-{kernel}"),
            model: "Test Disk".to_string(),
            system_mounts: vec![],
            has_content: false,
        }
    }

    #[test]
    fn preview_create_never_persists_state_and_returns_recorded_commands() {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        let req = CreateRequest {
            name: "shr1".to_string(),
            mode: RedundancyMode::Shr,
            disks: vec![
                disk("ata-DISK1", "sdb", 4_000_000_000_000),
                disk("ata-DISK2", "sdc", 4_000_000_000_000),
                disk("ata-DISK3", "sdd", 4_000_000_000_000),
            ],
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            mount_point: "/mnt/shr_data".to_string(),
            compression: "zstd:3".to_string(),
            system_disks: vec!["sda".to_string()],
        };

        let (state, commands) = preview_create(store.clone(), req).unwrap();

        assert_eq!(state.disks.len(), 3);
        assert!(commands.iter().any(|c| c.contains("mdadm --create")));
        assert!(!store.exists(), "a preview must never persist state.toml");
    }

    #[test]
    fn preview_create_planned_commands_include_the_internal_write_intent_bitmap() {
        // the screen shown before confirmation must show the SAME
        // `--bitmap=internal` that the real `mdadm --create` will issue --
        // a mismatch between preview and execution is a defect in its
        // own right.
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        let req = CreateRequest {
            name: "shr1".to_string(),
            mode: RedundancyMode::Shr,
            disks: vec![
                disk("ata-DISK1", "sdb", 4_000_000_000_000),
                disk("ata-DISK2", "sdc", 4_000_000_000_000),
                disk("ata-DISK3", "sdd", 4_000_000_000_000),
            ],
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            mount_point: "/mnt/shr_data".to_string(),
            compression: "zstd:3".to_string(),
            system_disks: vec!["sda".to_string()],
        };

        let (_, commands) = preview_create(store, req).unwrap();

        let create_cmds: Vec<&String> = commands.iter().filter(|c| c.contains("mdadm --create")).collect();
        assert_eq!(create_cmds.len(), 1, "{commands:?}");
        assert!(create_cmds[0].contains("--bitmap=internal"), "{create_cmds:?}");
    }

    /// Seed a real, planner-consistent 3x4TB RAID5 band0 (geometry computed
    /// by `shr_core::plan_initial`, not hand-picked -- `preview_expand`'s
    /// own `plan_expansion` re-validation insists this matches what the
    /// planner would derive fresh) so a following `preview_expand` call has
    /// a real group to expand. Mirrors `shr-tui`'s `wizard::tests::seed_group`.
    fn seed_three_disk_group(store: &StateStore, name: &str) {
        let core_disks = vec![
            shr_core::Disk::new("ata-DISK1", 4_000_000_000_000),
            shr_core::Disk::new("ata-DISK2", 4_000_000_000_000),
            shr_core::Disk::new("ata-DISK3", 4_000_000_000_000),
        ];
        let input = shr_core::PlannerInput::new(core_disks, RedundancyMode::Shr);
        let reserved_head = input.reserved_head;
        let initial_plan = shr_core::plan_initial(&input).unwrap();
        let band = &initial_plan.bands[0];
        let part_uuids = [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
            "33333333-3333-3333-3333-333333333333",
        ];
        let disks = band
            .members()
            .iter()
            .zip(part_uuids)
            .map(|(disk_id, part_uuid)| StateDisk {
                id: disk_id.as_str().to_string(),
                size_bytes: 4_000_000_000_000,
                serial: None,
                model: None,
                added_at: "2026-07-26T00:00:00Z".to_string(),
                partitions: vec![StatePartition {
                    part_uuid: part_uuid.to_string(),
                    offset_bytes: reserved_head + band.offset(),
                    size_bytes: band.size(),
                    band_index: band.band_index(),
                }],
            })
            .collect();
        let state = ArrayState {
            name: name.to_string(),
            mode: "shr".to_string(),
            created_at: "2026-07-26T00:00:00Z".to_string(),
            layout_version: 1,
            disks,
            bands: vec![StateBand {
                index: band.band_index(),
                level: "raid5".to_string(),
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
            filesystem: StateFilesystem {
                fs_uuid: Some("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d".to_string()),
                mount_point: "/mnt/shr_data".to_string(),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
            },
            expansion: StateExpansion::default(),
        };
        store.save(&shr_state::StateFile::new(vec![state])).unwrap();
    }

    #[test]
    fn preview_expand_create_band_step_also_includes_the_internal_bitmap() {
        // addendum: expand()'s CreateBand step (disks too large to just
        // grow an existing band, so a brand new mdadm array is created)
        // calls the exact same `MdadmExecutor::create_array` as `create()`'s
        // initial bands -- must show the bitmap in preview here too.
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_three_disk_group(&store, "shr1");

        let req = ExpandRequest {
            name: None,
            new_disks: vec![
                disk("ata-6TB-A", "sde", 6_000_000_000_000),
                disk("ata-6TB-B", "sdf", 6_000_000_000_000),
            ],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        };

        let (state, commands) = preview_expand(store, req).unwrap();

        assert_eq!(state.bands.len(), 2, "expected a new band1 to be planned: {:?}", state.bands);
        let create_cmds: Vec<&String> = commands.iter().filter(|c| c.contains("mdadm --create")).collect();
        assert_eq!(create_cmds.len(), 1, "{commands:?}");
        assert!(create_cmds[0].contains("--bitmap=internal"), "{create_cmds:?}");
    }

    /// A minimal "real system" double for `status_runner` tests: answers
    /// every read `expand()`'s validation checks make, with everything
    /// healthy EXCEPT `sync_action`, which reports a scrub genuinely
    /// running (`"check"`) -- unlike `DryRunRunner`, this actually answers
    /// instead of fabricating a fixed "idle"/"0" via an `is_dry_run()`
    /// shortcut.
    struct ScrubRunningStatusRunner;
    impl CommandRunner for ScrubRunningStatusRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<shr_exec::CommandOutput, shr_exec::ExecError> {
            let stdout = if program == "cat" && args.iter().any(|a| a.ends_with("/md/sync_action")) {
                "check\n".to_string()
            } else if program == "cat"
                && args.iter().any(|a| a.ends_with("/md/degraded") || a.ends_with("/md/mismatch_cnt"))
            {
                "0\n".to_string()
            } else {
                String::new()
            };
            Ok(shr_exec::CommandOutput { stdout, stderr: String::new() })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    #[test]
    fn preview_expand_against_a_real_status_runner_reports_a_running_scrub_not_e19s_stale_message() {
        // `preview_expand`'s plain `DryRunRunner`
        // can never see a scrub genuinely running on the real array --
        // `MdadmExecutor::sync_action`'s `is_dry_run()` shortcut always
        // fabricates `"idle"`, so this validation fell through to the
        // stale "no scrub completed yet" message even while a real scrub
        // was running, and `shr-cli`'s real (non-dry-run) `expand` handler
        // calls this preview FIRST -- so that wrong error killed the whole
        // command before the real `engine.expand()` call (which reads the
        // real system) ever ran. A real `status_runner` must fix this.
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        seed_three_disk_group(&store, "shr1");

        let req = ExpandRequest {
            name: None,
            new_disks: vec![
                disk("ata-6TB-A", "sde", 6_000_000_000_000),
                disk("ata-6TB-B", "sdf", 6_000_000_000_000),
            ],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: false,
        };

        let status_runner = ScrubRunningStatusRunner;
        let err = preview_expand_against(store, req, Some(&status_runner)).unwrap_err();
        let msg = format!("{err}");
        // `sync_action=check` is caught by `expand()`'s earlier
        // background-activity guard (the same one `expand_is_blocked_
        // while_a_scrub_is_running` in `orchestrate.rs` exercises against
        // a real runner) -- a correct, truthful message either way. What
        // this test actually proves is that a real `status_runner` makes
        // `preview_expand_against` see it AT ALL: with `DryRunRunner`
        // alone (`preview_expand`, no status runner), this same scenario
        // would fabricate `sync_action == "idle"` and fall through to
        // the stale message instead.
        assert!(msg.contains("sync_action=check"), "{msg}");
        assert!(
            !msg.contains("has not been fully checked for errors"),
            "must not show the misleading staleness message during preview: {msg}"
        );
    }
}
