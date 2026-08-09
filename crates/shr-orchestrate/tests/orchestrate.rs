use shr_command::{AlwaysConfirmSink, AlwaysRejectConfirmSink, RecordingConfirmSink, RecordingProgressSink};
use shr_core::{DiskId, ExpansionStep, RaidLevel, RedundancyMode, RedundantBand};
use shr_exec::{
    CommandOutput, CommandRunner, DryRunRunner, ExecError, MetricsSampler, ReshapePriority, ReshapeThrottle,
    ThrottleDecision, ThrottleMetrics, RESHAPE_SPEED_FLOOR_KB, RESHAPE_SPEED_INITIAL_KB,
};
use shr_inspect::{resolve_disk_ref, ByIdIndex, DiskRef, ResolvedDisk};
use shr_orchestrate::{
    preview_destroy, CreateRequest, ExpandRequest, OrchestrateError, OrchestrationEngine, ReconcileAction,
    AUTO_SNAPSHOT_PREFIX,
};
use shr_state::conf::{scrub_unit_paths, write_scrub_timer_units};
use shr_state::{
    ArrayState, NotifyPolicy, ScrubOutcome, StateBand, StateCheckpoint, StateDisk, StateExpansion, StateFile,
    StateFilesystem, StatePartition, StatePendingDisk, StateScrubResult, StateStore,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::tempdir;

/// Shared across almost every test in this file: the engine's default
/// confirm sink is fail-closed (`AlwaysRejectConfirmSink`, see
/// `OrchestrationEngine::new`'s doc comment), so any test here that drives a
/// REAL (non-`DryRunRunner`) `create`/`expand` to success has to opt in
/// explicitly, exactly like `shr-cli` does. Tests that exist specifically to
/// exercise reject/confirm semantics wire their own `RecordingConfirmSink`/
/// `AlwaysRejectConfirmSink` instead and don't use this.
static ALWAYS_CONFIRM: AlwaysConfirmSink = AlwaysConfirmSink;

/// A `CommandRunner` that behaves like a real (non-dry-run) system for
/// command-string-recording purposes, but never touches anything real, and
/// can be told to fail on the first call matching a substring (`fail_once`)
/// and/or on every call matching another substring (`fail_forever` --
/// used to simulate a rollback command itself failing, e.g. "device busy").
///
/// Used to test D10 (transactional rollback) and D11 (prerequisite checks
/// run before any destructive command) without needing a real Linux host:
/// `is_dry_run() == false` so every executor takes its real-execution
/// branch (unlike `DryRunRunner`, which never fails and skips read
/// simulation), while `run()` never actually spawns a process.
struct FailingRunner {
    recorded: Mutex<Vec<String>>,
    fail_once_trigger: Option<String>,
    fail_once_used: Mutex<bool>,
    fail_forever_trigger: Option<String>,
    filesystems_content: String,
    /// If set, only this md device (matched by substring against the
    /// `/sys/block/<md>/md/degraded` path) reports degraded; every other
    /// reports healthy. `None` means every md reports healthy.
    degraded_only_for: Option<String>,
    /// Boundary: if set, `cat /sys/block/<md>/md/degraded` for this md
    /// (matched by substring, same convention as `degraded_only_for`)
    /// SUCCEEDS (exit 0 -- the array IS assembled) but reports unparseable
    /// content instead of a digit -- `degraded_count`'s own `Prerequisite`
    /// error, structurally distinct from the real guest's "no such file"
    /// `NonZeroExit`. Checked before `degraded_only_for`; `None` (default)
    /// means every md's degraded read parses cleanly.
    degraded_unparseable_for: Option<String>,
    /// If set, `cat /sys/block/<md>/md/degraded` for this md (matched
    /// by substring) fails with the EXACT real-guest stderr -- `cat: <path>:
    /// No such file or directory` -- rather than `fail_forever_trigger`'s
    /// generic "simulated failure" text, so a test can prove
    /// `check_health`'s discriminated match actually keys off the real
    /// error shape, not any failure. `None` (default): no md's degraded
    /// read fails this way.
    degraded_array_missing_for: Option<String>,
    /// Pre-grow `MD_LEVEL`/`MD_DEVICES` to report from `mdadm --detail
    /// --export` -- used by execute_grow's F4 post-failure verification.
    /// Defaults to three_disks()'s band0 shape (raid5, 3 members).
    pre_grow_level: String,
    pre_grow_devices: u32,
    /// What `pvs --noheadings -o vg_name <path>` reports -- "" (not in any
    /// VG) unless a test needs to simulate a `vgextend` that partially
    /// committed despite reporting failure (F3).
    pv_vg_name_response: String,
    /// What `cat /sys/block/<md>/md/sync_action` reports for an md device
    /// that has actually had `mdadm --grow` issued against it (tracked via
    /// `grown_mds`) -- every OTHER md device always reports "idle", same as
    /// real mdadm before any `--grow` ever ran on it.
    sync_action_response: String,
    grown_mds: Mutex<std::collections::HashSet<String>>,
    /// Md devices `MdadmExecutor::replace_member` has been issued
    /// against (intercepted from the literal `mdadm <md> --replace ...
    /// --with ...` command) -- like `grown_mds`, but for the live-replace
    /// background copy instead of a `--grow` reshape, so a test can make
    /// `sync_action` report `sync_action_response` (e.g. "recover") for a
    /// `--replace` in progress without also having to fake a `--grow`.
    replacing_mds: Mutex<std::collections::HashSet<String>>,
    /// Flips every grown md's `sync_action` back to "idle" -- simulates a
    /// reshape that was running finishing on its own, for `reconcile()`
    /// tests. See `finish_reshape`.
    reshape_finished: Mutex<bool>,
    /// If true, every `mdadm --detail --export` call AFTER the first
    /// `mdadm --grow` fails -- simulates a transient read failure on the
    /// post-grow md_uuid re-read (an earlier review finding: this must never
    /// null out a band's already-known-good `md_uuid`).
    fail_mdadm_export_after_grow: bool,
    grow_seen: Mutex<bool>,
    /// What `cat /proc/mdstat` reports -- simulates `/dev/mdN` device(s)
    /// that already exist on the HOST, whether or not shr-rs's own
    /// `state.toml` knows about them (a foreign array, a leftover from a
    /// prior install, the kernel's own auto-assembly as `md127`, etc.).
    /// Empty by default, matching a host with no md arrays at all.
    mdstat_content: Mutex<String>,
    /// What `cat /proc/loadavg` reports for `LiveMetricsSampler` --
    /// empty by default (unparseable, `cpu_load` comes back `None`), unless
    /// a test opts into simulating a fully healthy, readable system via
    /// `healthy_with_live_metrics`.
    loadavg_response: String,
    /// Successive `cat /proc/stat` responses, one per call --
    /// `LiveMetricsSampler::sample` reads it TWICE (to diff), so a test that
    /// wants a specific `io_wait_pct` must supply two distinct lines here.
    /// Empty (falls through to `stat_call_count`'s default "" and an
    /// unparseable `None`) unless populated.
    stat_responses: Mutex<Vec<String>>,
    /// What `smartctl -j ...` reports for every member disk -- empty
    /// (unparseable JSON -> `None`) unless a test opts in.
    smartctl_response: String,
    /// Md devices `MdadmExecutor::scrub_start` has written
    /// `check` to (intercepted from the literal `sh -c echo check > ...`
    /// command, the same convention Stage B's throttle writes use) and
    /// `scrub_cancel`/`finish_scrub` hasn't cleared yet -- `sync_action`
    /// reports `"check"` for these, taking priority over `grown_mds`
    /// (a band can't be reshaping and scrubbing at once, but the test
    /// double doesn't need to enforce that itself).
    scrubbing_mds: Mutex<std::collections::HashSet<String>>,
    /// What `cat /sys/block/<md>/md/mismatch_cnt` reports.
    mismatch_cnt_response: String,
    /// What `btrfs scrub status <mount>` reports -- empty (parses
    /// as not-running, 0 errors) unless a test opts in.
    btrfs_scrub_status_response: String,
    /// When true, `btrfs scrub
    /// cancel <mount>` fails with the exact real-guest message -- "ERROR:
    /// scrub cancel failed on <mount>: not running" -- simulating Btrfs's
    /// half of a scrub having already finished on its own while mdadm's
    /// `check` half is still running.
    btrfs_scrub_cancel_reports_not_running: bool,
    /// What `readlink -e <by-partuuid path>` reports for `old_disk`'s
    /// member during `replace_disk`. `Some(name)` simulates the symlink
    /// still resolving to kernel device `name` (old disk physically
    /// present); `None` (the default) simulates a dangling symlink -- old
    /// disk physically removed -- matching `MdadmExecutor::
    /// resolve_member_kernel_name`'s `-e` (not `-f`) choice, which requires
    /// the target to actually exist.
    readlink_kernel_name: Option<String>,
    /// Per-disk `readlink -e /dev/disk/by-id/<id>` responses, keyed by
    /// the by-id NAME in the queried path -- distinct per disk, unlike
    /// `readlink_kernel_name` above (a single global value the pre-existing
    /// by-partuuid checks use). Lets a test prove `--old`'s live
    /// kernel-name fallback resolved the SPECIFIC disk whose symlink
    /// matched, not just that some disk in the group happened to be first.
    /// Checked before `readlink_kernel_name` below; empty (falls through)
    /// unless a test opts in.
    by_id_kernel_names: std::collections::HashMap<String, String>,
    /// By-partuuid/device paths a `mdadm <md> --remove <path>`
    /// command has been issued against. real-guest repro: `--remove`
    /// detaches a member but does NOT delete its partition, so
    /// `readlink -e <path>` keeps resolving fine (`readlink_kernel_name`
    /// is untouched by this set -- see the `readlink` handler below) --
    /// the only thing that actually changes is `/proc/mdstat`'s member
    /// list, simulated by `rendered_mdstat` filtering the removed kernel
    /// name back out of `mdstat_content`.
    removed_member_paths: Mutex<std::collections::HashSet<String>>,
    /// What `ls -1 <dir>` reports -- used to simulate
    /// `BtrfsExecutor::list_snapshot_names`'s view of a group's
    /// `@snapshots` directory for `prune_group_snapshots` tests. Empty
    /// (no entries) unless a test opts in.
    ls_response: String,
    /// If set, `vgs --noheadings -o vg_name <name>` succeeds (i.e. the
    /// VG already exists on the host) only when `<name>` matches this --
    /// every other `vgs` call fails (not found), matching real `vgs`'s exit
    /// code on an unknown name. `None` (the default) means no VG exists
    /// anywhere, matching a freshly-imaged host with no LVM state.
    existing_vg_name: Option<String>,
    /// Same idea as `existing_vg_name`, for `lvs --noheadings -o
    /// lv_name <vg>/<lv>` -- `Some("vg/lv")` only.
    existing_lv_target: Option<String>,
    /// Regression guard: md device names (e.g. `"md7"`, no `/dev/` prefix) a
    /// `mdadm --stop <path>` command has actually succeeded against.
    /// `rendered_mdstat` drops that array's line entirely once its name is
    /// in this set -- the ONLY way `holder_md_array`'s live-verification
    /// re-read (the lesson: don't trust `mdadm --stop`'s exit code alone)
    /// can observe a stop as having actually taken effect. Without this,
    /// `mdstat_content` would stay static forever regardless of how many
    /// `--stop` calls are issued, which would make the engine's own
    /// verify-after-stop check (correctly) treat every stop as unconfirmed.
    stopped_mds: Mutex<std::collections::HashSet<String>>,
    /// Trap test for the re-fix: when true, a successful `mdadm --stop`
    /// (exit 0, no failure trigger matched) does NOT get reflected into
    /// `stopped_mds` / `rendered_mdstat` -- simulating a stop that reports
    /// success but did not actually take effect. Used only to prove the
    /// engine's post-stop re-verification (rather than trusting the exit
    /// code) refuses to proceed in that case.
    stop_does_not_take_effect: bool,
    /// If `mount`'s target path (either the device or the mount
    /// point argument) contains this substring, `mount` fails with the
    /// exact real-guest shape of an unassembled array's LV -- exit 32,
    /// `mount: <point>: special device <dev> does not exist.` -- instead
    /// of the default success. Scoped by substring (not "every mount
    /// fails") so a multi-group `snapshot_auto_run` test can make ONE
    /// group's scratch mount fail while another group's still succeeds.
    /// `None` (default) means every mount succeeds, as before.
    mount_missing_device_for: Option<String>,
    /// If `mount`'s mount-point argument contains this
    /// substring, `mount` fails with the OTHER real exit-32 shape -- `mount:
    /// <point>: mount point does not exist.` -- distinct from
    /// `mount_missing_device_for`'s "special device ... does not exist."
    /// Used to prove `is_missing_array_device` does NOT treat a missing
    /// scratch directory as "array not assembled" (see
    /// `mount_point_missing_is_not_treated_as_absent_array`). `None`
    /// (default) means every mount succeeds, as before.
    mount_missing_mountpoint_for: Option<String>,
    /// Kernel name whose `lsblk -n -o MOUNTPOINT /dev/<name>` reports a
    /// SYSTEM mountpoint, as it does for a disk that has become part of the
    /// OS's storage since preflight ran. Exists to drive
    /// `reverify_targets`'s live system-disk gate, which reads that column
    /// precisely because it traces md and LVM stacking (`/proc/mounts`, what
    /// this check used to read, never names the underlying disk at all).
    /// `None` (default) leaves lsblk's answer empty, i.e. nothing mounted.
    live_system_mount_on: Option<String>,
}

impl FailingRunner {
    fn healthy() -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
            fail_once_trigger: None,
            fail_once_used: Mutex::new(false),
            fail_forever_trigger: None,
            filesystems_content: "nodev\tsysfs\nbtrfs\n".to_string(),
            degraded_only_for: None,
            degraded_unparseable_for: None,
            degraded_array_missing_for: None,
            pre_grow_level: "raid5".to_string(),
            pre_grow_devices: 3,
            pv_vg_name_response: String::new(),
            sync_action_response: "idle".to_string(),
            grown_mds: Mutex::new(std::collections::HashSet::new()),
            replacing_mds: Mutex::new(std::collections::HashSet::new()),
            reshape_finished: Mutex::new(false),
            fail_mdadm_export_after_grow: false,
            grow_seen: Mutex::new(false),
            mdstat_content: Mutex::new(String::new()),
            loadavg_response: String::new(),
            stat_responses: Mutex::new(Vec::new()),
            smartctl_response: String::new(),
            scrubbing_mds: Mutex::new(std::collections::HashSet::new()),
            mismatch_cnt_response: "0\n".to_string(),
            btrfs_scrub_status_response: String::new(),
            btrfs_scrub_cancel_reports_not_running: false,
            readlink_kernel_name: None,
            by_id_kernel_names: std::collections::HashMap::new(),
            removed_member_paths: Mutex::new(std::collections::HashSet::new()),
            ls_response: String::new(),
            existing_vg_name: None,
            existing_lv_target: None,
            stopped_mds: Mutex::new(std::collections::HashSet::new()),
            stop_does_not_take_effect: false,
            mount_missing_device_for: None,
            mount_missing_mountpoint_for: None,
            live_system_mount_on: None,
        }
    }

    /// Simulate a scrub finishing on its own (kernel transitions
    /// `sync_action` back to `idle` with no explicit `scrub_cancel` call) --
    /// mirrors `finish_reshape`'s role for reshape tests.
    fn finish_scrub(&self) {
        self.scrubbing_mds.lock().unwrap().clear();
    }

    fn finish_reshape(&self) {
        *self.reshape_finished.lock().unwrap() = true;
    }

    fn failing_once_on(trigger: impl Into<String>) -> Self {
        Self {
            fail_once_trigger: Some(trigger.into()),
            ..Self::healthy()
        }
    }

    fn failing_once_and_forever(once: impl Into<String>, forever: impl Into<String>) -> Self {
        Self {
            fail_once_trigger: Some(once.into()),
            fail_forever_trigger: Some(forever.into()),
            ..Self::healthy()
        }
    }

    fn without_btrfs() -> Self {
        Self {
            filesystems_content: "nodev\tsysfs\next4\n".to_string(),
            ..Self::healthy()
        }
    }

    fn degraded_band(md_name: impl Into<String>) -> Self {
        Self {
            degraded_only_for: Some(md_name.into()),
            ..Self::healthy()
        }
    }

    /// `md_name` is degraded (like `degraded_band`), but the failed
    /// member is `faulty_kernel_name`, NOT `old_disk`'s own member (which
    /// this simulates as still healthy and present, resolving fine via
    /// `readlink -e` to `old_kernel_name`) -- proves `replace_disk` blocks
    /// on a failure it doesn't already explain, rather than blocking on
    /// ANY degraded band regardless of which member caused it.
    ///
    /// Deliberately does NOT set `mdstat_content` here (unlike
    /// `host_has_md_numbers`): this runner is shared with `seeded_engine`'s
    /// own `create()` call, which scans `/proc/mdstat` via `host_md_numbers`
    /// to avoid allocating an already-used `mdN` -- if the mocked mdstat
    /// already claimed `md_name` at construction time, `create()` would
    /// itself skip it and allocate `md1` instead, silently invalidating
    /// `md_name`. `readlink_kernel_name` has no such hazard (`create()`
    /// never calls `readlink`), so it's safe to set right away. Callers
    /// must set the mdstat content via `set_mdstat_content` AFTER seeding,
    /// once the band's real (allocated) `md_name` is known.
    fn degraded_by_a_different_member(
        md_name: impl Into<String>,
        old_kernel_name: impl Into<String>,
    ) -> Self {
        Self {
            degraded_only_for: Some(md_name.into()),
            readlink_kernel_name: Some(old_kernel_name.into()),
            ..Self::healthy()
        }
    }

    /// Set what `cat /proc/mdstat` reports, after construction -- see
    /// `degraded_by_a_different_member`'s doc comment for why this can't
    /// just be a constructor field for that test.
    fn set_mdstat_content(&self, content: impl Into<String>) {
        *self.mdstat_content.lock().unwrap() = content.into();
    }

    /// `mdstat_content` as currently reported by `cat /proc/mdstat`, with
    /// two layers of live mutation applied on top of the static fixture:
    ///
    /// 1. Any array whose name is in `stopped_mds` (regression guard: a
    ///    successful `mdadm --stop <name>`) has its ENTIRE block (header
    ///    line plus indented detail line(s)) dropped -- a stopped array
    ///    doesn't appear in `/proc/mdstat` at all, matching real mdadm.
    ///    This is the only way `holder_md_array`'s post-stop
    ///    re-verification (don't trust `mdadm --stop`'s exit code
    ///    alone) can ever observe a stop as having actually taken effect.
    /// 2. `readlink_kernel_name`'s token is dropped from every remaining
    ///    array line once its device has actually been `--remove`d (
    ///    `readlink` itself keeps resolving, matching a real kernel where
    ///    the partition is never deleted -- only mdstat's member list
    ///    changes).
    ///
    /// Tests that want a specific post-mutation shape (e.g. a differing
    /// block count) can still set `mdstat_content` to the exact final state
    /// directly instead of relying on either filter.
    fn rendered_mdstat(&self) -> String {
        let content = self.mdstat_content.lock().unwrap().clone();

        let stopped = self.stopped_mds.lock().unwrap().clone();
        let content = if stopped.is_empty() {
            content
        } else {
            let mut out_lines: Vec<&str> = Vec::new();
            let mut skipping = false;
            for line in content.lines() {
                let is_header = line.starts_with("md") && line.contains(" : ");
                if is_header {
                    let name = line.split_whitespace().next().unwrap_or("");
                    skipping = stopped.contains(name);
                    if skipping {
                        continue;
                    }
                } else if skipping {
                    // Still part of the block being dropped (mdstat's
                    // indented detail/sync-progress lines) until the next
                    // header, a blank line, or the trailing
                    // "unused devices" line.
                    if line.trim().is_empty() || line.starts_with("unused devices") {
                        skipping = false;
                    } else {
                        continue;
                    }
                }
                out_lines.push(line);
            }
            out_lines.join("\n")
        };

        let Some(kernel_name) = &self.readlink_kernel_name else {
            return content;
        };
        if self.removed_member_paths.lock().unwrap().is_empty() {
            return content;
        }
        content
            .lines()
            .map(|line| {
                if line.starts_with("md") && line.contains(" : ") {
                    line.split_whitespace()
                        .filter(|tok| {
                            *tok != kernel_name.as_str() && !tok.starts_with(&format!("{kernel_name}["))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn vgextend_fails_but_pv_already_joined(vg_name: impl Into<String>) -> Self {
        Self {
            fail_once_trigger: Some("vgextend".to_string()),
            pv_vg_name_response: vg_name.into(),
            ..Self::healthy()
        }
    }

    fn reshaping() -> Self {
        Self {
            sync_action_response: "reshape".to_string(),
            ..Self::healthy()
        }
    }

    /// A reshaping array on a host where every `LiveMetricsSampler`
    /// signal is actually readable and reports a healthy, idle system --
    /// used to prove the default live sampler doesn't just blindly
    /// decelerate when it CAN read real telemetry (contrast with
    /// `reshaping()`, whose unscripted proc/smartctl responses are
    /// unparseable and so correctly trigger the "unknown -> decelerate"
    /// path instead).
    fn reshaping_with_healthy_live_metrics() -> Self {
        Self {
            sync_action_response: "reshape".to_string(),
            // cpu_load = 0.60 -- squarely in ReshapeThrottle::tick's "Hold"
            // band (not `< 0.5`, so the idle/Increase branch never fires;
            // not `> 0.85`, so Decrease never fires from load alone).
            loadavg_response: "0.60 0.58 0.55 1/1 1\n".to_string(),
            // total delta = (1010+1010+8080+220) - (1000+1000+8000+200) = 120
            // iowait delta = 220-200 = 20 -> 20/120*100 ~= 16.67%, inside
            // [15, 30]: not `< 15` (so not idle) and not `> 30` (so not
            // over-threshold) -- also squarely in the Hold band.
            stat_responses: Mutex::new(vec![
                "cpu  1000 0 1000 8000 200 0 0 0\n".to_string(),
                "cpu  1010 0 1010 8080 220 0 0 0\n".to_string(),
            ]),
            smartctl_response: r#"{"smart_status":{"passed":true},"temperature":{"current":35},
              "ata_smart_attributes":{"table":[{"id":5,"raw":{"value":0}}]}}"#
                .to_string(),
            ..Self::healthy()
        }
    }

    fn export_fails_after_grow() -> Self {
        Self {
            fail_mdadm_export_after_grow: true,
            ..Self::healthy()
        }
    }

    /// Simulate a host where `/dev/mdN` already exists for every number in
    /// `numbers`, entirely independent of shr-rs's own `state.toml` -- the
    /// host-wide md-name-collision scenario `host_md_numbers` exists to
    /// guard against.
    fn host_has_md_numbers(numbers: &[u32]) -> Self {
        let mut content = String::from("Personalities : [raid6] [raid5] [raid1]\n");
        for n in numbers {
            content.push_str(&format!(
                "md{n} : active raid5 sda1[0] sdb1[1] sdc1[2]\n      \
                 1953260032 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/3] [UUU]\n\n"
            ));
        }
        content.push_str("unused devices: <none>\n");
        Self {
            mdstat_content: Mutex::new(content),
            ..Self::healthy()
        }
    }

    /// Simulate a host where LVM volume group `vg_name` already
    /// exists, independent of anything `state.toml` knows about.
    fn vg_already_exists(vg_name: impl Into<String>) -> Self {
        Self {
            existing_vg_name: Some(vg_name.into()),
            ..Self::healthy()
        }
    }

    /// Simulate a host where logical volume `vg_name`/`lv_name`
    /// already exists (defense-in-depth half of the guard -- this
    /// combination, an existing LV inside a VG name `create()` doesn't
    /// otherwise think exists, can't occur from a real `create()`'s own
    /// past runs, but a future caller that reuses an already-existing VG
    /// must still be caught by the LV check on its own).
    fn lv_already_exists(vg_name: impl Into<String>, lv_name: impl Into<String>) -> Self {
        Self {
            existing_lv_target: Some(format!("{}/{}", vg_name.into(), lv_name.into())),
            ..Self::healthy()
        }
    }

    /// Regression guard repro: a foreign/auto-assembled array (`holder_md`,
    /// never recorded in THIS `create()` attempt's own undo journal)
    /// already holds member partition `holder_member` per `/proc/mdstat`
    /// -- simulating udev incremental assembly resurrecting a residual
    /// superblock the INSTANT `parted mkpart` recreates the partition,
    /// well before `create()` gets a chance to zero anything. No failure
    /// trigger: this constructor is for proving `create()` detects the
    /// holder, confirms it's made purely of this request's own new
    /// partitions, stops it, verifies the stop, and SUCCEEDS end to end --
    /// not merely rolls back cleanly.
    fn holder_array_of_own_partitions(holder_md: &str, holder_member: &str) -> Self {
        Self {
            mdstat_content: Mutex::new(single_member_mdstat(holder_md, holder_member)),
            ..Self::healthy()
        }
    }

    /// Like `holder_array_of_own_partitions`, but the holder array ALSO has
    /// a member (`foreign_member`) that is NOT one of this request's own
    /// new partitions -- simulating an operator reusing only SOME of a
    /// previously-`destroy`d group's disks, where udev resurrects the
    /// OLD, larger array from a mix of newly re-created and still-
    /// untouched partitions. Used to prove `create()` refuses to stop it
    /// (a hard validation error, not a silent stop) instead of touching a
    /// disk this request never confirmed anything about.
    fn foreign_holder_blocks_create(holder_md: &str, target_member: &str, foreign_member: &str) -> Self {
        let content = format!(
            "Personalities : [raid1]\n{holder_md} : active raid1 {foreign_member}[1] \
             {target_member}[0]\n      1000000 blocks super 1.2 [2/2] [UU]\n\
             unused devices: <none>\n"
        );
        Self {
            mdstat_content: Mutex::new(content),
            ..Self::healthy()
        }
    }

    /// Like `holder_array_of_own_partitions`, but `mdadm --stop` "succeeds"
    /// (exit 0) without the mock's `/proc/mdstat` ever actually reflecting
    /// it -- the earlier trap: a command exiting 0 is not proof the kernel
    /// state changed. Used to prove `create()`'s post-stop re-verification
    /// refuses to proceed, rather than trusting the exit code alone.
    fn holder_stop_does_not_take_effect(holder_md: &str, holder_member: &str) -> Self {
        Self {
            mdstat_content: Mutex::new(single_member_mdstat(holder_md, holder_member)),
            stop_does_not_take_effect: true,
            ..Self::healthy()
        }
    }

    fn get_recorded(&self) -> Vec<String> {
        self.recorded.lock().unwrap().clone()
    }
}

/// A minimal single-member `/proc/mdstat` fixture: one RAID1 array
/// (`md_name`) whose sole member is `member`. Shared by the
/// holder-array test constructors above.
fn single_member_mdstat(md_name: &str, member: &str) -> String {
    format!(
        "Personalities : [raid1]\n{md_name} : active raid1 {member}[0]\n      \
         1000000 blocks super 1.2 [1/1] [U]\nunused devices: <none>\n"
    )
}

/// Like `single_member_mdstat`, but a self-contained two-member RAID1 --
/// the `execute_create_band` test needs this since a brand-new band always
/// has 2+ members, unlike `execute_grow`'s single new member.
fn two_member_mdstat(md_name: &str, member_a: &str, member_b: &str) -> String {
    format!(
        "Personalities : [raid1]\n{md_name} : active raid1 {member_b}[1] {member_a}[0]\n      \
         1000000 blocks super 1.2 [2/2] [UU]\nunused devices: <none>\n"
    )
}

/// Extract `md0` out of a literal `sh -c echo VALUE > /sys/block/md0/md/sync_action`
/// command string -- the same `write_sysfs` convention Stage B settled on
/// (see `shr_exec::cmd::write_sysfs`'s doc comment).
fn md_name_from_sync_action_command(cmd_str: &str) -> Option<String> {
    let after = cmd_str.split("/sys/block/").nth(1)?;
    after.split("/md/sync_action").next().map(str::to_string)
}

fn fnv1a32(s: &str) -> u32 {
    let hash = s.bytes().fold(0x811c_9dc5_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    hash as u32
}

fn simulated_failure(program: &str) -> ExecError {
    ExecError::NonZeroExit {
        program: program.to_string(),
        exit_code: 1,
        stdout: String::new(),
        stderr: "simulated failure injected by test".to_string(),
    }
}

impl CommandRunner for FailingRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let cmd_str = format!("{} {}", program, args.join(" "));
        self.recorded.lock().unwrap().push(cmd_str.clone());

        if cmd_str.starts_with("mdadm --grow") {
            *self.grow_seen.lock().unwrap() = true;
            if let Some(md_arg) = args.iter().find(|a| a.starts_with("/dev/md")) {
                self.grown_mds
                    .lock()
                    .unwrap()
                    .insert(md_arg.trim_start_matches("/dev/").to_string());
            }
        }
        if cmd_str.starts_with("mdadm") && cmd_str.contains("--replace") && cmd_str.contains("--with") {
            if let Some(md_arg) = args.first().filter(|a| a.starts_with("/dev/md")) {
                self.replacing_mds
                    .lock()
                    .unwrap()
                    .insert(md_arg.trim_start_matches("/dev/").to_string());
            }
        }
        if cmd_str.starts_with("sh -c echo check >") && cmd_str.contains("/md/sync_action") {
            if let Some(md_name) = md_name_from_sync_action_command(&cmd_str) {
                self.scrubbing_mds.lock().unwrap().insert(md_name);
            }
        }
        if cmd_str.starts_with("sh -c echo idle >") && cmd_str.contains("/md/sync_action") {
            if let Some(md_name) = md_name_from_sync_action_command(&cmd_str) {
                self.scrubbing_mds.lock().unwrap().remove(&md_name);
            }
        }
        if self.fail_mdadm_export_after_grow
            && cmd_str.contains("--export")
            && *self.grow_seen.lock().unwrap()
        {
            return Err(simulated_failure(program));
        }
        if let Some(trigger) = &self.fail_forever_trigger {
            if cmd_str.contains(trigger.as_str()) {
                return Err(simulated_failure(program));
            }
        }
        if let Some(trigger) = &self.fail_once_trigger {
            if cmd_str.contains(trigger.as_str()) {
                let mut used = self.fail_once_used.lock().unwrap();
                if !*used {
                    *used = true;
                    return Err(simulated_failure(program));
                }
            }
        }

        // Only reached once every failure-injection check above has passed
        // -- i.e. this `--remove` is really going to succeed -- so a
        // failure-injected `--remove` (see `failing_once_on`) correctly
        // does NOT mark the target as gone.
        if cmd_str.starts_with("mdadm") && cmd_str.contains("--remove") {
            if let Some(path) = args.last() {
                self.removed_member_paths.lock().unwrap().insert(path.to_string());
            }
        }
        // Same "only reached once the command has really succeeded" rule,
        // for `mdadm --stop` (regression guard): a failure-injected stop must
        // NOT make `rendered_mdstat` pretend the array is gone. Also
        // skipped when `stop_does_not_take_effect` is set (trap test):
        // the command still returns success below, but the mock state is
        // deliberately left unchanged, to prove the engine's own
        // post-stop re-verification catches that instead of trusting the
        // exit code.
        if cmd_str.starts_with("mdadm --stop") && !self.stop_does_not_take_effect {
            if let Some(path) = args.last() {
                self.stopped_mds
                    .lock()
                    .unwrap()
                    .insert(path.trim_start_matches("/dev/").to_string());
            }
        }

        if program == "readlink" {
            // `--old`'s live kernel-name fallback queries a SPECIFIC
            // disk's by-id symlink -- resolve those per-path, before falling
            // through to the single global `readlink_kernel_name` the
            // pre-existing by-partuuid checks use.
            if let Some(path) = args.iter().find(|a| a.starts_with("/dev/disk/by-id/")) {
                let id = path.trim_start_matches("/dev/disk/by-id/");
                return match self.by_id_kernel_names.get(id) {
                    Some(kernel) => Ok(CommandOutput {
                        stdout: format!("/dev/{kernel}\n"),
                        stderr: String::new(),
                    }),
                    None => Err(simulated_failure(program)),
                };
            }
            // A `--remove`d member's partition is never deleted, so
            // `readlink -e` keeps resolving regardless of removal status --
            // ONLY `readlink_kernel_name` (a dangling by-partuuid symlink,
            // i.e. the disk physically gone) makes this fail.
            return match &self.readlink_kernel_name {
                Some(name) => Ok(CommandOutput {
                    stdout: format!("/dev/{name}\n"),
                    stderr: String::new(),
                }),
                None => Err(simulated_failure(program)),
            };
        }

        // `vgs`/`lvs` against a name that doesn't exist exit nonzero on
        // a real host ("Volume group \"x\" not found") -- default to that
        // ("nothing exists anywhere") unless a test opts in via
        // `existing_vg_name`/`existing_lv_target`, so every OTHER test
        // (which never configures either) sees `create()`'s new guard
        // pass through exactly as if no collision guard existed at all.
        if program == "vgs" {
            let requested = args.last().copied().unwrap_or("");
            return match &self.existing_vg_name {
                Some(name) if name == requested => Ok(CommandOutput {
                    stdout: format!("  {name}\n"),
                    stderr: String::new(),
                }),
                _ => Err(simulated_failure(program)),
            };
        }
        if program == "lvs" {
            let requested = args.last().copied().unwrap_or("");
            return match &self.existing_lv_target {
                Some(target) if target == requested => Ok(CommandOutput {
                    stdout: format!("  {target}\n"),
                    stderr: String::new(),
                }),
                _ => Err(simulated_failure(program)),
            };
        }
        // Layer 3: real-guest repro (reused loop13/loop14 after
        // `destroy` without `--zero-superblocks`) -- `lvcreate` lands the
        // new LV at the same offset the destroyed group's LV occupied,
        // libblkid still finds that old Btrfs signature, and with no
        // explicit wipe answer the non-interactive "Wipe it? [y/n]" prompt
        // defaults to "no": lvcreate leaves the signature and aborts with
        // exactly the transcript below. Measured on the real guest that
        // `--wipesignatures y` alone (`-Wy`) does NOT suppress the prompt;
        // only adding `--yes` did (three-way comparison, see
        // `LvmExecutor::lvcreate_max`'s doc comment) -- gate on `--yes`,
        // not on `-Wy`/`-Zy`, since those two were not individually proven
        // necessary. Modeled unconditionally (not gated to a "reused disk"
        // scenario) so every create() test in this file is itself an
        // regression guard -- the flag is a no-op on a clean device, so
        // this changes nothing about what a real lvcreate would do on the
        // happy path.
        if program == "lvcreate" {
            let confirmed_non_interactively = args.contains(&"--yes");
            if !confirmed_non_interactively {
                return Err(ExecError::NonZeroExit {
                    program: program.to_string(),
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "1 existing signature left on the device.\nFailed to wipe \
                             signatures on logical volume vg_gc/data.\nAborting. Failed to \
                             wipe start of new LV.\n"
                        .to_string(),
                });
            }
        }
        // Real-guest message a `btrfs scrub cancel` prints when Btrfs's
        // half of a scrub already finished on its own -- see
        // `btrfs_scrub_cancel_reports_not_running`'s doc comment.
        if program == "btrfs"
            && args.first() == Some(&"scrub")
            && args.get(1) == Some(&"cancel")
            && self.btrfs_scrub_cancel_reports_not_running
        {
            let mount = args.last().copied().unwrap_or("");
            return Err(ExecError::NonZeroExit {
                program: program.to_string(),
                exit_code: 2,
                stdout: String::new(),
                stderr: format!("ERROR: scrub cancel failed on {mount}: not running\n"),
            });
        }

        // See `mount_missing_device_for`'s doc comment.
        if program == "mount" {
            if let Some(target) = &self.mount_missing_device_for {
                if args.iter().any(|a| a.contains(target.as_str())) {
                    let dev_path = args.get(args.len().saturating_sub(2)).copied().unwrap_or("");
                    let mount_point = args.last().copied().unwrap_or("");
                    return Err(ExecError::NonZeroExit {
                        program: program.to_string(),
                        exit_code: 32,
                        stdout: String::new(),
                        stderr: format!("mount: {mount_point}: special device {dev_path} does not exist.\n"),
                    });
                }
            }
            // See `mount_missing_mountpoint_for`'s doc
            // comment -- the OTHER real exit-32 shape, which must NOT be
            // confused with the device-absent one above.
            if let Some(target) = &self.mount_missing_mountpoint_for {
                if args.iter().any(|a| a.contains(target.as_str())) {
                    let mount_point = args.last().copied().unwrap_or("");
                    return Err(ExecError::NonZeroExit {
                        program: program.to_string(),
                        exit_code: 32,
                        stdout: String::new(),
                        stderr: format!("mount: {mount_point}: mount point does not exist.\n"),
                    });
                }
            }
        }

        // Real-guest shape of `cat` on a not-assembled array's
        // `/sys/block/<md>/md/degraded` -- distinct from `fail_forever_
        // trigger`'s generic "simulated failure injected by test" stderr,
        // which does NOT contain "No such file or directory" and so must
        // NOT be treated as `ArrayMissing` by `check_health`'s discriminated
        // match. Scoped by substring the same way `degraded_only_for` is.
        if program == "cat" && args.iter().any(|a| a.ends_with("/md/degraded")) {
            if let Some(target) = &self.degraded_array_missing_for {
                let path = args
                    .iter()
                    .find(|a| a.ends_with("/md/degraded"))
                    .copied()
                    .unwrap_or("");
                if path.contains(target.as_str()) {
                    return Err(ExecError::NonZeroExit {
                        program: program.to_string(),
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: format!("cat: {path}: No such file or directory\n"),
                    });
                }
            }
        }

        let stdout = if program == "cat" && args.contains(&"/proc/filesystems") {
            self.filesystems_content.clone()
        } else if program == "cat" && args.contains(&"/proc/mdstat") {
            self.rendered_mdstat()
        } else if program == "cat" && args.contains(&"/proc/loadavg") {
            self.loadavg_response.clone()
        } else if program == "cat" && args.contains(&"/proc/stat") {
            let mut queue = self.stat_responses.lock().unwrap();
            if queue.is_empty() {
                String::new()
            } else {
                queue.remove(0)
            }
        } else if program == "smartctl" {
            self.smartctl_response.clone()
        } else if program == "cat" && args.iter().any(|a| a.ends_with("/md/degraded")) {
            let path = args
                .iter()
                .find(|a| a.ends_with("/md/degraded"))
                .copied()
                .unwrap_or("");
            match (&self.degraded_unparseable_for, &self.degraded_only_for) {
                (Some(target), _) if path.contains(target.as_str()) => "not-a-number\n".to_string(),
                (_, Some(target)) if path.contains(target.as_str()) => "1\n".to_string(),
                _ => "0\n".to_string(),
            }
        } else if program == "mdadm" && args.contains(&"--export") {
            format!(
                "MD_LEVEL={}\nMD_DEVICES={}\nMD_UUID=aaaaaaaa:bbbbbbbb:cccccccc:dddddddd\n",
                self.pre_grow_level, self.pre_grow_devices
            )
        } else if program == "pvs" && args.contains(&"vg_name") {
            self.pv_vg_name_response.clone()
        } else if program == "ls" {
            self.ls_response.clone()
        } else if program == "cat" && args.iter().any(|a| a.ends_with("/md/sync_action")) {
            let path = args
                .iter()
                .find(|a| a.ends_with("/md/sync_action"))
                .copied()
                .unwrap_or("");
            let md_name = path
                .trim_start_matches("/sys/block/")
                .trim_end_matches("/md/sync_action");
            if self.scrubbing_mds.lock().unwrap().contains(md_name) {
                "check".to_string()
            } else if (self.grown_mds.lock().unwrap().contains(md_name)
                || self.replacing_mds.lock().unwrap().contains(md_name))
                && !*self.reshape_finished.lock().unwrap()
            {
                self.sync_action_response.clone()
            } else {
                "idle".to_string()
            }
        } else if program == "cat" && args.iter().any(|a| a.ends_with("/md/mismatch_cnt")) {
            self.mismatch_cnt_response.clone()
        } else if program == "btrfs" && args.first() == Some(&"scrub") && args.get(1) == Some(&"status") {
            self.btrfs_scrub_status_response.clone()
        } else if program == "lsblk" && args.contains(&"MOUNTPOINT") {
            // `reverify_targets`'s live system-disk gate. The real column
            // prints one mountpoint per line, blank for every unmounted
            // node in the disk's holder tree.
            match (&self.live_system_mount_on, args.last()) {
                (Some(kernel), Some(dev)) if *dev == format!("/dev/{kernel}") => "\n\n/\n".to_string(),
                _ => String::new(),
            }
        } else if program == "blkid" {
            // Vary by target path so distinct partitions/filesystems don't
            // collide onto the same UUID (state.toml would then have
            // duplicate part_uuids, which a real system never produces).
            let target = args.last().copied().unwrap_or("");
            format!("{:08x}-0000-4000-8000-000000000000", fnv1a32(target))
        } else {
            String::new()
        };
        Ok(CommandOutput {
            stdout,
            stderr: String::new(),
        })
    }

    fn is_dry_run(&self) -> bool {
        false
    }
}

/// Build a `ResolvedDisk` by hand for tests that don't need to exercise the
/// full lsblk/by-id resolution pipeline (see `create_via_static_inspector...`
/// below for a test that does).
fn resolved_disk(id: &str, kernel: &str, size_bytes: u64) -> ResolvedDisk {
    ResolvedDisk {
        reference: DiskRef::Path(kernel.to_string()),
        kernel_name: kernel.to_string(),
        id: id.into(),
        size_bytes,
        serial: format!("SN-{kernel}"),
        model: "Test Disk".to_string(),
        system_mounts: vec![],
        has_content: false,
    }
}

fn three_disks() -> Vec<ResolvedDisk> {
    vec![
        resolved_disk("ata-DISK1", "sdb", 4_000_000_000_000),
        resolved_disk("ata-DISK2", "sdc", 4_000_000_000_000),
        resolved_disk("ata-DISK3", "sdd", 4_000_000_000_000),
    ]
}

/// The the design's canonical heterogeneous example: band0 spans all
/// 4 disks (RAID5), band1 spans only the two largest (RAID1), and the 6TB
/// disk's remaining 2TB is unusable (no band can form from a single disk).
fn hetero_disks() -> Vec<ResolvedDisk> {
    vec![
        resolved_disk("ata-3TB-A", "sdb", 3_000_000_000_000),
        resolved_disk("ata-3TB-B", "sdc", 3_000_000_000_000),
        resolved_disk("ata-4TB", "sdd", 4_000_000_000_000),
        resolved_disk("ata-6TB", "sde", 6_000_000_000_000),
    ]
}

/// `FailingRunner` reports `is_dry_run() == false` (it intentionally
/// exercises the "real execution" code paths) and, historically, didn't
/// sandbox raw `std::fs` calls the way it sandboxes `CommandRunner::run()`
/// -- `create()`'s mount-point `create_dir_all` used to be exactly such a
/// raw call, so every `FailingRunner`-based test using a fixed literal like
/// `/mnt/data` was creating a REAL directory on the host running `cargo
/// test` (confirmed: this had already left `C:\mnt\data` on the Windows dev
/// machine this project is built on). A later fix routed that call through
/// `self.runner` instead, closing the bypass at its source. This tempdir
/// indirection is kept anyway -- defense in depth against the same class of
/// bug recurring, not a required workaround anymore.
fn create_req(disks: Vec<ResolvedDisk>) -> CreateRequest {
    create_req_named("default", disks)
}

/// Like `create_req`, but for tests exercising multi-group behavior where
/// more than one group needs to exist in the same `state.toml` -- each
/// needs its own name (and, since they share the same fixed mount-point
/// literal below is per-call, its own subdirectory) to avoid colliding.
fn create_req_named(name: &str, disks: Vec<ResolvedDisk>) -> CreateRequest {
    CreateRequest {
        name: name.to_string(),
        mode: RedundancyMode::Shr,
        disks,
        vg_name: "shr_vg".to_string(),
        lv_name: "data".to_string(),
        mount_point: std::env::temp_dir()
            .join(format!("shr-rs-orchestrate-test-mount-{name}"))
            .to_string_lossy()
            .to_string(),
        compression: "zstd:3".to_string(),
        system_disks: vec!["sda".to_string()],
    }
}

#[test]
fn create_dry_run_never_creates_the_mount_point_directory_on_disk() {
    // An earlier review finding: `std::fs::create_dir_all(&req.mount_point)` was
    // a raw filesystem call, not routed through the runner like every other
    // side effect in `create()` -- it ran unconditionally, so `--dry-run`
    // silently created a REAL directory on disk. Confirmed concretely: this
    // bug had already created C:\mnt\data on the dev machine via the OTHER
    // dry-run test's hardcoded "/mnt/data" mount point running natively on
    // Windows. Use a tempdir-scoped path here so this test is portable and
    // actually proves the absence.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let mount_point = dir.path().join("would-be-mounted-here");
    let mut req = create_req(three_disks());
    req.mount_point = mount_point.to_string_lossy().to_string();

    engine.create(req).unwrap();

    assert!(
        !mount_point.exists(),
        "dry-run must never create the mount point directory on disk"
    );
}

/// `create()`'s mount-point directory, `create_snapshot_now`'s scratch
/// mount directory, and `prune_group_snapshots`' own separate scratch mount
/// directory were all raw `std::fs::create_dir_all` calls gated on
/// `!self.runner.is_dry_run()`. That guard does not make raw `std::fs`
/// mockable: `FailingRunner` (used by nearly every non-dry-run test in this
/// file, and every real, non-`--dry-run` invocation) reports `is_dry_run()
/// == false`, so the guard passed and the real filesystem call ran anyway --
/// confirmed leaving `C:\run\shr-rs\...` and `C:\var\lib\shr-rs` on the
/// Windows dev host from ordinary `cargo test` runs. Routed through
/// `self.runner.run("mkdir", &["-p", ..])` instead, so every directory
/// creation is now a recorded, mockable command like every other side
/// effect -- this test fails (red) against the old raw-`std::fs` code
/// because nothing about it was ever recorded in `get_recorded()`.
#[test]
fn directory_creation_for_mount_points_goes_through_the_runner_not_raw_fs() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);

    let req = create_req(three_disks());
    let mount_point = req.mount_point.clone();
    let created = engine.create(req).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c == &format!("mkdir -p {mount_point}")),
        "create()'s mount-point directory creation must be recorded by the runner: {cmds:?}"
    );

    state_store.save(&StateFile::new(vec![created])).unwrap();
    let mark = runner.get_recorded().len();
    engine.snapshot_create(None, "manual-check").unwrap();
    let snapshot_cmds = &runner.get_recorded()[mark..];
    assert!(
        snapshot_cmds
            .iter()
            .any(|c| c == "mkdir -p /run/shr-rs/snapshot-mount-default"),
        "create_snapshot_now's scratch mount directory creation must be recorded by the runner: \
         {snapshot_cmds:?}"
    );

    // `snapshot_auto_run` drives BOTH `create_snapshot_now` (a new
    // auto-snapshot) AND `prune_group_snapshots` (retention) on the same
    // scratch path, each its own separate mount/unmount cycle -- two
    // `mkdir -p` calls, one per call site.
    let auto_mark = runner.get_recorded().len();
    engine.snapshot_auto_run(7).unwrap();
    let auto_cmds = &runner.get_recorded()[auto_mark..];
    let mkdir_count = auto_cmds
        .iter()
        .filter(|c| c.as_str() == "mkdir -p /run/shr-rs/snapshot-mount-default")
        .count();
    assert_eq!(
        mkdir_count, 2,
        "expected one mkdir from create_snapshot_now and one from prune_group_snapshots: {auto_cmds:?}"
    );
}

#[test]
fn create_array_pipeline_dry_run() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();

    let engine = OrchestrationEngine::new(&runner, state_store.clone());

    let state = engine.create(create_req(three_disks())).unwrap();

    assert_eq!(state.mode, "shr");
    assert_eq!(state.disks.len(), 3);
    assert_eq!(state.bands.len(), 1);
    assert_eq!(state.bands[0].level, "raid5");

    // D3: identifiers must be real (or, under dry-run, an explicit
    // structurally-valid simulation) -- never the old hardcoded placeholder
    // shape. Check the *shape* directly rather than excluding one literal
    // (an earlier review finding: `assert_ne!` against a single value doesn't
    // prove the general placeholder shape is gone).
    let md_uuid = state.bands[0].md_uuid.as_ref().unwrap();
    assert!(looks_like_real_md_uuid(md_uuid), "not MD_UUID-shaped: {md_uuid}");
    let fs_uuid = state.filesystem.fs_uuid.as_ref().unwrap();
    assert!(looks_like_real_fs_uuid(fs_uuid), "not UUID-shaped: {fs_uuid}");
    for d in &state.disks {
        assert!(!d.id.starts_with("disk-"), "id must be real by-id, got {}", d.id);
    }
    assert_eq!(state.disks[0].id, "ata-DISK1");
    assert_eq!(state.disks[0].serial.as_deref(), Some("SN-sdb"));
    assert_eq!(state.disks[0].model.as_deref(), Some("Test Disk"));

    assert!(
        !state_store.exists(),
        "dry-run must never create persistent array state"
    );

    let cmds = runner.get_recorded();
    assert!(!cmds.is_empty());
    assert!(cmds.iter().any(|c| c.contains("parted")));
    assert!(cmds.iter().any(|c| c.contains("mdadm --create")));
    assert!(cmds.iter().any(|c| c.contains("vgcreate")));
    assert!(cmds.iter().any(|c| c.contains("mkfs.btrfs")));

    // An earlier review finding: nothing previously proved partitioning targets
    // the stable by-id path (the design) rather than the unstable
    // /dev/sdX kernel enumeration -- reverting engine.rs to use kernel paths
    // kept every other assertion here green.
    assert!(
        cmds.iter().any(|c| c.contains("ata-DISK1")),
        "expected parted commands to reference the by-id path, got: {cmds:?}"
    );
}

#[test]
fn create_rejects_a_disk_with_no_usable_capacity_in_this_layout() {
    // An earlier review finding: a disk small enough that reserved_head +
    // reserved_tail + band_alignment rounding leaves it with 0 usable bytes
    // never appears in ANY band's membership (not even as an "unusable
    // tail" warning -- that only fires for disks that participate in at
    // least one candidate slice). Before this fix, the engine still wiped
    // such a disk's GPT and recorded it in state.toml with an empty
    // partition list, for zero benefit -- silently discarding a disk the
    // user explicitly asked to include. It must be rejected before any
    // destructive command runs, not after.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    // align_down(2GiB - 128MiB - 8MiB, 4GiB) == 0 -- this disk can never be
    // a member of any band regardless of what else is in the request.
    let mut disks = three_disks();
    disks.push(resolved_disk("ata-TOO-SMALL", "sde", 2 * 1024 * 1024 * 1024));

    let err = engine.create(create_req(disks)).unwrap_err();
    assert!(
        format!("{err}").contains("ata-TOO-SMALL"),
        "error should name the disk that has no usable capacity: {err}"
    );
    assert!(
        runner.get_recorded().is_empty(),
        "must fail before issuing any destructive command, got: {:?}",
        runner.get_recorded()
    );
}

fn looks_like_real_md_uuid(v: &str) -> bool {
    let groups: Vec<&str> = v.split(':').collect();
    groups.len() == 4
        && groups
            .iter()
            .all(|g| g.len() == 8 && g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn looks_like_real_fs_uuid(v: &str) -> bool {
    v.len() == 36
        && [8usize, 13, 18, 23].into_iter().all(|i| v.as_bytes()[i] == b'-')
        && v.bytes()
            .enumerate()
            .all(|(i, b)| [8, 13, 18, 23].contains(&i) || b.is_ascii_hexdigit())
}

#[test]
fn create_rejects_duplicate_disk_id() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let disks = vec![
        resolved_disk("ata-SAME-ID", "sdb", 4_000_000_000_000),
        resolved_disk("ata-SAME-ID", "sdc", 4_000_000_000_000),
    ];

    let err = engine.create(create_req(disks)).unwrap_err();
    assert!(format!("{err}").contains("duplicate disk id"));
}

#[test]
fn create_via_static_inspector_resolution_uses_real_by_id_and_metadata() {
    // End-to-end proof (not just "engine copies whatever struct it's given"):
    // run the actual shr-inspect resolution pipeline (lsblk fixture + by-id
    // index) that the CLI uses, and confirm the resulting StateDisk carries
    // the real by-id name and the fixture's serial/model -- not
    // engine-invented placeholders.
    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"sdb","size":4000000000000,"type":"disk","model":"WD Red Plus","serial":"WD-SERIAL-B"},
      {"name":"sdc","size":4000000000000,"type":"disk","model":"WD Red Plus","serial":"WD-SERIAL-C"},
      {"name":"sdd","size":4000000000000,"type":"disk","model":"Seagate IronWolf","serial":"ST-SERIAL-D"}
    ]}"#;
    let lsblk = shr_inspect::parse_lsblk(LSBLK).unwrap();
    let mut by_id = ByIdIndex::empty();
    by_id.insert("sdb", "ata-WDC_WD40EFPX_WD-SERIAL-B");
    by_id.insert("sdc", "ata-WDC_WD40EFPX_WD-SERIAL-C");
    by_id.insert("sdd", "ata-ST4000VN006_ST-SERIAL-D");

    let resolved: Vec<ResolvedDisk> = ["sdb", "sdc", "sdd"]
        .iter()
        .map(|k| resolve_disk_ref(&DiskRef::parse(k), &lsblk, &by_id).unwrap())
        .collect();

    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine.create(create_req(resolved)).unwrap();

    let sdb = state
        .disks
        .iter()
        .find(|d| d.id == "ata-WDC_WD40EFPX_WD-SERIAL-B")
        .unwrap();
    assert_eq!(sdb.serial.as_deref(), Some("WD-SERIAL-B"));
    assert_eq!(sdb.model.as_deref(), Some("WD Red Plus"));

    let sdd = state
        .disks
        .iter()
        .find(|d| d.id == "ata-ST4000VN006_ST-SERIAL-D")
        .unwrap();
    assert_eq!(sdd.serial.as_deref(), Some("ST-SERIAL-D"));
    assert_eq!(sdd.model.as_deref(), Some("Seagate IronWolf"));
}

#[test]
fn create_partitions_only_on_actual_band_members_heterogeneous() {
    // D2: the engine used to create a partition for every band on every
    // disk regardless of band membership, breaking SHR's core concept in
    // any heterogeneous-capacity configuration. This is the design's
    // canonical [3,3,4,6] TB example: band0 (4 members, RAID5) must
    // span all disks; band1 (2 members, RAID1) must span ONLY the 4TB and
    // 6TB disks -- the two 3TB disks must not get a second partition.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine.create(create_req(hetero_disks())).unwrap();

    assert_eq!(
        state.bands.len(),
        2,
        "expected band0 (4 members) + band1 (2 members)"
    );
    assert_eq!(state.bands[0].level, "raid5");
    assert_eq!(state.bands[0].member_partitions.len(), 4);
    assert_eq!(state.bands[1].level, "raid1");
    assert_eq!(state.bands[1].member_partitions.len(), 2);

    // The two 3TB disks (smallest) must have exactly ONE partition (band0
    // only) -- band1 needs 4TB+ of usable length to have 2 members at all.
    for id in ["ata-3TB-A", "ata-3TB-B"] {
        let d = state.disks.iter().find(|d| d.id == id).unwrap();
        assert_eq!(
            d.partitions.len(),
            1,
            "{id} should only host band0, got {:?}",
            d.partitions
        );
        assert_eq!(d.partitions[0].band_index, 0);
    }
    // The 4TB and 6TB disks host both bands (the 6TB disk's remaining 2TB
    // stays unpartitioned -- unusable, per the planner).
    for id in ["ata-4TB", "ata-6TB"] {
        let d = state.disks.iter().find(|d| d.id == id).unwrap();
        assert_eq!(d.partitions.len(), 2, "{id} should host band0 and band1");
    }

    // Cross-check against the recorded command log: exactly 6 partitions
    // created (4 for band0 + 2 for band1), not 8 (4 disks x 2 bands -- the
    // D2 bug, which every assertion above could theoretically miss if
    // duplicate/extra mkpart calls were silently issued past what's
    // reflected in state).
    let cmds = runner.get_recorded();
    let mkpart_count = cmds.iter().filter(|c| c.contains("mkpart")).count();
    assert_eq!(mkpart_count, 6, "expected 6 mkpart calls, got: {cmds:?}");
    for id in ["ata-3TB-A", "ata-3TB-B"] {
        let count = cmds
            .iter()
            .filter(|c| c.contains("mkpart") && c.contains(id))
            .count();
        assert_eq!(
            count, 1,
            "{id} should appear in exactly one mkpart command, got {count}"
        );
    }
}

#[test]
fn adjacent_band_partitions_on_the_same_disk_do_not_share_a_boundary_byte() {
    // Found running Step 3's real-VM smoke test: `parted mkpart ... STARTB
    // ENDB` treats ENDB as the LAST byte included in the partition (rounded
    // to its containing sector), not an exclusive bound. Passing band N's
    // mathematically-exclusive end byte straight through as band N+1's
    // touching start byte made real `parted` reject the second mkpart --
    // the two partitions would have shared one sector. This never showed up
    // under DryRunRunner because dry-run never actually asks parted to
    // reconcile a byte position against real sector boundaries.
    //
    // The 4TB and 6TB disks each host two back-to-back bands (band0 then
    // band1) with no gap between them, so their recorded commands are where
    // this must be checked.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req(hetero_disks())).unwrap();
    let cmds = runner.get_recorded();

    let band0_end = mkpart_end_bytes(&cmds, "ata-4TB", 0);
    let band1_start = mkpart_start_bytes(&cmds, "ata-4TB", 1);
    assert_eq!(
        band0_end + 1,
        band1_start,
        "band0's requested end must be exactly one byte before band1's start, not equal to it"
    );
}

/// Parse the Nth (0-indexed) `mkpart` command referencing `disk_id` and
/// return its start byte argument.
fn mkpart_start_bytes(cmds: &[String], disk_id: &str, index: usize) -> u64 {
    mkpart_args(cmds, disk_id, index).0
}

fn mkpart_end_bytes(cmds: &[String], disk_id: &str, index: usize) -> u64 {
    mkpart_args(cmds, disk_id, index).1
}

fn mkpart_args(cmds: &[String], disk_id: &str, index: usize) -> (u64, u64) {
    let matching: Vec<&String> = cmds
        .iter()
        .filter(|c| c.contains("mkpart") && c.contains(disk_id))
        .collect();
    let cmd = matching
        .get(index)
        .unwrap_or_else(|| panic!("no mkpart command #{index} for {disk_id} in {cmds:?}"));
    // "parted -s <path> mkpart primary <start>B <end>B"
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let end = parts[parts.len() - 1].trim_end_matches('B').parse().unwrap();
    let start = parts[parts.len() - 2].trim_end_matches('B').parse().unwrap();
    (start, end)
}

#[test]
fn create_partition_offsets_match_planner_reserved_head_and_alignment() {
    // D9: the engine hardcoded its own 128 MiB reserved-head literal instead
    // of reading it from the same PlannerInput used for planning, and never
    // consulted band_alignment/reserved_tail. Cross-check every recorded
    // partition's offset/size against shr-core's planner (the source of
    // truth) run over the same disks -- not hand-computed expected numbers,
    // which would just re-encode the same arithmetic mistake if wrong.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let disks = hetero_disks();
    let core_disks: Vec<shr_core::Disk> = disks
        .iter()
        .map(shr_inspect::ResolvedDisk::to_planner_disk)
        .collect();
    let plan = shr_core::plan_initial(&shr_core::PlannerInput::new(core_disks, RedundancyMode::Shr)).unwrap();

    let state = engine.create(create_req(disks)).unwrap();

    assert_eq!(plan.bands.len(), state.bands.len());
    for band in &plan.bands {
        // Every band boundary the planner produces is already a multiple of
        // its band_alignment (4 GiB) by construction -- this only fails if
        // the fixture ever stops exercising that path.
        assert_eq!(band.offset() % shr_core::DEFAULT_BAND_ALIGNMENT, 0);

        let expected_start = shr_core::DEFAULT_RESERVED_HEAD + band.offset();
        let expected_end = expected_start + band.size();
        for member_id in band.members() {
            let d = state
                .disks
                .iter()
                .find(|d| d.id == member_id.as_str())
                .unwrap_or_else(|| panic!("{member_id} missing from created state -- D2 regression"));
            let part = d
                .partitions
                .iter()
                .find(|p| p.band_index == band.band_index())
                .unwrap_or_else(|| panic!("{member_id} has no partition for band {}", band.band_index()));
            assert_eq!(
                part.offset_bytes,
                expected_start,
                "band {} offset mismatch for {member_id}",
                band.band_index()
            );
            assert_eq!(
                part.offset_bytes + part.size_bytes,
                expected_end,
                "band {} end mismatch for {member_id}",
                band.band_index()
            );
        }
    }
}

fn two_disks() -> Vec<ResolvedDisk> {
    vec![
        resolved_disk("ata-DISK1", "sdb", 4_000_000_000_000),
        resolved_disk("ata-DISK2", "sdc", 4_000_000_000_000),
    ]
}

/// A disk set with IDs/kernel names wholly disjoint from every other fixture
/// in this file -- for multi-group tests that need a second, independent
/// group to coexist with one built from `three_disks()`/`two_disks()`/
/// `hetero_disks()` without ever tripping the "disk already belongs to an
/// existing group" check by accident.
fn other_two_disks() -> Vec<ResolvedDisk> {
    vec![
        resolved_disk("ata-OTHER1", "sdx", 4_000_000_000_000),
        resolved_disk("ata-OTHER2", "sdy", 4_000_000_000_000),
    ]
}

fn expand_req(new_disks: Vec<ResolvedDisk>) -> ExpandRequest {
    // `name: None` relies on `resolve_group_index`'s "exactly one group ->
    // default to it" rule -- every existing test seeds exactly one group
    // via `seeded_engine`, so this is unambiguous for all of them.
    // `skip_scrub_check: true` -- an earlier fix is exercised by its own dedicated
    // tests; every other test using this helper is testing something else
    // and shouldn't need a fabricated scrub history just to call expand().
    ExpandRequest {
        name: None,
        new_disks,
        system_disks: vec!["sda".to_string()],
        skip_scrub_check: true,
    }
}

/// Seed a real array via `create()` on `runner`, persist it, and return an
/// engine over the same store ready to `expand()`.
fn seeded_engine<'a>(
    runner: &'a dyn CommandRunner,
    state_store: Arc<StateStore>,
    initial_disks: Vec<ResolvedDisk>,
) -> OrchestrationEngine<'a> {
    let create_engine =
        OrchestrationEngine::new(runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created_state = create_engine.create(create_req(initial_disks)).unwrap();
    // Force real persistence even under a dry-run `runner` (some callers
    // pass `DryRunRunner` here specifically to build a fixture without
    // exercising create()'s real-execution path, then need the resulting
    // group to actually be on disk for a subsequent expand() to find).
    state_store.save(&StateFile::new(vec![created_state])).unwrap();
    OrchestrationEngine::new(runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM)
}

#[test]
fn expand_grows_existing_band_at_the_same_level() {
    // D1/D12: plan_expansion is actually called and its GrowBand step
    // actually executed -- the previous implementation just incremented
    // layout_version and touched nothing.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let state = engine.expand(expand_req(vec![new_disk])).unwrap();

    assert_eq!(
        state.bands.len(),
        1,
        "same-level growth must not create a new band"
    );
    assert_eq!(state.bands[0].level, "raid5");
    assert_eq!(state.bands[0].member_partitions.len(), 4);
    assert!(state
        .disks
        .iter()
        .any(|d| d.id == "ata-DISK4" && d.partitions.len() == 1));
    assert!(!state.expansion.in_progress);

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c.contains("mdadm --add /dev/md0")),
        "{cmds:?}"
    );
    // D6: --backup-file always present, and no --level= for a same-level grow.
    let grow = cmds
        .iter()
        .find(|c| c.starts_with("mdadm --grow"))
        .expect("no grow command");
    assert_eq!(
        grow,
        "mdadm --grow /dev/md0 --raid-devices=4 --backup-file=/var/lib/shr-rs/backup-md0.bak"
    );
    assert!(cmds.iter().any(|c| c == "pvresize /dev/md0"), "{cmds:?}");
    assert!(cmds.iter().any(|c| c.contains("lvextend")), "{cmds:?}");
    assert!(cmds.iter().any(|c| c.contains("resize max")), "{cmds:?}");

    //: --add must precede --grow.
    let add_pos = cmds.iter().position(|c| c.contains("mdadm --add")).unwrap();
    let grow_pos = cmds.iter().position(|c| c.starts_with("mdadm --grow")).unwrap();
    assert!(add_pos < grow_pos, "add must happen before grow: {cmds:?}");
}

#[test]
fn expand_removes_a_stale_backup_file_before_growing() {
    // A `--backup-file` left behind by a previous (crashed/aborted)
    // attempt on this exact band must not make a later `--grow` fail
    // outright because mdadm refuses to reuse an existing backup file.
    // `FailingRunner::healthy()` answers any unrecognized command
    // (including `test -e ...`) with success by default, simulating "the
    // file is there".
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();

    let cmds = runner.get_recorded();
    let rm_pos = cmds
        .iter()
        .position(|c| c == "rm -f /var/lib/shr-rs/backup-md0.bak")
        .unwrap_or_else(|| panic!("no removal of the stale backup file: {cmds:?}"));
    let grow_pos = cmds.iter().position(|c| c.starts_with("mdadm --grow")).unwrap();
    assert!(
        rm_pos < grow_pos,
        "stale backup file must be cleared BEFORE growing: {cmds:?}"
    );
    assert!(cmds.iter().any(|c| c == "mkdir -p /var/lib/shr-rs"), "{cmds:?}");
}

#[test]
fn expand_does_not_remove_a_backup_file_that_does_not_exist() {
    // The removal must be conditional -- a band being grown for the
    // very first time (the common case) has no stale file to clean up, and
    // `rm -f` on a nonexistent path, while harmless to mdadm, would still be
    // a command this engine has no real reason to issue.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    // Specific to the BACKUP FILE's existence probe -- must not also match
    // the `reverify_targets` by-id existence probe for the disk being
    // added, which would abort the whole expand() before ever reaching
    // `prepare_backup_file`.
    let runner = FailingRunner {
        fail_forever_trigger: Some("test -e /var/lib/shr-rs".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("rm -f") && c.contains("backup-")),
        "must not remove a backup file that was never found to exist: {cmds:?}"
    );
    let grow = cmds
        .iter()
        .find(|c| c.starts_with("mdadm --grow"))
        .expect("no grow command");
    assert_eq!(
        grow,
        "mdadm --grow /dev/md0 --raid-devices=4 --backup-file=/var/lib/shr-rs/backup-md0.bak"
    );
}

#[test]
fn expand_grow_never_overwrites_a_known_good_md_uuid_with_none_on_a_failed_reread() {
    // An earlier review finding: after a successful `mdadm --grow`, the engine
    // re-reads the array's md_uuid (best-effort, since a real UUID stays
    // stable across --grow and a failed re-read shouldn't abort an
    // already-committed step). The bug: it wrote that best-effort result
    // UNCONDITIONALLY, so a transient read failure nulled out a md_uuid
    // that was already known-good from `create()` -- and
    // `write_managed_configs` would then use the None to silently DELETE
    // the live array's real ARRAY line from /etc/mdadm.conf, while
    // `expand()` still reports success.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::export_fails_after_grow();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let state = engine
        .expand(expand_req(vec![new_disk]))
        .expect("a failed re-read must not fail expand()");

    assert!(
        state.bands[0].md_uuid.is_some(),
        "a known-good md_uuid must never be nulled out by a failed post-grow re-read"
    );
}

#[test]
fn expand_refuses_to_start_while_another_band_is_still_reshaping() {
    // An earlier review finding: nothing previously stopped a SECOND band's
    // `--grow` from starting while a first band was already mid-reshape --
    // two reshapes competing for the same underlying spindles at once,
    // which the design assumes never happens.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let first_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine
        .expand(expand_req(vec![first_disk]))
        .expect("first expand should succeed -- grow starts a reshape, resize is deferred");

    let engine2 = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);
    let second_disk = resolved_disk("ata-DISK5", "sdf", 4_000_000_000_000);
    let err = engine2.expand(expand_req(vec![second_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(err.to_string().contains("sync_action"), "{err}");

    let cmds = runner.get_recorded();
    assert_eq!(
        cmds.iter().filter(|c| c.starts_with("mdadm --grow")).count(),
        1,
        "the second expand must never reach a second --grow: {cmds:?}"
    );
}

#[test]
fn expand_grow_defers_lvm_and_btrfs_resize_while_a_reshape_is_still_running() {
    // Step 8 SM-EXPAND-1 finding: `mdadm --grow` only STARTS the reshape --
    // the underlying block device's reported size doesn't increase until it
    // actually finishes, so `lvextend`/`btrfs resize max` would fail with
    // "No size change" if run immediately. The real array growth (already
    // committed to state.toml -- an earlier review finding) must still count as a
    // successful `expand()`; only the LVM/Btrfs resize itself is skipped
    // until a later, currently-unimplemented retry (see
    // The design).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let state = engine
        .expand(expand_req(vec![new_disk]))
        .expect("a running reshape must not fail expand()");

    assert_eq!(
        state.bands[0].member_partitions.len(),
        4,
        "the real member count must still be recorded"
    );
    assert!(!state.expansion.in_progress);
    assert!(
        state.bands[0].resize_pending,
        "an earlier review: the deferred resize must be a persisted, visible record, not silently dropped"
    );

    let cmds = runner.get_recorded();
    assert!(cmds.iter().any(|c| c.starts_with("mdadm --grow")), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c == "pvresize /dev/md0"),
        "pvresize must be skipped mid-reshape: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("lvextend")),
        "lvextend must be skipped mid-reshape: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("resize max")),
        "btrfs resize must be skipped mid-reshape: {cmds:?}"
    );
}

/// A `MetricsSampler` test double that always reports the same fixed sample
/// -- used to inject a specific danger/idle signal into `expand()`'s reshape
/// throttle wiring without needing a real kernel to sample from.
struct FixedMetricsSampler(ThrottleMetrics);
impl MetricsSampler for FixedMetricsSampler {
    fn sample(&self) -> Option<ThrottleMetrics> {
        Some(self.0)
    }
}

fn benign_metrics() -> ThrottleMetrics {
    ThrottleMetrics {
        cpu_load: Some(0.6),
        io_wait_pct: Some(20.0),
        user_io_latency_p99_ms: Some(50),
        disk_temp_max: Some(40),
        smart_delta_reallocated: Some(0),
    }
}

#[test]
fn expand_reshape_throttle_emergency_brakes_the_moment_smart_reallocated_rises() {
    // Safety requirement: a SMART reallocated-sector increase must
    // brake the reshape immediately, proven by the actual kernel parameter
    // write landing in the mock CommandRunner's command log -- not merely a
    // ThrottleDecision being computed and discarded.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let create_engine =
        OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = create_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let danger = FixedMetricsSampler(ThrottleMetrics {
        smart_delta_reallocated: Some(1),
        ..benign_metrics()
    });
    let engine = OrchestrationEngine::new(&runner, state_store)
        .with_confirm_sink(&ALWAYS_CONFIRM)
        .with_metrics_sampler(&danger);

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine
        .expand(expand_req(vec![new_disk]))
        .expect("a reshaping array must still expand successfully");

    let cmds = runner.get_recorded();
    let grow_pos = cmds
        .iter()
        .position(|c| c.starts_with("mdadm --grow"))
        .expect("no grow command");
    let speed_writes: Vec<&String> = cmds[grow_pos..]
        .iter()
        .filter(|c| c.contains("speed_limit_max"))
        .collect();
    // Two writes are expected: `apply_initial`'s own profile-based write
    // (Balanced's 100_000 KB/s), immediately followed by the one-shot
    // danger tick overriding it down to the floor -- proving the decision
    // actually changed the kernel parameter a second time, not merely that
    // a `ThrottleDecision::EmergencyBrake` was computed and discarded.
    assert_eq!(
        speed_writes.len(),
        2,
        "expected an initial write plus one overriding brake: {cmds:?}"
    );
    assert!(
        speed_writes[0].contains(&RESHAPE_SPEED_INITIAL_KB.to_string()),
        "{speed_writes:?}"
    );
    assert!(
        speed_writes[1].contains(&RESHAPE_SPEED_FLOOR_KB.to_string()),
        "an emergency brake must actually write the floor speed to speed_limit_max, not just log \
         a decision: {speed_writes:?}"
    );
}

#[test]
fn expand_reshape_throttle_with_no_sampler_override_uses_a_real_live_sampler_by_default() {
    // The actual defect: production never wired ANY live sampler, so
    // `NullMetricsSampler`'s fabricated "everything is fine" reading was
    // silently what every real reshape saw. Proof this is fixed lives here,
    // not only in shr-exec's unit tests: `expand()` -- called exactly the
    // way `shr-cli`'s real `Expand` handler calls it, with NO
    // `.with_metrics_sampler(...)` override -- must issue REAL `smartctl`/
    // `/proc` reads (see `crates/shr-orchestrate/src/engine.rs`'s
    // `tick_throttle_decision`, which builds a `LiveMetricsSampler` exactly
    // when `self.metrics_sampler` is `None`, and `start_reshape_throttle`,
    // which calls it right after `mdadm --grow` succeeds).
    //
    // This mock host's `/proc/loadavg`/`/proc/stat`/`smartctl` are
    // deliberately left unscripted (empty stdout, unparseable) -- simulating
    // an environment where telemetry isn't fully available yet. the other
    // requirement is that this must never be read as "safe": the resulting
    // decision must decelerate, not hold at (or exceed) the initial speed.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c == "cat /proc/loadavg"),
        "no live CPU-load read: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.as_str() == "cat /proc/stat"),
        // Only one call here: this mock's /proc/stat is unscripted (empty),
        // so the FIRST read already fails to parse and short-circuits
        // before a second sample would be taken -- see
        // `read_io_wait_pct`/`read_cpu_stat_line`. The point of this
        // assertion is that a real read was ATTEMPTED at all.
        "no live IO-wait read attempt: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("smartctl")),
        "no live SMART read: {cmds:?}"
    );

    let speed_max_writes: Vec<&String> = cmds
        .iter()
        .filter(|c| c.contains("speed_limit_max") && c.contains("/proc/sys/dev/raid"))
        .collect();
    assert_eq!(
        speed_max_writes.len(),
        2,
        "unreadable telemetry must decelerate on top of apply_initial's write, never hold at \
         (or silently exceed) the initial speed as if everything were confirmed healthy: {cmds:?}"
    );
    assert!(
        speed_max_writes[0].contains(&RESHAPE_SPEED_INITIAL_KB.to_string()),
        "{speed_max_writes:?}"
    );
    let decelerated: u64 = (RESHAPE_SPEED_INITIAL_KB as f64 * 0.7).round() as u64;
    assert!(
        speed_max_writes[1].contains(&decelerated.to_string()),
        "must actually write the decelerated speed to the kernel parameter, not just decide it: \
         {speed_max_writes:?}"
    );
}

#[test]
fn expand_reshape_throttle_holds_when_the_live_sampler_reads_a_healthy_system_with_a_known_smart_baseline() {
    // Complements the test above: the live default must not be a disguised
    // "always decelerate" -- once a real SMART baseline is on record (as it
    // would be after this band's first ever tick, or a completed scrub) and
    // every signal reads back healthy, it must hold at the initial speed,
    // exactly like a caller that opted out of monitoring used to see.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping_with_healthy_live_metrics();
    let create_engine =
        OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let mut created = create_engine.create(create_req(three_disks())).unwrap();
    // Simulate an already-known-good SMART baseline for band0 (e.g. from an
    // earlier tick or scrub) -- without this, the very first ever reading
    // has nothing to diff against and correctly decelerates once (the
    // "unknown never means safe" rule applies to a missing baseline too).
    created.bands[0].last_smart_reallocated = Some(0);
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();

    let cmds = runner.get_recorded();
    let speed_max_writes: Vec<&String> = cmds
        .iter()
        .filter(|c| c.contains("speed_limit_max") && c.contains("/proc/sys/dev/raid"))
        .collect();
    assert_eq!(
        speed_max_writes.len(),
        1,
        "a healthy, fully-readable system with a known SMART baseline must hold, not decelerate: {cmds:?}"
    );
    assert!(
        speed_max_writes[0].contains(&RESHAPE_SPEED_INITIAL_KB.to_string()),
        "{speed_max_writes:?}"
    );

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups[0].bands[0].last_smart_reallocated,
        Some(0),
        "the new absolute SMART total must round-trip through state.toml for the next tick"
    );
}

/// The periodic tick a systemd timer fires (`shr-rs internal
/// reshape-throttle-tick`, one brand-new process per fire) must seed from
/// the REAL current kernel speed rather than silently re-assuming the
/// priority profile's initial value -- there is no in-memory
/// `ThrottleController` surviving between fires the way there is within one
/// `expand()` call.
#[test]
fn tick_active_reshapes_seeds_from_the_kernels_real_current_speed_and_applies_a_fresh_decision() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();
    let mark = runner.get_recorded().len();

    let ticked = engine.tick_active_reshapes().unwrap();
    assert_eq!(ticked, 1, "exactly the one reshaping band must be ticked");

    let cmds = &runner.get_recorded()[mark..];
    assert!(
        cmds.iter().any(|c| c == "cat /proc/sys/dev/raid/speed_limit_max"),
        "must read the REAL current speed before deciding, not assume the initial one: {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("sh -c") && c.contains("speed_limit_max")),
        "a fresh decision must actually be applied to the kernel parameter: {cmds:?}"
    );
}

#[test]
fn tick_active_reshapes_ignores_bands_that_are_not_reshaping() {
    // A band merely doing a post-create resync, or idle, or scrubbing
    // (`sync_action == "check"`) must never have its speed_limit_max
    // touched by the reshape throttle -- ticking is scoped to
    // `sync_action == "reshape"` only.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let ticked = engine.tick_active_reshapes().unwrap();
    assert_eq!(ticked, 0, "an idle band must not be throttled");
    assert!(
        !runner
            .get_recorded()
            .iter()
            .any(|c| c.contains("speed_limit_max")),
        "an idle band's kernel parameters must never be touched"
    );
}

#[test]
fn expand_reshape_throttle_honors_a_non_default_priority_profile() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let create_engine =
        OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = create_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let engine = OrchestrationEngine::new(&runner, state_store)
        .with_confirm_sink(&ALWAYS_CONFIRM)
        .with_priority(ReshapePriority::Background);

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c.contains("speed_limit_max") && c.contains("20000")),
        "background priority's 20 MB/s initial speed must actually be written: {cmds:?}"
    );
}

/// `expand --priority background` is honored once (by
/// `start_reshape_throttle`, in-process, right after `mdadm --grow`) but the
/// PERIODIC tick (`tick_active_reshapes`, driven by a systemd timer) runs in
/// a brand-new process every fire with no memory of the original CLI flag --
/// it must read the priority back from `state.toml`, not silently fall back
/// to `OrchestrationEngine::new`'s `Balanced` default.
///
/// Proven by the actual kernel write the tick makes, not merely by the
/// persisted field: `FailingRunner::reshaping()`'s unparseable proc/smartctl
/// signals make `tick()` always decide `Decrease(0.7)` (the
/// unknown-must-decelerate rule), and `ThrottleController::resume` falls back
/// to the PRIORITY PROFILE's own initial speed (its `cat speed_limit_max` is
/// also unparseable) before scaling by 0.7. So Background's math
/// (20_000 * 0.7 = 14_000) is measurably different from Balanced's
/// (100_000 * 0.7 = 70_000) -- what today's bug (tick always defaults to
/// Balanced, ignoring what was persisted) would actually write.
#[test]
fn tick_active_reshapes_uses_the_bands_own_persisted_priority_not_the_engines_default() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine =
        seeded_engine(&runner, state_store.clone(), three_disks()).with_priority(ReshapePriority::Background);
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();
    let mark = runner.get_recorded().len();

    // A brand-new engine, built exactly like the CLI's `ReshapeThrottleTick`
    // handler (no `.with_priority()` call) -- this IS the periodic-tick
    // process, which has no memory of the original `expand --priority
    // background` invocation.
    let tick_engine =
        OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let ticked = tick_engine.tick_active_reshapes().unwrap();
    assert_eq!(ticked, 1);

    let cmds = &runner.get_recorded()[mark..];
    let writes: Vec<&String> = cmds
        .iter()
        .filter(|c| c.starts_with("sh -c") && c.contains("speed_limit_max"))
        .collect();
    assert_eq!(writes.len(), 1, "{cmds:?}");
    assert!(
        writes[0].contains("14000"),
        "the tick must apply BACKGROUND's profile (20000 * 0.7 = 14000), not balanced's \
         (100000 * 0.7 = 70000) which is what today's bug (no persisted priority, tick \
         defaults to Balanced) would write: {writes:?}"
    );
}

/// An earlier fix addressed this for the speed CEILING (`ThrottleController::resume`)
/// but left the DECISION THRESHOLDS (`ReshapeThrottle`'s `SafetyThresholds`)
/// reading `self.priority` -- the periodic-tick process's own default
/// (always Balanced), never the band's persisted profile. Discriminates on
/// the thresholds specifically (not the ceiling, which the earlier test above
/// already covers): an injected sample is healthy on every emergency-brake
/// axis (temperature, SMART) but has `cpu_load` over BALANCED's 0.85
/// decelerate threshold. `max`'s own thresholds are infinite for
/// cpu_load/io_wait/latency, so a band persisted as `max` must `Hold` (no
/// kernel write at all); under the bug it would `Decrease(0.7)` instead.
#[test]
fn tick_active_reshapes_uses_the_bands_own_persisted_priority_for_decision_thresholds_too() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine =
        seeded_engine(&runner, state_store.clone(), three_disks()).with_priority(ReshapePriority::Max);
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();
    let mark = runner.get_recorded().len();

    let over_balanced_cpu = FixedMetricsSampler(ThrottleMetrics {
        cpu_load: Some(0.9),
        ..benign_metrics()
    });
    // Sanity check on the premise: this exact sample must actually
    // decelerate under Balanced's thresholds, and every signal it reports
    // (cpu_load/io_wait_pct/disk_temp_max/smart_delta_reallocated) must be
    // readable -- otherwise the "unknown must decelerate" rule, not the
    // priority profile, would be what's under test here, proving nothing.
    assert_eq!(
        ReshapeThrottle::new(ReshapePriority::Balanced.thresholds(), &over_balanced_cpu).tick(),
        ThrottleDecision::Decrease(0.7),
        "test premise broken: this sample must decelerate under Balanced for the bug to be \
         observable at all"
    );
    assert_eq!(
        ReshapeThrottle::new(ReshapePriority::Max.thresholds(), &over_balanced_cpu).tick(),
        ThrottleDecision::Hold,
        "test premise broken: this same sample must hold under Max's own thresholds"
    );

    // A brand-new engine, built exactly like the CLI's `ReshapeThrottleTick`
    // handler (no `.with_priority()` call) -- the periodic-tick process,
    // with no memory of the original `expand --priority max` invocation.
    let tick_engine = OrchestrationEngine::new(&runner, state_store)
        .with_confirm_sink(&ALWAYS_CONFIRM)
        .with_metrics_sampler(&over_balanced_cpu);
    let ticked = tick_engine.tick_active_reshapes().unwrap();
    assert_eq!(ticked, 1);

    let cmds = &runner.get_recorded()[mark..];
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("sh -c") && c.contains("speed_limit_max")),
        "a band persisted as `max` must not decelerate on CPU load alone -- a write here means \
         the tick used Balanced's thresholds (today's bug) instead of the band's own persisted \
         `max` profile: {cmds:?}"
    );
}

#[test]
fn reconcile_is_a_noop_while_the_reshape_is_still_running() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store, three_disks());
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();
    let mark = runner.get_recorded().len();

    let outcome = engine
        .reconcile()
        .unwrap()
        .expect("an active array must be found");
    assert!(
        outcome.state.groups[0].bands[0].resize_pending,
        "still reshaping -- must remain pending"
    );
    // A resize that's still genuinely pending must not be reported as
    // something reconcile DID -- `performed` stays empty until the reshape
    // actually finishes.
    assert!(
        outcome.performed.is_empty(),
        "still reshaping -- reconcile must not report a completed action yet: {:?}",
        outcome.performed
    );

    let cmds = &runner.get_recorded()[mark..];
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("pvresize") || c.contains("lvextend") || c.contains("resize max")),
        "{cmds:?}"
    );
}

#[test]
fn reconcile_completes_a_deferred_resize_once_the_reshape_finishes() {
    // An earlier review: `resize_pending` must be an actually-completable
    // record, not a dead end. Once the reshape that `execute_grow`
    // deferred on has gone back to `idle`, `reconcile()` must run the
    // deferred pvresize/lvextend/btrfs resize and clear the flag.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store, three_disks());
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let state = engine.expand(expand_req(vec![new_disk])).unwrap();
    assert!(state.bands[0].resize_pending);

    runner.finish_reshape();
    let mark = runner.get_recorded().len();
    let outcome = engine
        .reconcile()
        .unwrap()
        .expect("an active array must be found");

    assert!(
        !outcome.state.groups[0].bands[0].resize_pending,
        "resize_pending must clear once the deferred resize actually ran"
    );
    // A completed deferred resize must be reported
    // as something reconcile DID, not left indistinguishable from "nothing
    // was ever pending" -- the previous CLI report was built purely from
    // the post-state's `resize_pending` flags, which read identically
    // (all `false`) whether nothing was pending or a pending resize just
    // finished right here.
    assert_eq!(
        outcome.performed,
        vec![ReconcileAction::ResizeCompleted {
            group: outcome.state.groups[0].name.clone(),
            band_index: outcome.state.groups[0].bands[0].index,
            md_name: outcome.state.groups[0].bands[0].md_name.clone(),
        }],
        "reconcile() must report the resize it just completed"
    );
    let cmds = &runner.get_recorded()[mark..];
    assert!(cmds.iter().any(|c| c == "pvresize /dev/md0"), "{cmds:?}");
    assert!(cmds.iter().any(|c| c.contains("lvextend")), "{cmds:?}");
    assert!(cmds.iter().any(|c| c.contains("resize max")), "{cmds:?}");
}

#[test]
fn expand_opportunistically_reconciles_a_previously_deferred_resize_before_doing_anything_new() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::reshaping();
    let engine = seeded_engine(&runner, state_store, three_disks());
    let first_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![first_disk])).unwrap();

    // The first expand's own reshape must finish before a SECOND expand is
    // even allowed to start (the reshape-serialization guard) -- exactly
    // the moment `reconcile()` should also have a chance to run.
    runner.finish_reshape();
    let second_disk = resolved_disk("ata-DISK5", "sdf", 4_000_000_000_000);
    let state = engine.expand(expand_req(vec![second_disk])).unwrap();

    assert!(
        !state.bands[0].resize_pending,
        "the first expand's deferred resize must be completed by the second expand's opportunistic reconcile"
    );
}

#[test]
fn reconcile_with_no_active_array_returns_none() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);
    assert!(engine.reconcile().unwrap().is_none());
}

#[test]
fn expand_promotes_raid1_to_raid5_with_backup_file() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, two_disks());

    let new_disk = resolved_disk("ata-DISK3", "sdd", 4_000_000_000_000);
    let state = engine.expand(expand_req(vec![new_disk])).unwrap();

    assert_eq!(state.bands.len(), 1);
    assert_eq!(
        state.bands[0].level, "raid5",
        "RAID1(2) + 1 disk must promote to RAID5(3)"
    );
    assert_eq!(state.bands[0].member_partitions.len(), 3);

    let cmds = runner.get_recorded();
    let grow = cmds
        .iter()
        .find(|c| c.starts_with("mdadm --grow"))
        .expect("no grow command");
    assert_eq!(
        grow,
        "mdadm --grow /dev/md0 --level=raid5 --raid-devices=3 --backup-file=/var/lib/shr-rs/backup-md0.bak"
    );
}

#[test]
fn expand_level_up_with_a_larger_disk_leaves_its_remainder_unusable() {
    // The design's scenario: [4TB,4TB] (RAID1) + 1x6TB -> band0 becomes
    // RAID5(3), consuming only 4TB of the 6TB disk; the remaining 2TB is a
    // MarkUnusable step -- no destructive command, no phantom band1.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, two_disks());

    let new_disk = resolved_disk("ata-6TB", "sdd", 6_000_000_000_000);
    let state = engine.expand(expand_req(vec![new_disk])).unwrap();

    assert_eq!(
        state.bands.len(),
        1,
        "MarkUnusable must not create a band: {:?}",
        state.bands
    );
    let disk6tb = state.disks.iter().find(|d| d.id == "ata-6TB").unwrap();
    assert_eq!(
        disk6tb.partitions.len(),
        1,
        "only the 4TB portion should be partitioned"
    );

    let cmds = runner.get_recorded();
    assert!(
        !cmds.iter().any(|c| c.contains("md1")),
        "no second array should ever be created: {cmds:?}"
    );
}

#[test]
fn expand_creates_new_band_for_two_larger_disks() {
    // three_disks() (RAID5, 3x4TB, band0 only) + 2x6TB -> band0 grows to 5
    // members (GrowBand); the two 6TB disks' shared upper 2TB becomes a
    // brand new band1 (RAID1, CreateBand) -- extending the EXISTING VG
    // (vgextend), not creating a new one.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len(); // isolate expand()'s commands from create()'s

    let new_disks = vec![
        resolved_disk("ata-6TB-A", "sde", 6_000_000_000_000),
        resolved_disk("ata-6TB-B", "sdf", 6_000_000_000_000),
    ];
    let state = engine.expand(expand_req(new_disks)).unwrap();

    assert_eq!(state.bands.len(), 2, "{:?}", state.bands);
    let band0 = state.bands.iter().find(|b| b.index == 0).unwrap();
    assert_eq!(band0.level, "raid5");
    assert_eq!(band0.member_partitions.len(), 5);
    let band1 = state.bands.iter().find(|b| b.index == 1).unwrap();
    assert_eq!(band1.level, "raid1");
    assert_eq!(band1.member_partitions.len(), 2);

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert!(
        cmds.iter().any(|c| c.contains("mdadm --create /dev/md1")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.contains("pvcreate") && c.contains("md1")),
        "{cmds:?}"
    );
    // The VG already exists (from the original create()) -- must extend it,
    // never create a second one.
    assert!(cmds.iter().any(|c| c == "vgextend shr_vg /dev/md1"), "{cmds:?}");
    assert!(!cmds.iter().any(|c| c.starts_with("vgcreate")), "{cmds:?}");
}

#[test]
fn expand_resumes_a_crashed_multi_step_plan_from_the_persisted_checkpoint_without_replaying_finished_steps() {
    // Hand-build a "crashed mid-plan" state.toml -- step 0 (GrowBand on
    // band0) already committed for real (exactly what `execute_grow` would
    // have persisted before a crash), step 1 (CreateBand for a new band1)
    // never ran. `expansion.plan`/`new_disks`/`checkpoint.step_index` are
    // exactly what a real `expand()` call would have persisted at the start
    // -- see the fields this test sets on `state.expansion` below. Band1's
    // own geometry is hand-picked, not planner-derived (resume_expand never
    // recomputes a plan -- it replays whatever is stored -- so only
    // internal self-consistency matters here, not matching what
    // `plan_expansion` would actually produce for this disk set).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let create_engine =
        OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let mut state = create_engine.create(create_req(three_disks())).unwrap();
    assert_eq!(state.bands.len(), 1);
    let band0_partition = state.disks[0].partitions[0].clone();

    let grow_step = ExpansionStep::GrowBand {
        band_index: 0,
        add_members: vec![DiskId::from("ata-NEW1"), DiskId::from("ata-NEW2")],
    };
    let band1_offset = band0_partition.offset_bytes + band0_partition.size_bytes;
    let band1 = RedundantBand::from_parts(
        1,
        band1_offset,
        1 << 30,
        vec![DiskId::from("ata-NEW1"), DiskId::from("ata-NEW2")],
        RaidLevel::Raid1,
    )
    .unwrap();
    let create_band_step = ExpansionStep::CreateBand { band: band1 };

    // Apply step 0's real effect to the fixture by hand: band0 now has 5
    // members, and the two new disks each carry one band0 partition.
    state.bands[0]
        .member_partitions
        .push("part-new1-band0".to_string());
    state.bands[0]
        .member_partitions
        .push("part-new2-band0".to_string());
    for (id, kernel) in [("ata-NEW1", "sde"), ("ata-NEW2", "sdf")] {
        state.disks.push(StateDisk {
            id: id.to_string(),
            size_bytes: 4_000_000_000_000,
            serial: Some(format!("SN-{kernel}")),
            model: Some("Test Disk".to_string()),
            added_at: "2026-07-26T00:00:00Z".to_string(),
            partitions: vec![StatePartition {
                part_uuid: format!("part-{}-band0", id.to_lowercase()),
                offset_bytes: band0_partition.offset_bytes,
                size_bytes: band0_partition.size_bytes,
                band_index: 0,
            }],
        });
    }

    state.expansion.in_progress = true;
    state.expansion.plan = vec![grow_step, create_band_step];
    state.expansion.target_layout_version = 2;
    state.expansion.new_disks = vec![
        StatePendingDisk {
            id: "ata-NEW1".to_string(),
            kernel_name: "sde".to_string(),
            size_bytes: 4_000_000_000_000,
            serial: Some("SN-sde".to_string()),
            model: Some("Test Disk".to_string()),
        },
        StatePendingDisk {
            id: "ata-NEW2".to_string(),
            kernel_name: "sdf".to_string(),
            size_bytes: 4_000_000_000_000,
            serial: Some("SN-sdf".to_string()),
            model: Some("Test Disk".to_string()),
        },
    ];
    state.expansion.checkpoint = Some(StateCheckpoint {
        step_index: 1,
        band_index: Some(1),
        resumable: true,
        description: "creating band 1".to_string(),
    });
    state_store.save(&StateFile::new(vec![state])).unwrap();
    let mark = runner.get_recorded().len();

    let resume_engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);
    // `req` is never consulted on the resume path (the persisted plan is
    // authoritative) -- an empty/irrelevant request proves that.
    let result = resume_engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        })
        .expect("resume must succeed");

    assert_eq!(result.bands.len(), 2, "{:?}", result.bands);
    assert!(!result.expansion.in_progress);
    assert!(result.expansion.checkpoint.is_none());
    assert!(
        result.expansion.plan.is_empty(),
        "a finished expansion must not leave a stale plan behind"
    );
    assert!(result.expansion.new_disks.is_empty());
    assert_eq!(result.layout_version, 2);
    let band1_result = result.bands.iter().find(|b| b.index == 1).unwrap();
    assert_eq!(band1_result.level, "raid1");
    assert_eq!(band1_result.member_partitions.len(), 2);

    let cmds = &runner.get_recorded()[mark..];
    // step 0 (GrowBand on band0) must NOT be replayed.
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --add /dev/md0")),
        "step 0 was replayed: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.starts_with("mdadm --grow /dev/md0")),
        "step 0 was replayed: {cmds:?}"
    );
    // step 1 (CreateBand) must actually run.
    assert!(
        cmds.iter().any(|c| c.contains("mdadm --create /dev/md1")),
        "{cmds:?}"
    );
    assert!(cmds.iter().any(|c| c == "vgextend shr_vg /dev/md1"), "{cmds:?}");
}

#[test]
fn expand_is_blocked_when_the_array_is_degraded() {
    // The design: never run a destructive expansion against a
    // degraded array -- a second failure mid-reshape risks data loss.
    //
    // An earlier review finding: seeded with a TWO-band layout
    // (hetero_disks()) and only band1 (md1) reporting degraded, so this
    // actually proves the check iterates every current band -- a
    // regression to checking only `state.bands[0]` would pass the old
    // single-band version of this test but must fail this one.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md1");
    let engine = seeded_engine(&runner, state_store, hetero_disks());
    let mark = runner.get_recorded().len();

    let new_disk = resolved_disk("ata-DISK5", "sdg", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("mkpart") || c.contains("mdadm --add") || c.contains("mdadm --grow")),
        "{cmds:?}"
    );
}

#[test]
fn expand_refuses_when_an_expansion_is_already_in_progress() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let create_engine =
        OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let mut created_state = create_engine.create(create_req(three_disks())).unwrap();
    created_state.expansion.in_progress = true;
    state_store.save(&StateFile::new(vec![created_state])).unwrap();
    let mark = runner.get_recorded().len();

    let expand_engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = expand_engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    let after = runner.get_recorded();
    assert_eq!(
        &after[mark..],
        &[] as &[String],
        "must not touch anything: {after:?}"
    );
}

#[test]
fn expand_dry_run_never_persists_state() {
    // Same class of bug Codex already fixed for create() (engine.rs dry-run
    // must never overwrite the real state.toml) -- found still present in
    // expand() while wiring D5, Step 1. A dry-run expand must leave the
    // persisted state file exactly as it was before the call.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let sys_runner = DryRunRunner::new(); // used only to build the initial state via a real create
    let create_engine = OrchestrationEngine::new(&sys_runner, state_store.clone());
    let created_state = create_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created_state])).unwrap();
    let before = std::fs::read_to_string(dir.path().join("state.toml")).unwrap();

    let dry_runner = DryRunRunner::new();
    let expand_engine = OrchestrationEngine::new(&dry_runner, state_store.clone());
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let result = expand_engine.expand(expand_req(vec![new_disk])).unwrap();

    // The simulated result can report the bumped version and a grown band...
    assert_eq!(result.layout_version, 2);
    // ...but the on-disk file must be byte-for-byte unchanged.
    let after = std::fs::read_to_string(dir.path().join("state.toml")).unwrap();
    assert_eq!(before, after, "dry-run expand must not touch state.toml");
}

#[test]
fn expand_blocks_when_new_disk_is_a_system_disk() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-SYS", "sda", 4_000_000_000_000);
    assert!(engine.expand(expand_req(vec![new_disk])).is_err());
}

#[test]
fn expand_allows_a_disk_that_only_shares_a_substring_with_a_system_disk() {
    // An earlier review finding: `expand_blocks_when_new_disk_is_a_system_disk`
    // above would also pass under the old substring-matching SafetyGuard
    // (`disk_path.contains(sys_disk)` -- "/dev/sda".contains("sda") is true
    // either way), so it proves nothing about the D4 fix specifically. This
    // test exercises the exact false-positive the old code had:
    // "loop10".contains("loop1") == true, which used to wrongly block a
    // completely different loopback disk.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-LOOP10", "loop10", 4_000_000_000_000);
    let req = ExpandRequest {
        name: None,
        new_disks: vec![new_disk],
        system_disks: vec!["loop1".to_string()],
        skip_scrub_check: true,
    };
    assert!(
        engine.expand(req).is_ok(),
        "loop10 must not be treated as the system disk loop1"
    );
}

// --- an earlier review findings (Step 4+5 review) ---

#[test]
fn expand_rejects_a_disk_that_the_plan_would_not_use() {
    // F5: mirrors create()'s earlier-review guard. A requested disk too
    // small to extend any existing band or seed a new one must be
    // rejected explicitly, not silently ignored while the CLI still
    // reports success for a request that did nothing.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len();

    // align_down(2GiB - 128MiB - 8MiB, 4GiB) == 0 -- contributes nothing to
    // band0 (still 3 members either way) and can't seed a new band alone.
    let tiny = resolved_disk("ata-TINY", "sde", 2 * 1024 * 1024 * 1024);
    let err = engine.expand(expand_req(vec![tiny])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(
        format!("{err}").contains("ata-TINY"),
        "error should name the unused disk: {err}"
    );

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("mkpart") || c.contains("mdadm --add") || c.contains("mdadm --grow")),
        "{cmds:?}"
    );
}

#[test]
fn expand_persists_real_growth_even_if_a_later_command_fails() {
    // F1: a step's state mutation must be committed and persisted
    // immediately once its mdadm change succeeds (the point of no return),
    // BEFORE pvresize/lvextend/resize_max run -- otherwise a failure in
    // one of those follow-up commands would lose the record that the
    // array really did grow, and a retry would re-partition a disk that's
    // already a live array member.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("lvextend");
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Exec(_)), "{err:?}");

    let persisted = state_store.load().unwrap().unwrap();
    assert_eq!(
        persisted.groups[0].bands[0].member_partitions.len(),
        4,
        "the real mdadm growth must be recorded even though the overall call failed"
    );
    assert!(persisted.groups[0].disks.iter().any(|d| d.id == "ata-DISK4"));
    // A real, uncompleted physical change -- the lock must stay held (F2).
    assert!(persisted.groups[0].expansion.in_progress);
}

#[test]
fn expand_clears_in_progress_after_a_cleanly_rolled_back_failure() {
    // F2: a failure that rolls back completely BEFORE the point of no
    // return must not permanently lock the user out of future expansions
    // -- only Step 5's original design did that, for every failure, not
    // just crashes.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("mdadm --add");
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Exec(_)), "{err:?}");

    let persisted = state_store.load().unwrap().unwrap();
    assert!(
        !persisted.groups[0].expansion.in_progress,
        "a cleanly rolled back failure must release the lock"
    );
    assert!(persisted.groups[0].expansion.checkpoint.is_none());
}

#[test]
fn expand_add_member_failure_rolls_back_the_partition() {
    // F6: expand()'s rollback path (RemoveSpareMember/RemovePartition) had
    // no coverage at all -- reverting either wrap_with_rollback call in
    // execute_grow to a bare `return Err(e)` would have left the suite
    // green.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("mdadm --add");
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len();

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Exec(_)),
        "original error must be preserved: {err:?}"
    );

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert_eq!(
        cmds.iter().filter(|c| c.contains("mkpart")).count(),
        1,
        "{cmds:?}"
    );
    assert_eq!(
        cmds.iter()
            .filter(|c| c.contains("parted") && c.contains(" rm "))
            .count(),
        1,
        "{cmds:?}"
    );
    assert!(!cmds.iter().any(|c| c.starts_with("mdadm --grow")), "{cmds:?}");
}

#[test]
fn expand_grow_failure_detaches_the_spare_and_removes_the_partition() {
    // F6, and F4's "verify real state before rolling back" path exercised
    // on its SAFE branch (FailingRunner's mdadm --detail --export stub
    // reports the array unchanged, matching three_disks()'s pre-grow
    // raid5/3-member shape, so `unchanged == true` and rollback proceeds).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("mdadm --grow");
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len();

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Exec(_)),
        "original error must be preserved: {err:?}"
    );

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert!(cmds.iter().any(|c| c.contains("mdadm --add")), "{cmds:?}");
    assert!(cmds.iter().any(|c| c.contains("mdadm --remove")), "{cmds:?}");
    assert!(
        cmds.iter().any(|c| c.contains("parted") && c.contains(" rm ")),
        "{cmds:?}"
    );
}

#[test]
fn expand_vgextend_failure_does_not_wipe_a_pv_that_actually_joined() {
    // F3: vgextend can fail partway through committing its metadata across
    // every PV in the VG. If the PV actually ended up joined despite the
    // reported failure, rollback must NOT pvremove it -- `-ff -y` force-wipes
    // a PV's label even if LVM believes it's live in a VG, which would
    // corrupt the shared VG's metadata out from under the user's existing,
    // unrelated data.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::vgextend_fails_but_pv_already_joined("shr_vg");
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len();

    let new_disks = vec![
        resolved_disk("ata-6TB-A", "sde", 6_000_000_000_000),
        resolved_disk("ata-6TB-B", "sdf", 6_000_000_000_000),
    ];
    let err = engine.expand(expand_req(new_disks)).unwrap_err();
    match err {
        OrchestrateError::Rollback { failures, .. } => {
            assert!(
                failures.iter().any(|f| f.contains("partially accepted")),
                "{failures:?}"
            );
        }
        other => panic!("expected OrchestrateError::Rollback, got {other:?}"),
    }

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert!(
        !cmds.iter().any(|c| c.contains("pvremove")),
        "must not pvremove a PV that may already be live in the VG: {cmds:?}"
    );
}

// --- Step 4: prerequisite checks (D11) + transactional rollback (D10) ---

#[test]
fn create_checks_prerequisites_before_any_destructive_command() {
    // D11: ensure_supported() must run for every executor before ANY
    // partitioning/mdadm/lvm/btrfs command is issued. Simulate mdadm being
    // unavailable and confirm nothing destructive was attempted.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("mdadm --version");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Exec(_)),
        "expected Exec error, got {err:?}"
    );

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("mklabel") || c.contains("mkpart") || c.contains("--create")),
        "no destructive command should run before prerequisite checks pass: {cmds:?}"
    );
}

#[test]
fn create_blocks_when_btrfs_is_unsupported_before_any_partitioning() {
    // D11's specific historical bug: Btrfs support was checked last (at
    // mkfs.btrfs time), after partitions and mdadm arrays already existed.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::without_btrfs();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(&err, OrchestrateError::Exec(shr_exec::ExecError::Prerequisite(_))),
        "expected Prerequisite error, got {err:?}"
    );

    let cmds = runner.get_recorded();
    assert!(
        !cmds.iter().any(|c| c.contains("mkpart")),
        "no partition before the btrfs check: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --create")),
        "no mdadm array before the btrfs check: {cmds:?}"
    );
}

#[test]
fn create_aborts_when_a_target_became_a_system_disk_through_md_or_lvm_since_preflight() {
    // The live re-verification gate used to read `/proc/mounts` and match
    // its device names against the target's kernel name as a prefix. That
    // only ever sees a filesystem mounted straight off a partition, so on
    // an md RAID root (`/dev/mdN`) or an LVM root
    // (`/dev/mapper/<vg>-<lv>`) it matched nothing and never fired -- the
    // exact layouts it exists to protect. Measured on a real RAID1-root
    // guest whose `/`, `/boot` and `/boot/efi` all lived on `sda`:
    // `grep -c '^/dev/sda' /proc/mounts` was 0.
    //
    // Reading lsblk's MOUNTPOINT column instead walks the holder tree, so
    // a `/` that sits on LVM on md on this disk is still traced back to it.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        live_system_mount_on: Some("sdc".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();

    assert!(
        matches!(&err, OrchestrateError::Validation(m) if m.contains("system mountpoint")),
        "expected the live system-disk gate to fire, got {err:?}"
    );
    let cmds = runner.get_recorded();
    assert!(
        !cmds.iter().any(|c| c.contains("mkpart")),
        "must abort before any partitioning: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --create")),
        "must abort before any array is created: {cmds:?}"
    );
}

#[test]
fn mdadm_create_failure_rolls_back_the_partitions_just_created() {
    // D10: three_disks() is a single-band (RAID5, 3 members) SHR layout, so
    // by the time mdadm --create runs, all 3 disks have exactly 1 partition
    // each. Failing that call must remove all 3.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("mdadm --create");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Exec(_)),
        "original error must be preserved: {err:?}"
    );

    let cmds = runner.get_recorded();
    assert_eq!(cmds.iter().filter(|c| c.contains("mkpart")).count(), 3);
    let rm_count = cmds
        .iter()
        .filter(|c| c.contains("parted") && c.contains(" rm "))
        .count();
    assert_eq!(
        rm_count, 3,
        "expected rollback to remove all 3 partitions: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("vgcreate")),
        "must not proceed to LVM after mdadm failed"
    );
}

#[test]
fn vgcreate_failure_rolls_back_mdadm_array_and_partitions() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_on("vgcreate");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Exec(_)),
        "original error must be preserved: {err:?}"
    );

    let cmds = runner.get_recorded();
    assert_eq!(cmds.iter().filter(|c| c.contains("mdadm --create")).count(), 1);
    assert!(
        cmds.iter().any(|c| c.contains("mdadm --stop")),
        "rollback must stop the array: {cmds:?}"
    );
    // `create()` now ALSO zeroes each new member partition's
    // residual superblock right before `mdadm --create` (3 calls), on top
    // of rollback's own 3 (TeardownArray zeroing the array it just made) --
    // 6 total, not 3.
    let zero_count = cmds.iter().filter(|c| c.contains("--zero-superblock")).count();
    assert_eq!(
        zero_count, 6,
        "expected 3 pre-create zero-superblock calls plus 3 rollback \
         TeardownArray zero-superblock calls: {cmds:?}"
    );
    let rm_count = cmds
        .iter()
        .filter(|c| c.contains("parted") && c.contains(" rm "))
        .count();
    assert_eq!(
        rm_count, 3,
        "rollback must also remove the underlying partitions: {cmds:?}"
    );
}

#[test]
fn create_zeroes_residual_superblocks_on_new_partitions_before_mdadm_create() {
    // `destroy` without `--zero-superblocks` (the default) can
    // leave a residual mdadm 1.2 superblock on a member PARTITION. If a
    // later `create()` on the same disks re-creates a byte-identical
    // partition at the same offset, udev incremental assembly can
    // resurrect the OLD array before `mdadm --create` even runs against
    // it, producing EBUSY -- measured on a real guest.
    // Authorization isn't in question at
    // this point in `create()` (the ConfirmSink gate above already
    // approved an irreversible partition-and-format of exactly these
    // disks), so neutralizing any residual superblock right after
    // partitioning and before `mdadm --create` closes the trigger.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req(three_disks())).unwrap();

    let cmds = runner.get_recorded();
    let zero_idx = cmds.iter().position(|c| c.contains("--zero-superblock"));
    let create_idx = cmds.iter().position(|c| c.contains("mdadm --create"));
    assert!(
        zero_idx.is_some(),
        "expected a --zero-superblock call on the new member partitions: {cmds:?}"
    );
    assert!(create_idx.is_some(), "expected mdadm --create to run: {cmds:?}");
    assert!(
        zero_idx.unwrap() < create_idx.unwrap(),
        "zero-superblock must run BEFORE mdadm --create, not after: {cmds:?}"
    );
    let zero_count = cmds.iter().filter(|c| c.contains("--zero-superblock")).count();
    assert_eq!(
        zero_count, 3,
        "expected one zero-superblock call per new member partition (three_disks() is a \
         single RAID5 band, 3 members): {cmds:?}"
    );
}

/// Derive the exact kernel-name string `create()`'s own code will look for
/// on disk `ata-DISK2`'s first partition, via the SAME production helper it
/// uses (`PartedExecutor::partition_path_for_read`) -- rather than assuming
/// a real-Linux-shaped path. This test suite runs on Windows, where
/// `std::fs::canonicalize`'s fallback keeps the by-id path's OS-native
/// separators a real Linux guest would never produce, so hardcoding a
/// Linux-shaped path would silently test the wrong string.
/// `partition_path_for_read` issues no `CommandRunner` calls (its fallback
/// is plain Rust stdlib), so probing it doesn't record anything or need any
/// special runner state.
fn disk2_first_partition_kernel_name() -> String {
    partition_kernel_name("ata-DISK2", 1)
}

/// Generalizes `disk2_first_partition_kernel_name` to any disk id and
/// partition number -- needed for the `execute_grow`/`execute_create_band`
/// holder tests below, which inject a resurrected array onto a NEW disk
/// added by `expand()` (not `create()`'s original three) and, for
/// `execute_create_band`, onto partition 2 (band1) rather than partition 1
/// (band0).
fn partition_kernel_name(id: &str, part_num: u32) -> String {
    let probe = FailingRunner::healthy();
    let disk_path = shr_inspect::resolve_disk_path(&DiskId::new(id))
        .display()
        .to_string();
    let path = shr_exec::PartedExecutor::new(&probe).partition_path_for_read(&disk_path, part_num);
    path.rsplit('/').next().unwrap().to_string()
}

#[test]
fn create_stops_a_self_contained_holder_before_mdadm_create_then_succeeds() {
    // CORRECTED. Real-guest re-verification of the first version of
    // this fix (zero-superblock only, no stop) showed it does NOT work:
    // by the time `zero_superblock` ran, udev incremental assembly had
    // ALREADY resurrected the old array on the partition -- this happens
    // the instant `parted mkpart` recreates it, immediately after
    // `settle_udev`, well before any code in `create()` runs against that
    // partition. Zeroing (and then `mdadm --create`) both then hit the
    // exact same EBUSY the fix was meant to prevent -- exact same
    // transcript as before the first attempt. The holder has to be
    // stopped FIRST, confirmed gone, THEN zeroed, THEN created on top of.
    //
    // This test proves the corrected order end to end: the array holding
    // `ata-DISK2`'s new partition is made purely of THIS request's own new
    // partitions (safe to stop), so `create()` stops it, re-reads live
    // `/proc/mdstat` to confirm it's actually gone (not just trusting
    // `mdadm --stop`'s exit code), and SUCCEEDS -- not merely rolls back
    // cleanly, the way the (still correct, but insufficient on its own)
    // rollback fix alone would.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let holder_member = disk2_first_partition_kernel_name();
    let runner = FailingRunner::holder_array_of_own_partitions("md7", &holder_member);
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine
        .create(create_req(three_disks()))
        .expect("create must succeed, not merely roll back");
    assert_eq!(state.bands.len(), 1);

    let cmds = runner.get_recorded();
    let stop_idx = cmds
        .iter()
        .position(|c| c.starts_with("mdadm --stop") && c.contains("md7"))
        .expect("expected the self-contained holder to be stopped: {cmds:?}");
    let zero_idx = cmds
        .iter()
        .position(|c| c.contains("--zero-superblock"))
        .expect("expected zero-superblock calls: {cmds:?}");
    let create_idx = cmds
        .iter()
        .position(|c| c.contains("mdadm --create"))
        .expect("expected mdadm --create to run: {cmds:?}");
    assert!(
        stop_idx < zero_idx && zero_idx < create_idx,
        "order must be stop -> zero -> create, not any other order: {cmds:?}"
    );
    // The stop must be re-verified against live mdstat, not just issued --
    // i.e. `cat /proc/mdstat` runs again after `mdadm --stop`.
    let mdstat_calls_after_stop = cmds[stop_idx..]
        .iter()
        .filter(|c| c.as_str() == "cat /proc/mdstat")
        .count();
    assert!(
        mdstat_calls_after_stop >= 1,
        "expected a live /proc/mdstat re-read after the stop, not just trusting its exit code: {cmds:?}"
    );
}

#[test]
fn create_refuses_to_stop_a_holder_that_also_spans_a_disk_outside_this_request() {
    // Safety scoping, explicitly requested by the coordinator: an
    // array made purely of THIS create() request's own new partitions is
    // safe to stop (proved above), but an array that ALSO has a member
    // OUTSIDE this request's disks is not something authorizing a create
    // on a different disk set gives permission to touch -- e.g. an
    // operator reusing only SOME of a previously-destroyed group's disks,
    // where udev resurrects the OLD, larger array from a mix of newly
    // re-created and still-untouched partitions.
    //
    // Also proves there is no contradiction between the forward path and
    // its own rollback: partitions were already carved for EVERY disk
    // before this band's array-creation check ran, so the refusal still
    // triggers a rollback -- and rollback's OWN holder-stop logic
    // must refuse the SAME array for the SAME reason, not silently stop it
    // just because it's unwinding instead of proceeding forward.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let target_member = disk2_first_partition_kernel_name();
    let runner = FailingRunner::foreign_holder_blocks_create("md9", &target_member, "sdz1");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    match err {
        OrchestrateError::Rollback { source, failures } => {
            assert!(matches!(*source, OrchestrateError::Validation(_)), "{source:?}");
            let source_msg = source.to_string();
            assert!(
                source_msg.contains("md9") && source_msg.contains("sdz1"),
                "{source_msg}"
            );
            assert!(
                failures
                    .iter()
                    .any(|f| f.contains("md9") && f.contains("will not stop")),
                "rollback must ALSO refuse to stop the foreign-spanning array, not silently \
                 stop what the forward path just refused: {failures:?}"
            );
        }
        other => panic!(
            "expected OrchestrateError::Rollback (refused by both the forward path and its \
             own rollback), got {other:?}"
        ),
    }

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("mdadm --stop") && c.contains("md9")),
        "must never actually stop the foreign-spanning array, in either direction: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --create")),
        "must never reach mdadm --create once the holder is refused: {cmds:?}"
    );
}

#[test]
fn create_refuses_to_proceed_when_stopping_the_holder_does_not_actually_take_effect() {
    // The same trap, explicitly guarded against per the coordinator's
    // second point: `mdadm --stop` exiting 0 is not proof the kernel state
    // actually changed. Simulate a stop that reports success but leaves
    // `/proc/mdstat` unchanged -- `create()` must refuse to proceed onto a
    // partition the kernel still shows as in use, not blindly zero/create
    // on top of it because the stop command merely returned 0.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let holder_member = disk2_first_partition_kernel_name();
    let runner = FailingRunner::holder_stop_does_not_take_effect("md7", &holder_member);
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    let source_msg = match &err {
        OrchestrateError::Rollback { source, .. } => source.to_string(),
        OrchestrateError::Validation(msg) => msg.clone(),
        other => panic!("expected Validation or Rollback-wrapped Validation, got {other:?}"),
    };
    assert!(source_msg.contains("still shows a holder"), "{source_msg}");

    let cmds = runner.get_recorded();
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --create")),
        "must never reach mdadm --create on a partition still shown as held: {cmds:?}"
    );
}

#[test]
fn expand_grow_stops_a_self_contained_holder_before_mdadm_add_then_succeeds() {
    // the design recorded this as "found by code
    // reading, not measured" -- `execute_grow` never called
    // `stop_any_foreign_holder_before_create` or zeroed a new member's
    // superblock before `mdadm --add`, unlike `create()`. A member
    // being ADDED to an existing band is exposed to the exact same udev
    // race: `parted mkpart` on a disk reused from an earlier `destroy`
    // (without `--zero-superblocks`) resurrects the old array on the new
    // partition immediately, before `execute_grow` runs against it.
    //
    // Mirrors `create_stops_a_self_contained_holder_before_mdadm_create_then_succeeds`
    // exactly, reusing the SAME helpers (`stop_any_foreign_holder_before_create`,
    // `holder_md_array`) -- not a parallel implementation -- just for
    // `mdadm --add` instead of `mdadm --create`.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let holder_member = partition_kernel_name("ata-DISK4", 1);
    let runner = FailingRunner::holder_array_of_own_partitions("md7", &holder_member);
    let engine = seeded_engine(&runner, state_store, three_disks());
    // `seeded_engine` runs a real `create()` first, which itself
    // zero-superblocks its own new partitions unconditionally -- without
    // this mark, that earlier zero-superblock call would be mistaken for
    // this test's expand-path one and sort before `stop_idx` by pure
    // accident, exactly the "adjacent, not the same" trap this project
    // tracks (see `expand_create_band_...` below, which already does this).
    let mark = runner.get_recorded().len();

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let state = engine
        .expand(expand_req(vec![new_disk]))
        .expect("expand must succeed, not merely roll back");
    assert_eq!(state.bands[0].member_partitions.len(), 4);

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    let stop_idx = cmds
        .iter()
        .position(|c| c.starts_with("mdadm --stop") && c.contains("md7"))
        .unwrap_or_else(|| panic!("expected the self-contained holder to be stopped: {cmds:?}"));
    let zero_idx = cmds
        .iter()
        .position(|c| c.contains("--zero-superblock"))
        .unwrap_or_else(|| panic!("expected a zero-superblock call: {cmds:?}"));
    let add_idx = cmds
        .iter()
        .position(|c| c.contains("mdadm --add"))
        .unwrap_or_else(|| panic!("expected mdadm --add to run: {cmds:?}"));
    assert!(
        stop_idx < zero_idx && zero_idx < add_idx,
        "order must be stop -> zero -> add, not any other order: {cmds:?}"
    );
    // The stop must be re-verified against live mdstat, not just
    // issued.
    let mdstat_calls_after_stop = cmds[stop_idx..]
        .iter()
        .filter(|c| c.as_str() == "cat /proc/mdstat")
        .count();
    assert!(
        mdstat_calls_after_stop >= 1,
        "expected a live /proc/mdstat re-read after the stop: {cmds:?}"
    );
}

#[test]
fn expand_grow_refuses_to_stop_a_holder_that_also_spans_a_disk_outside_this_request() {
    // Safety scoping mirror of `create()`'s equivalent guard: a
    // resurrected array made purely of THIS grow's own new member is safe
    // to stop (proved above), but one that ALSO has a member outside this
    // request's disks must never be touched, and rollback must refuse the
    // exact same array for the exact same reason rather than silently
    // stopping what the forward path just refused.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let target_member = partition_kernel_name("ata-DISK4", 1);
    let runner = FailingRunner::foreign_holder_blocks_create("md9", &target_member, "sdz1");
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    match err {
        OrchestrateError::Rollback { source, failures } => {
            assert!(matches!(*source, OrchestrateError::Validation(_)), "{source:?}");
            let source_msg = source.to_string();
            assert!(
                source_msg.contains("md9") && source_msg.contains("sdz1"),
                "{source_msg}"
            );
            assert!(
                failures
                    .iter()
                    .any(|f| f.contains("md9") && f.contains("will not stop")),
                "rollback must ALSO refuse to stop the foreign-spanning array: {failures:?}"
            );
        }
        other => panic!(
            "expected OrchestrateError::Rollback (refused by both the forward path and its \
             own rollback), got {other:?}"
        ),
    }

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("mdadm --stop") && c.contains("md9")),
        "must never actually stop the foreign-spanning array, in either direction: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --add")),
        "must never reach mdadm --add once the holder is refused: {cmds:?}"
    );
}

#[test]
fn expand_create_band_stops_a_self_contained_holder_before_mdadm_create_then_succeeds() {
    // `execute_create_band` also lacked the holder-stop +
    // zero-superblock protection. Uses the same disk set as
    // `expand_creates_new_band_for_two_larger_disks` (three_disks() plus
    // two 6TB disks -> band0 GrowBand + band1 CreateBand in one expand()
    // call) so the fix can be proven in the same combined-step shape that
    // scenario exercises, but places the holder ONLY on band1's own new
    // partitions (part_num=2 on both 6TB disks) -- band0's GrowBand step
    // (part_num=1, already covered by the tests above) must run
    // untouched, isolating this test to `execute_create_band`'s own fix.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let member_a = partition_kernel_name("ata-6TB-A", 2);
    let member_b = partition_kernel_name("ata-6TB-B", 2);
    let runner = FailingRunner {
        mdstat_content: Mutex::new(two_member_mdstat("md9", &member_a, &member_b)),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len(); // isolate expand()'s commands from create()'s

    let new_disks = vec![
        resolved_disk("ata-6TB-A", "sde", 6_000_000_000_000),
        resolved_disk("ata-6TB-B", "sdf", 6_000_000_000_000),
    ];
    let state = engine
        .expand(expand_req(new_disks))
        .expect("expand must succeed, not merely roll back");
    assert_eq!(state.bands.len(), 2, "{:?}", state.bands);
    let band1 = state.bands.iter().find(|b| b.index == 1).unwrap();
    assert_eq!(band1.member_partitions.len(), 2);

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    let stop_idx = cmds
        .iter()
        .position(|c| c.starts_with("mdadm --stop") && c.contains("md9"))
        .unwrap_or_else(|| panic!("expected the self-contained holder to be stopped: {cmds:?}"));
    let create_idx = cmds
        .iter()
        .position(|c| c.contains("mdadm --create"))
        .unwrap_or_else(|| panic!("expected mdadm --create to run: {cmds:?}"));
    // band0's own GrowBand step (part_num=1) zero-superblocks its own new
    // member unconditionally before this band1 (part_num=2) work even
    // starts -- searching `cmds` from index 0 for "--zero-superblock" would
    // find THAT call, not band1's, and pass even if band1's own zero were
    // missing entirely. Scope the search to strictly between this band's
    // stop and create so the assertion actually verifies band1's ordering.
    let zero_idx = cmds[stop_idx..create_idx]
        .iter()
        .position(|c| c.contains("--zero-superblock"))
        .map(|i| i + stop_idx)
        .unwrap_or_else(|| {
            panic!("expected a zero-superblock call between the stop and the create: {cmds:?}")
        });
    assert!(
        stop_idx < zero_idx && zero_idx < create_idx,
        "order must be stop -> zero -> create, not any other order: {cmds:?}"
    );
    let mdstat_calls_after_stop = cmds[stop_idx..]
        .iter()
        .filter(|c| c.as_str() == "cat /proc/mdstat")
        .count();
    assert!(
        mdstat_calls_after_stop >= 1,
        "expected a live /proc/mdstat re-read after the stop: {cmds:?}"
    );
    // band0's own GrowBand step (part_num=1, no holder there) must never
    // have touched md9 -- confirms the fix is scoped to band1's own check,
    // not a coincidental side effect of some other guard.
    let grow_pos = cmds.iter().position(|c| c.starts_with("mdadm --grow")).unwrap();
    assert!(
        grow_pos < stop_idx,
        "band0's grow must run before band1's holder is stopped: {cmds:?}"
    );
}

#[test]
fn expand_create_band_refuses_to_stop_a_holder_that_also_spans_a_disk_outside_this_request() {
    // Safety scoping mirror for `execute_create_band`: the holder found
    // on band1's own new partitions ALSO has a member outside this
    // request's disks (`sdz2`) -- must never be stopped, in either the
    // forward path or its own rollback.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let member_a = partition_kernel_name("ata-6TB-A", 2);
    let member_b = partition_kernel_name("ata-6TB-B", 2);
    let content = format!(
        "Personalities : [raid1]\nmd9 : active raid1 sdz2[2] {member_b}[1] {member_a}[0]\n      \
         1000000 blocks super 1.2 [3/3] [UUU]\nunused devices: <none>\n"
    );
    let runner = FailingRunner {
        mdstat_content: Mutex::new(content),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks());
    let mark = runner.get_recorded().len();

    let new_disks = vec![
        resolved_disk("ata-6TB-A", "sde", 6_000_000_000_000),
        resolved_disk("ata-6TB-B", "sdf", 6_000_000_000_000),
    ];
    let err = engine.expand(expand_req(new_disks)).unwrap_err();
    let (source_msg, failures) = match err {
        OrchestrateError::Rollback { source, failures } => {
            assert!(matches!(*source, OrchestrateError::Validation(_)), "{source:?}");
            (source.to_string(), failures)
        }
        other => panic!(
            "expected OrchestrateError::Rollback (refused by both the forward path and its \
             own rollback), got {other:?}"
        ),
    };
    assert!(
        source_msg.contains("md9") && source_msg.contains("sdz2"),
        "{source_msg}"
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("md9") && f.contains("will not stop")),
        "rollback must ALSO refuse to stop the foreign-spanning array: {failures:?}"
    );

    let all = runner.get_recorded();
    let cmds = &all[mark..];
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("mdadm --stop") && c.contains("md9")),
        "must never actually stop the foreign-spanning array, in either direction: {cmds:?}"
    );
    // band0's GrowBand step never calls `mdadm --create` (only band1's
    // CreateBand step would) -- so ANY occurrence here would be band1's,
    // which must never run once the holder is refused.
    assert!(
        !cmds.iter().any(|c| c.contains("mdadm --create")),
        "must never reach mdadm --create for band1 once the holder is refused: {cmds:?}"
    );
}

#[test]
fn create_rejects_a_colliding_vg_name_before_any_destructive_command() {
    // Read `create()`/`shr-cli` yourself before trusting
    // another agent's finding -- verified here by driving the real code
    // path: `vgcreate` doesn't run until deep inside the destructive
    // sequence (LVM setup, well after partitions and mdadm arrays already
    // exist), so a colliding VG name must be caught up front instead, and
    // against LIVE LVM state (`vgs`), never `state.toml` (a VG can exist on
    // the host without shr-rs knowing about it at all).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::vg_already_exists("shr_vg");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Validation(_)),
        "expected Validation error, got {err:?}"
    );
    assert!(err.to_string().contains("shr_vg"), "{err}");

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("mklabel") || c.contains("mkpart") || c.contains("mdadm --create")),
        "no destructive command should run before the VG-collision guard: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("vgs")),
        "expected a live vgs check: {cmds:?}"
    );
}

#[test]
fn create_rejects_a_colliding_lv_name_before_any_destructive_command() {
    // The LV-within-the-VG half of the guard -- exercised directly (a
    // real host can't reach this combination through `create()` itself,
    // since `create()` always makes a brand-new VG, but a future caller
    // that legitimately reuses an already-existing, otherwise-clean VG must
    // still be caught if the LV name inside it collides).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::lv_already_exists("shr_vg", "data");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Validation(_)),
        "expected Validation error, got {err:?}"
    );

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("mklabel") || c.contains("mkpart") || c.contains("mdadm --create")),
        "no destructive command should run before the LV-collision guard: {cmds:?}"
    );
}

#[test]
fn rollback_failure_is_reported_without_losing_the_original_error() {
    // Main failure: vgcreate. Rollback then tries to remove the 3 member
    // partitions; make partition removal always fail (simulating e.g.
    // "device busy") to prove a rollback failure doesn't swallow the
    // original error. Deliberately NOT `--zero-superblock` as the
    // forever-fail trigger (this test's shape before that fix): `create()` now
    // ALSO calls `--zero-superblock` proactively on each new member
    // partition BEFORE `mdadm --create` even runs, so a forever-fail on
    // that substring would abort at the very first partition -- long
    // before `vgcreate` is ever reached -- and no longer exercise a
    // *rollback* failure at all. `" rm "` (parted's partition-removal verb)
    // only ever appears during rollback, so it isolates the same "a
    // rollback step itself fails" scenario without that collision.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::failing_once_and_forever("vgcreate", " rm ");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    match err {
        OrchestrateError::Rollback { source, failures } => {
            // An earlier review finding: `matches!(*source,
            // OrchestrateError::Exec(_))` alone doesn't discriminate
            // "source preserved" from "source silently replaced by a
            // rollback failure" -- the injected rollback failures (` rm `)
            // are ALSO ExecError::NonZeroExit, so a bug that overwrote
            // source with a rollback error would still pass that check.
            // Assert the source's actual content instead.
            assert!(
                matches!(*source, OrchestrateError::Exec(_)),
                "original vgcreate error must be preserved as the rollback error's source"
            );
            let source_msg = source.to_string();
            assert!(
                source_msg.contains("vgcreate"),
                "source must specifically be the vgcreate failure, not a rollback failure: {source_msg}"
            );
            assert_eq!(
                failures.len(),
                3,
                "expected all 3 partition-removal rollback failures: {failures:?}"
            );
            assert!(
                failures.iter().all(|f| f.contains("remove partition")),
                "{failures:?}"
            );
        }
        other => panic!("expected OrchestrateError::Rollback, got {other:?}"),
    }
}

#[test]
fn rollback_never_runs_under_dry_run() {
    // A dry-run failure (e.g. duplicate id, validated before anything is
    // built) must never trigger rollback command issuance -- there was
    // nothing real to undo, and DryRunRunner would misrepresent undo
    // commands as if they'd actually run.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let mut disks = three_disks();
    disks.push(resolved_disk("ata-DISK1", "sde", 4_000_000_000_000)); // duplicate id
    let err = engine.create(create_req(disks)).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)));
    assert!(runner.get_recorded().is_empty());
}

// --- Multi-group support: many independent SHR groups on one host ---

#[test]
fn create_rejects_a_duplicate_group_name() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    let mark = runner.get_recorded().len();

    let err = engine
        .create(create_req_named("shr1", other_two_disks()))
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("shr1"));

    let all = runner.get_recorded();
    assert_eq!(
        &all[mark..],
        &[] as &[String],
        "must not touch anything: {:?}",
        &all[mark..]
    );
}

#[test]
fn create_rejects_a_disk_that_already_belongs_to_another_group() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    let mark = runner.get_recorded().len();

    // ata-DISK1 already belongs to "shr1" -- a disk can only ever be a
    // member of one group, so a second group trying to claim it must be
    // rejected before anything is touched, even though the group NAME here
    // is different and would otherwise be perfectly valid.
    let overlapping = vec![resolved_disk("ata-DISK1", "sdb", 4_000_000_000_000)];
    let err = engine.create(create_req_named("shr2", overlapping)).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("ata-DISK1"));

    let all = runner.get_recorded();
    assert_eq!(
        &all[mark..],
        &[] as &[String],
        "must not touch anything: {:?}",
        &all[mark..]
    );
}

#[test]
fn expand_rejects_a_disk_that_already_belongs_to_another_group() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();
    let mark = runner.get_recorded().len();

    // ata-OTHER1 already belongs to shr2 -- trying to add it to shr1 via
    // expand() must be rejected the same way create() rejects it.
    let overlapping = vec![resolved_disk("ata-OTHER1", "sdx", 4_000_000_000_000)];
    let err = engine
        .expand(ExpandRequest {
            name: Some("shr1".to_string()),
            new_disks: overlapping,
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        })
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("ata-OTHER1"));

    let all = runner.get_recorded();
    assert_eq!(
        &all[mark..],
        &[] as &[String],
        "must not touch anything: {:?}",
        &all[mark..]
    );
}

/// A `StateFile` with one throwaway group whose bands occupy exactly the
/// given `/dev/mdN` numbers -- lets a test force `used_md_numbers` (the
/// state.toml-only source) to already claim a contiguous run of numbers, so
/// the NEXT free number under the old (state-only) allocation logic lands on
/// a specific value a test wants to also mark as host-occupied.
fn state_with_md_numbers(numbers: impl IntoIterator<Item = u32>) -> StateFile {
    let bands: Vec<StateBand> = numbers
        .into_iter()
        .enumerate()
        .map(|(i, n)| StateBand {
            index: i as u8,
            level: "raid5".to_string(),
            md_name: format!("md{n}"),
            md_uuid: None,
            member_partitions: vec![],
            usable_bytes: 0,
            resize_pending: false,
            last_smart_reallocated: None,
            last_scrub: None,
            scrub_in_progress: false,
            pending_member_removal: None,
            reshape_priority: None,
        })
        .collect();
    StateFile::new(vec![ArrayState {
        name: "filler".to_string(),
        mode: "shr".to_string(),
        created_at: "1970-01-01T00:00:00Z".to_string(),
        layout_version: 1,
        disks: vec![],
        bands,
        filesystem: StateFilesystem {
            fs_uuid: None,
            mount_point: "/mnt/filler".to_string(),
            vg_name: "filler_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
        },
        expansion: StateExpansion::default(),
    }])
}

// --- Host-wide md name collision (allocation must not just look at state.toml) ---

#[test]
fn create_does_not_allocate_an_md_name_the_host_already_has_outside_state_toml() {
    // `used_md_numbers` only sees bands shr-rs itself recorded in
    // state.toml -- but `/dev/mdN` is a host-global kernel namespace, not
    // something shr-rs owns exclusively. A foreign array, a leftover
    // superblock from a previous install, or the kernel's own
    // auto-assembly can all claim a number shr-rs never wrote down.
    // Simulate a host that already has a REAL /dev/md0 shr-rs knows
    // nothing about (state.toml is empty -- this is the very first group
    // ever created here) and confirm allocation skips it.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::host_has_md_numbers(&[0]);
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine.create(create_req(three_disks())).unwrap();

    assert_ne!(
        state.bands[0].md_name, "md0",
        "must not collide with the host's pre-existing md0 (which state.toml has no record of)"
    );
    assert_eq!(state.bands[0].md_name, "md1");
}

#[test]
fn create_does_not_allocate_an_md_name_the_host_already_has_at_a_high_number() {
    // Same bug, exercised at a realistic real-world number: mdadm commonly
    // auto-assembles an unrecognized/foreign array as md127. Seed
    // state.toml so ITS OWN bookkeeping alone would already pick "md127" as
    // the next free number, and confirm the host's real md127 (which
    // state.toml still has no record of) is also excluded -- proving this
    // isn't just a "lowest number" special case.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    state_store.save(&state_with_md_numbers(0..127)).unwrap();
    let runner = FailingRunner::host_has_md_numbers(&[127]);
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine.create(create_req(three_disks())).unwrap();

    assert_ne!(
        state.bands[0].md_name, "md127",
        "must not collide with the host's pre-existing md127"
    );
    assert_eq!(state.bands[0].md_name, "md128");
}

#[test]
fn expand_creating_a_new_band_does_not_allocate_an_md_name_the_host_already_has() {
    // Same collision, exercised via `expand()`'s `ExpansionStep::CreateBand`
    // path (its own `used_md` seed, separate code from create()'s).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::host_has_md_numbers(&[1]);
    let engine = seeded_engine(&runner, state_store, three_disks());

    // three_disks() (RAID5, 3x4TB, band0=md0 only) + 2x6TB creates a brand
    // new band1 -- normally allocated md1, the next free number after
    // create()'s own md0. The host already has md1 (a foreign array), so
    // it must be skipped in favor of md2.
    let new_disks = vec![
        resolved_disk("ata-6TB-A", "sde", 6_000_000_000_000),
        resolved_disk("ata-6TB-B", "sdf", 6_000_000_000_000),
    ];
    let state = engine.expand(expand_req(new_disks)).unwrap();

    let new_band = state
        .bands
        .iter()
        .find(|b| b.index == 1)
        .expect("a new band must have been created");
    assert_ne!(
        new_band.md_name, "md1",
        "must not collide with the host's pre-existing md1"
    );
    assert_eq!(new_band.md_name, "md2");
}

#[test]
fn two_groups_created_independently_never_receive_the_same_md_name() {
    // The correctness trap this change exists to close: band indices
    // restart at 0 within every group, so deriving md_name from band_index
    // alone (the pre-multi-group scheme) would give shr1's band0 and shr2's
    // band0 the SAME `/dev/md0`.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);

    let shr1 = engine.create(create_req_named("shr1", three_disks())).unwrap();
    assert_eq!(shr1.bands[0].md_name, "md0");

    let shr2 = engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();
    assert_ne!(
        shr1.bands[0].md_name, shr2.bands[0].md_name,
        "two independently-created groups' band0 must never collide on /dev/mdN"
    );
    assert_eq!(
        shr2.bands[0].md_name, "md1",
        "the next free md number after shr1's md0"
    );

    let persisted = state_store.load().unwrap().unwrap();
    assert_eq!(persisted.groups.len(), 2);
}

#[test]
fn expand_creating_a_new_band_never_collides_with_another_groups_md_name() {
    // shr1: three_disks() -> band0 = md0.
    // shr2: an independent hetero group -> band0 = md1, band1 = md2
    // (allocation is host-wide, so shr2's own two bands are also never
    // md0/md1 -- they continue right after shr1's).
    // Then expanding shr1 with two 6TB disks grows band0 in place (reuses
    // md0) AND creates a brand-new band -- exactly the path that allocates
    // a FRESH md name, which must not collide with shr2's md1/md2 either.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();

    let shr2_disks = vec![
        resolved_disk("ata-S2-A", "sdk", 3_000_000_000_000),
        resolved_disk("ata-S2-B", "sdl", 3_000_000_000_000),
        resolved_disk("ata-S2-C", "sdm", 4_000_000_000_000),
        resolved_disk("ata-S2-D", "sdn", 6_000_000_000_000),
    ];
    let shr2 = engine.create(create_req_named("shr2", shr2_disks)).unwrap();
    assert_eq!(shr2.bands.len(), 2, "{:?}", shr2.bands);
    let shr2_md_names: HashSet<String> = shr2.bands.iter().map(|b| b.md_name.clone()).collect();

    let new_disks = vec![
        resolved_disk("ata-S1-6TB-A", "sdo", 6_000_000_000_000),
        resolved_disk("ata-S1-6TB-B", "sdp", 6_000_000_000_000),
    ];
    let expanded = engine
        .expand(ExpandRequest {
            name: Some("shr1".to_string()),
            new_disks,
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        })
        .unwrap();

    assert_eq!(expanded.bands.len(), 2, "{:?}", expanded.bands);
    let new_band = expanded
        .bands
        .iter()
        .find(|b| b.index == 1)
        .expect("a new band must have been created");
    assert!(
        !shr2_md_names.contains(&new_band.md_name),
        "shr1's newly-created band got md name `{}`, which collides with shr2's: {shr2_md_names:?}",
        new_band.md_name
    );
    assert_eq!(
        new_band.md_name, "md3",
        "next free after md0 (shr1 band0), md1/md2 (shr2)"
    );
}

#[test]
fn expand_requires_a_name_when_multiple_groups_exist() {
    // Silently picking "the first group" when several exist would let an
    // operator expand the wrong group by accident just by omitting --name
    // -- far worse than requiring them to be explicit.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();
    let mark = runner.get_recorded().len();

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        })
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("--name"));

    let all = runner.get_recorded();
    assert_eq!(
        &all[mark..],
        &[] as &[String],
        "must not touch anything: {:?}",
        &all[mark..]
    );
}

#[test]
fn expand_can_target_a_specific_group_leaving_the_other_group_unaffected() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    let shr2_before = engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let expanded = engine
        .expand(ExpandRequest {
            name: Some("shr1".to_string()),
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        })
        .unwrap();
    assert_eq!(expanded.name, "shr1");
    assert_eq!(
        expanded.bands[0].member_partitions.len(),
        4,
        "shr1 must have grown"
    );

    let persisted = state_store.load().unwrap().unwrap();
    let shr2_after = persisted.find("shr2").expect("shr2 must still exist");
    assert_eq!(
        *shr2_after, shr2_before,
        "expanding shr1 must not touch shr2 at all"
    );
}

#[test]
fn creating_a_second_group_leaves_the_first_groups_mdadm_conf_and_fstab_entries_intact() {
    // The end-to-end version of the same correctness trap covered at the
    // `shr_state::conf` unit level: exercised here through real
    // `OrchestrationEngine::create()` calls (real md_uuid/fs_uuid values
    // from `FailingRunner`'s blkid/mdadm-export stubs, real
    // write_managed_configs call sites), not hand-built fixtures.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let mdadm_conf = dir.path().join("mdadm.conf");
    let fstab = dir.path().join("fstab");
    let engine = OrchestrationEngine::new(&runner, state_store)
        .with_conf_paths(&mdadm_conf, &fstab)
        .with_confirm_sink(&ALWAYS_CONFIRM);

    let shr1 = engine.create(create_req_named("shr1", three_disks())).unwrap();
    let mdadm_conf_after_first = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(mdadm_conf_after_first.contains(&format!("ARRAY /dev/{}", shr1.bands[0].md_name)));
    let fstab_after_first = std::fs::read_to_string(&fstab).unwrap();
    assert!(fstab_after_first.contains(&shr1.filesystem.mount_point));

    engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    let mdadm_conf_after_second = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(
        mdadm_conf_after_second.contains(&format!("ARRAY /dev/{}", shr1.bands[0].md_name)),
        "creating shr2 must not delete shr1's ARRAY line: {mdadm_conf_after_second}"
    );

    let fstab_after_second = std::fs::read_to_string(&fstab).unwrap();
    assert!(
        fstab_after_second.contains(&shr1.filesystem.mount_point),
        "creating shr2 must not delete shr1's fstab mount: {fstab_after_second}"
    );
}

// --- Stage 0 / fail-closed follow-up: ProgressSink / ConfirmSink ------
//
// the design Stage 0: `create`/`expand`/`reconcile` must
// accept a `ConfirmSink`/`ProgressSink`, and a rejected `ConfirmSink` answer
// must stop a destructive operation before it touches anything real -- not
// just return an error.
//
// Post-Stage-0 follow-up (still Stage A prep): the engine's default when no
// `ConfirmSink` is wired flipped from `AlwaysConfirmSink` (fail-open) to
// `AlwaysRejectConfirmSink` (fail-closed) -- see
// `OrchestrationEngine::new`'s doc comment for why. Every test ABOVE this
// point that drives a real (non-`DryRunRunner`) `create`/`expand` to success
// now explicitly wires `ALWAYS_CONFIRM` (defined near the top of this file)
// to opt back into the old auto-approve behavior, exactly like `shr-cli`
// does at its own call site -- this is the only way any of them still pass.
// `default_engine_without_a_confirm_sink_now_fails_closed` just below is the
// direct proof of the new default itself.

/// True for this project's known DESTRUCTIVE commands -- as opposed to a
/// read-only prerequisite/probe/inspection call (`parted --version`, `cat
/// /proc/mdstat`, a `sync_action`/`degraded` sysfs read) that `create`/
/// `expand` legitimately issue before ever reaching the `ConfirmSink` gate.
/// Used to prove a rejected `create`/`expand` touched nothing real, without
/// having to enumerate every harmless read it's allowed to make first.
fn is_destructive(cmd: &str) -> bool {
    const MARKERS: &[&str] = &[
        "mkpart",
        "mklabel",
        "mdadm --create",
        "mdadm --add",
        "mdadm --grow",
        "vgcreate",
        "vgextend",
        "lvcreate",
        "lvextend",
        "pvresize",
        "mkfs.btrfs",
        "pvcreate -ff",
        "mount ",
    ];
    !cmd.contains("--version") && MARKERS.iter().any(|m| cmd.contains(m))
}

#[test]
fn confirm_reject_blocks_create_before_any_destructive_command() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let confirm = RecordingConfirmSink::rejecting();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&confirm);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().all(|c| !is_destructive(c)),
        "a rejected create must not issue any destructive command, got: {cmds:?}"
    );
    assert!(
        !state_store.exists(),
        "a rejected create must not persist any state"
    );

    let requests = confirm.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].operation, "create");
    assert!(
        requests[0].irreversible,
        "create is never cleanly undoable past its point of no return"
    );
}

#[test]
fn confirm_proceed_allows_create_to_succeed_and_records_the_request() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let confirm = RecordingConfirmSink::proceeding();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&confirm);

    let state = engine.create(create_req(three_disks())).unwrap();

    assert_eq!(state.disks.len(), 3);
    assert_eq!(confirm.requests().len(), 1);
    assert!(runner.get_recorded().iter().any(|c| c.contains("mdadm --create")));
}

/// The core fail-closed guarantee: an engine built with NO
/// `.with_confirm_sink` call at all -- not even `AlwaysRejectConfirmSink`
/// spelled out explicitly, just the bare default -- must refuse to run a
/// real, destructive `create`, and must not touch a single disk before
/// refusing. This is the entire point of the post-Stage-0 flip: a future
/// caller that simply forgets to wire a `ConfirmSink` gets a loud rejection
/// instead of a silent, unattended "yes".
#[test]
fn default_engine_without_a_confirm_sink_now_fails_closed() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone());

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );
    assert!(
        runner.get_recorded().iter().all(|c| !is_destructive(c)),
        "the default (no confirm sink wired) must not issue any destructive command"
    );
    assert!(!state_store.exists(), "the default must not persist any state");
}

/// The concrete "no one to ask" sink fails closed through the real engine,
/// not just in its own unit test -- this is what a daemon/Cockpit-spawn
/// caller should wire up for any `create`/`expand` it can't get a real
/// interactive answer for.
#[test]
fn always_reject_confirm_sink_fails_closed_through_the_engine() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let reject = AlwaysRejectConfirmSink;
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&reject);

    let err = engine.create(create_req(three_disks())).unwrap_err();
    assert!(matches!(err, OrchestrateError::Rejected(_)));
    assert!(runner.get_recorded().iter().all(|c| !is_destructive(c)));
    assert!(!state_store.exists());
}

/// Requirement 3 (non-interactive default must never be a quiet "yes") has
/// a mirror-image failure mode worth guarding too: `ConfirmSink` must not
/// fire AT ALL during `--dry-run`, so wiring a fail-closed sink into a real
/// run's engine (reused, say, for its `--dry-run` preview too) can't make
/// the preview itself fail for a reason that has nothing to do with dry-run.
#[test]
fn dry_run_never_invokes_confirm_sink() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let confirm = RecordingConfirmSink::rejecting();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&confirm);

    let state = engine.create(create_req(three_disks())).unwrap();

    assert_eq!(state.disks.len(), 3);
    assert!(
        confirm.requests().is_empty(),
        "dry-run must never call ConfirmSink"
    );
}

#[test]
fn confirm_reject_blocks_expand_before_any_destructive_command() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let confirm = RecordingConfirmSink::rejecting();
    let engine = seeded_engine(&runner, state_store, three_disks()).with_confirm_sink(&confirm);

    let cmds_before = runner.get_recorded().len();
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );

    let all_cmds = runner.get_recorded();
    let new_cmds = &all_cmds[cmds_before..];
    assert!(
        new_cmds.iter().all(|c| !is_destructive(c)),
        "a rejected expand must not issue any destructive command, got: {new_cmds:?}"
    );

    let requests = confirm.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].operation, "expand");
    assert!(requests[0].irreversible);
}

#[test]
fn create_reports_progress_stages_in_order() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let progress = RecordingProgressSink::new();
    let engine = OrchestrationEngine::new(&runner, state_store)
        .with_progress_sink(&progress)
        .with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req(three_disks())).unwrap();

    let updates = progress.updates();
    assert!(updates.iter().all(|u| u.operation == "create"), "{updates:?}");
    let stages: Vec<String> = updates.into_iter().map(|u| u.stage).collect();
    assert_eq!(stages, vec!["partition", "array", "lvm", "filesystem", "done"]);
}

#[test]
fn create_with_no_progress_sink_reports_nothing_and_behaves_identically() {
    // Stage 0 DoD: a caller that doesn't opt in gets today's behavior --
    // silence, not a panic or a changed result.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine.create(create_req(three_disks())).unwrap();
    assert_eq!(state.disks.len(), 3);
}

#[test]
fn expand_reports_progress_per_step_and_a_final_done() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let progress = RecordingProgressSink::new();
    let engine = seeded_engine(&runner, state_store, three_disks()).with_progress_sink(&progress);

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine.expand(expand_req(vec![new_disk])).unwrap();

    let updates = progress.updates();
    assert!(updates.iter().all(|u| u.operation == "expand"), "{updates:?}");
    assert!(
        updates.iter().any(|u| u.stage == "step-1-of-1"),
        "expected a per-step update, got: {updates:?}"
    );
    assert_eq!(updates.last().unwrap().stage, "done");
}

// ---------------------------------------------------------------------
// Scrub
// ---------------------------------------------------------------------

fn fresh_completed_scrub() -> StateScrubResult {
    StateScrubResult {
        finished_at: chrono::Utc::now().to_rfc3339(),
        outcome: ScrubOutcome::Completed,
        error_count: 0,
    }
}

/// Overwrite every band's `last_scrub` in the persisted state for `group`
/// (there is only ever one group in these tests) -- the earlier test fixture
/// helper.
fn seed_scrub_history(store: &StateStore, result: StateScrubResult) {
    let mut state = store.load().unwrap().unwrap();
    for band in &mut state.groups[0].bands {
        band.last_scrub = Some(result.clone());
    }
    store.save(&state).unwrap();
}

#[test]
fn scrub_start_writes_check_to_every_band_and_starts_btrfs_scrub() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    engine.scrub_start(None).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c == "sh -c echo check > /sys/block/md0/md/sync_action"),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("btrfs scrub start")),
        "{cmds:?}"
    );

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands[0].scrub_in_progress,
        "must record that a scrub was started here"
    );
}

#[test]
fn scrub_start_is_blocked_when_a_band_is_degraded() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md0");
    let engine = seeded_engine(&runner, state_store, three_disks());

    let err = engine.scrub_start(None).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("degraded"), "{err}");
    assert!(
        !runner
            .get_recorded()
            .iter()
            .any(|c| c.contains("sync_action") && c.contains("check")),
        "must never issue the scrub write once blocked"
    );
}

#[test]
fn scrub_start_is_blocked_while_an_expansion_is_in_progress() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let mut state = state_store.load().unwrap().unwrap();
    state.groups[0].expansion.in_progress = true;
    state_store.save(&state).unwrap();

    let err = engine.scrub_start(None).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("expansion"), "{err}");
}

#[test]
fn scrub_start_is_blocked_when_a_scrub_is_already_running() {
    // A band with ANY non-idle background activity (not just reshape) must
    // block a NEW scrub -- exercised here via a scrub already in flight,
    // the same general "background activity" guard `expand()` also uses.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());
    engine.scrub_start(None).unwrap();

    let err = engine.scrub_start(None).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("sync_action"), "{err}");
}

#[test]
fn expand_is_blocked_while_a_scrub_is_running() {
    // The direction that needed NEW code: `expand()`'s existing "band
    // has background activity" guard already rejects ANY non-idle
    // `sync_action`, and a running scrub reports `sync_action == "check"` --
    // proving the EXISTING guard covers this without modification.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());
    engine.scrub_start(None).unwrap();

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("sync_action"), "{err}");
}

#[test]
fn expand_reports_a_currently_running_btrfs_scrub_clearly_instead_of_e19s_stale_message() {
    // Mdadm's own per-device `check` scan (what the
    // sync_action guard watches) can finish well before Btrfs's own,
    // separate, filesystem-level scrub does -- `sync_action` reads back
    // `idle` while `btrfs scrub status` still reports `running`. The old
    // code fell through the mdadm guard (sync_action is idle, so it passes)
    // straight to the "has not been fully checked for errors... run
    // `fs scrub start`"
    // message -- actively misleading, since a scrub IS running, just not
    // finished yet. `skip_scrub_check: true` (via `expand_req`) proves this
    // is checked independently of the freshness gate, not a special case
    // of it.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        btrfs_scrub_status_response: "Status:           running\n".to_string(),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine.expand(expand_req(vec![new_disk])).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    let msg = format!("{err}");
    assert!(msg.contains("currently has a scrub running"), "{msg}");
    assert!(
        !msg.contains("has not been fully checked for errors"),
        "must not show the misleading staleness message: {msg}"
    );
}

#[test]
fn scrub_cancel_writes_idle_and_clears_scrub_in_progress_even_when_degraded() {
    // Cancelling must always be reachable -- including on a degraded array
    // -- so an operator is never stuck unable to stop a scrub.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let healthy_runner = FailingRunner::healthy();
    let engine = seeded_engine(&healthy_runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();

    let degraded_runner = FailingRunner::degraded_band("md0");
    let engine2 =
        OrchestrationEngine::new(&degraded_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    engine2.scrub_cancel(None).unwrap();

    assert!(
        degraded_runner
            .get_recorded()
            .iter()
            .any(|c| c == "sh -c echo idle > /sys/block/md0/md/sync_action"),
        "{:?}",
        degraded_runner.get_recorded()
    );
    let saved = state_store.load().unwrap().unwrap();
    assert!(!saved.groups[0].bands[0].scrub_in_progress);
}

#[test]
fn scrub_cancel_treats_an_already_finished_btrfs_scrub_as_success_and_still_persists_idle_state() {
    // Btrfs's half of
    // a scrub routinely finishes long before mdadm's per-band `check` half
    // does. By the time an operator cancels, `btrfs scrub cancel` reports
    // "not running" -- exit 2, real message reproduced by the mock below --
    // even though mdadm's own cancel just succeeded moments earlier. Before
    // the fix that error `?`-propagated out of `scrub_cancel` immediately,
    // so `scrub_in_progress` was never cleared and `state.toml` was never
    // saved even though mdadm's own cancel HAD already landed.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        btrfs_scrub_cancel_reports_not_running: true,
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();

    engine.scrub_cancel(None).unwrap();

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands.iter().all(|b| !b.scrub_in_progress),
        "mdadm's cancel succeeded and btrfs's own scrub had already finished -- \
         scrub_in_progress must be cleared and persisted, not left stuck at true"
    );
}

#[test]
fn scrub_cancel_tolerates_an_mdadm_write_that_reports_failure_but_sync_action_already_reads_idle() {
    // Symmetry: the same "already achieved is not a failure" reasoning
    // applies to mdadm's own `echo idle` write -- a write that reports
    // failure while `/sys/block/<md>/md/sync_action` already reads back
    // `idle` reached the desired end state regardless (rule 2: trust the
    // sysfs read, not the write's exit code).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let healthy_runner = FailingRunner::healthy();
    let engine = seeded_engine(&healthy_runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();

    // A fresh runner whose own `sync_action` was never put into "check" (it
    // never saw `scrub_start`'s write), so it already answers "idle" the
    // same way a real control file would right after mdadm's write lands --
    // but the `echo idle` write command itself is forced to report failure.
    let runner2 = FailingRunner {
        fail_forever_trigger: Some("sh -c echo idle > /sys/block/md0/md/sync_action".to_string()),
        ..FailingRunner::healthy()
    };
    let engine2 = OrchestrationEngine::new(&runner2, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);

    engine2.scrub_cancel(None).unwrap();

    let saved = state_store.load().unwrap().unwrap();
    assert!(!saved.groups[0].bands[0].scrub_in_progress);
}

#[test]
fn scrub_cancel_surfaces_a_genuine_btrfs_failure_and_leaves_scrub_in_progress_true() {
    // "not running" is the ONLY btrfs cancel error this project treats
    // as benign. Any other failure (permission denied, btrfs missing,
    // device gone) must still be reported to the operator, and state must
    // not falsely claim the scrub stopped when the filesystem itself still
    // reports it running.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        fail_forever_trigger: Some("btrfs scrub cancel".to_string()),
        btrfs_scrub_status_response: "Status:           running\n".to_string(),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();

    let err = engine.scrub_cancel(None).unwrap_err();
    assert!(
        matches!(err, OrchestrateError::Validation(_)),
        "expected a reported failure, got {err:?}"
    );
    assert!(format!("{err}").contains("btrfs cancel"), "{err}");

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands.iter().all(|b| b.scrub_in_progress),
        "btrfs is still really running -- must not silently clear scrub_in_progress"
    );
}

#[test]
fn scrub_cancel_persists_state_and_reports_a_read_back_failure_on_one_band() {
    // A narrower recurrence -- an earlier fix addressed the CANCEL step's early
    // exit; this is the same early-exit shape one step later, in the
    // READ-BACK that runs after both cancels. Measured scenario: a two-band
    // group with a scrub running; both mdadm cancels succeed, btrfs's own
    // cancel reports the tolerated "not running", and THEN band 1's
    // `sync_action` read-back fails -- plausible because mdadm can
    // auto-stop/degrade an array mid-`check`, exactly what a scrub exists
    // to catch. Before the fix, the bare `?` on that read propagated
    // immediately: `store.save` never ran (both bands stayed `true` on disk
    // despite band 0's genuine, successful cancel) and the caller got a
    // bare `ExecError` with no mention of anything already collected into
    // `failures`.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let healthy_runner = FailingRunner::healthy();
    let engine = seeded_engine(&healthy_runner, state_store.clone(), hetero_disks());
    engine.scrub_start(None).unwrap();

    // A fresh runner for the cancel call: its own `scrubbing_mds` is empty,
    // so both bands' `echo idle` writes succeed and (once reached) their
    // `sync_action` reads already answer "idle" -- same "reflects the real
    // world after cancel took effect" convention the earlier tests above use --
    // except band 1's read is forced to fail.
    let runner2 = FailingRunner {
        btrfs_scrub_cancel_reports_not_running: true,
        fail_forever_trigger: Some("cat /sys/block/md1/md/sync_action".to_string()),
        ..FailingRunner::healthy()
    };
    let engine2 = OrchestrationEngine::new(&runner2, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine2.scrub_cancel(None).unwrap_err();

    // Half 2: the read-back failure must be reported, not silently dropped.
    assert!(
        matches!(err, OrchestrateError::Validation(_)),
        "expected a reported failure, got {err:?}"
    );
    assert!(
        format!("{err}").contains("md1"),
        "read-back failure must name the band: {err}"
    );

    // Half 1: state.toml must be written regardless of the read-back
    // failure -- band 0's genuine cancel confirmed by a successful read,
    // band 1 fail-safe (an unreadable signal is treated as still running,
    // per the rule, not as "safe to clear").
    let saved = state_store.load().unwrap().unwrap();
    assert!(
        !saved.groups[0].bands[0].scrub_in_progress,
        "band 0's cancel was confirmed by a successful read"
    );
    assert!(
        saved.groups[0].bands[1].scrub_in_progress,
        "band 1's unreadable signal must fail safe to \"still running\", not false"
    );
}

#[test]
fn scrub_status_persists_the_result_once_every_band_and_btrfs_finish() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();

    // Still running: sync_action reports "check" until finish_scrub().
    let mid = engine.scrub_status(None).unwrap();
    assert!(mid.running);
    let saved_mid = state_store.load().unwrap().unwrap();
    assert!(
        saved_mid.groups[0].bands[0].last_scrub.is_none(),
        "must not persist a result while still running"
    );

    runner.finish_scrub();
    let done = engine.scrub_status(None).unwrap();
    assert!(!done.running);

    let saved = state_store.load().unwrap().unwrap();
    let last_scrub = saved.groups[0].bands[0]
        .last_scrub
        .as_ref()
        .expect("must persist a result once finished");
    assert_eq!(last_scrub.outcome, ScrubOutcome::Completed);
    assert_eq!(last_scrub.error_count, 0);
    assert!(!saved.groups[0].bands[0].scrub_in_progress);
}

#[test]
fn reconcile_self_heals_a_stale_scrub_in_progress_flag_left_by_a_scheduled_scrub() {
    // The systemd timer this project generates only ever runs
    // `fs scrub start` -- nothing calls `fs scrub status` afterward to
    // observe a scheduled scrub that finished on its own hours later, so
    // `scrub_in_progress` stayed stuck `true` in `state.toml` forever
    // until an operator happened to run `fs scrub status` by hand. Proves
    // `reconcile()` alone -- never `scrub_status()` -- clears it once the
    // real scrub has actually finished, matching this project's existing
    // "trust real kernel state over recorded state" principle (`reconcile
    // ()`'s deferred-resize completion already works this way).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();
    assert!(state_store.load().unwrap().unwrap().groups[0].bands[0].scrub_in_progress);

    // The scrub finishes on its own (real `sync_action` back to `idle`),
    // but nothing ever calls `fs scrub status` -- simulating exactly what
    // the scheduled-scrub timer leaves behind.
    runner.finish_scrub();

    let outcome = engine
        .reconcile()
        .unwrap()
        .expect("an active array must be found");
    assert!(
        !outcome.state.groups[0].bands[0].scrub_in_progress,
        "reconcile() must self-heal the stale flag once the real scrub has finished"
    );
    // The self-heal itself must be reported, not just visible as a
    // side effect on the returned state -- same "what did reconcile
    // actually DO" gap the member-removal/resize paths had.
    assert_eq!(
        outcome.performed,
        vec![ReconcileAction::ScrubSelfHealed {
            group: outcome.state.groups[0].name.clone(),
            band_index: outcome.state.groups[0].bands[0].index,
            md_name: outcome.state.groups[0].bands[0].md_name.clone(),
            error_count: 0,
        }],
        "reconcile() must report the scrub self-heal it performed: {:?}",
        outcome.performed
    );
    let saved = state_store.load().unwrap().unwrap();
    assert!(
        !saved.groups[0].bands[0].scrub_in_progress,
        "the self-heal must be persisted, not just returned"
    );
    let last_scrub = saved.groups[0].bands[0]
        .last_scrub
        .as_ref()
        .expect("reconcile must also record the result");
    assert_eq!(last_scrub.outcome, ScrubOutcome::Completed);
}

#[test]
fn reconcile_does_not_probe_scrub_status_for_a_group_that_was_never_scrubbed() {
    // Guards the cost side of that fix: a group whose bands were never
    // marked `scrub_in_progress` has nothing to reconcile, so `reconcile()`
    // must not needlessly read every group's live sync_action/mismatch_cnt/
    // `btrfs scrub status` on every call -- this is also what several
    // "must not touch anything before returning" validation-path tests
    // rely on for groups with no scrub history at all.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    engine.reconcile().unwrap();

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("sync_action") || c.contains("scrub status") || c.contains("mismatch_cnt")),
        "must not probe scrub status for a group that was never scrubbed: {cmds:?}"
    );
}

#[test]
fn scrub_status_reports_the_real_error_count_from_mismatch_cnt() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        mismatch_cnt_response: "7\n".to_string(),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    engine.scrub_start(None).unwrap();
    runner.finish_scrub();

    let report = engine.scrub_status(None).unwrap();
    assert_eq!(
        report.error_count, 7,
        "mdadm's mismatch_cnt must surface as the scrub's error count"
    );

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups[0].bands[0].last_scrub.as_ref().unwrap().error_count,
        7
    );
}

// ---------------------------------------------------------------------
// Notifications (webhook + systemd-notify)
// ---------------------------------------------------------------------

#[test]
fn scrub_status_reports_success_and_persists_the_result_even_when_webhook_delivery_fails() {
    // The strict earlier requirement: a scrub that found real errors must
    // still be reported as successfully OBSERVED (and persisted) even if
    // notifying about it fails -- a dead webhook must never make an
    // otherwise-successful `fs scrub status` call look like it failed.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        mismatch_cnt_response: "7\n".to_string(),
        fail_forever_trigger: Some("curl".to_string()),
        ..FailingRunner::healthy()
    };
    let engine =
        seeded_engine(&runner, state_store.clone(), three_disks()).with_notify_policy(NotifyPolicy {
            webhook_url: Some("https://dead.example.com".to_string()),
            systemd_notify: false,
        });
    engine.scrub_start(None).unwrap();
    runner.finish_scrub();

    let report = engine
        .scrub_status(None)
        .expect("a dead webhook must not fail the scrub status call");
    assert_eq!(report.error_count, 7);

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups[0].bands[0].last_scrub.as_ref().unwrap().error_count,
        7,
        "the real result must still be persisted despite the notification failure"
    );

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c.starts_with("curl ")),
        "must still have attempted delivery: {cmds:?}"
    );
}

#[test]
fn scrub_status_posts_a_webhook_when_errors_are_found() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        mismatch_cnt_response: "3\n".to_string(),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks()).with_notify_policy(NotifyPolicy {
        webhook_url: Some("https://hooks.example.com/x".to_string()),
        systemd_notify: false,
    });
    engine.scrub_start(None).unwrap();
    runner.finish_scrub();

    engine.scrub_status(None).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c.starts_with("curl ")
            && c.contains("https://hooks.example.com/x")
            && c.contains("scrub_errors_found")
            && c.contains("\"error_count\":3")),
        "{cmds:?}"
    );
}

#[test]
fn scrub_status_does_not_notify_when_no_errors_were_found() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks()).with_notify_policy(NotifyPolicy {
        webhook_url: Some("https://hooks.example.com/x".to_string()),
        systemd_notify: true,
    });
    engine.scrub_start(None).unwrap();
    runner.finish_scrub();

    engine.scrub_status(None).unwrap();

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("curl ") || c.starts_with("systemd-notify")),
        "{cmds:?}"
    );
}

#[test]
fn check_health_reports_a_degraded_band_via_systemd_notify_by_default() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md0");
    // Default policy: systemd_notify is ON without any explicit opt-in
    // (earlier lesson) -- exercised here via `seeded_engine`'s plain
    // `OrchestrationEngine::new`, no `.with_notify_policy` call at all.
    let engine = seeded_engine(&runner, state_store, three_disks());

    engine.check_health().unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("systemd-notify --status=") && c.contains("DEGRADED")),
        "{cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.starts_with("curl ")),
        "no webhook configured -- must not attempt one: {cmds:?}"
    );
}

/// `systemd-notify --status=...` never reaches `journalctl -u
/// <unit>` for the `Type=oneshot` units this project generates -- see
/// `NotifyExecutor::systemd_notify`'s doc comment. `OrchestrationEngine::
/// notify` must ALSO emit a `tracing` event (captured here) so the event is
/// visible via the process's own stderr, which every generated unit's
/// journal entry already captures with no `Environment=RUST_LOG=...`
/// needed (see `shr-bin::init_tracing`).
///
/// A callsite's `tracing::Interest` is cached in a PROCESS-WIDE
/// static (`tracing::callsite`), not scoped to any one subscriber. This
/// binary has other tests that call `check_health()`/`reconcile()` with the
/// default `NotifyPolicy` (`systemd_notify: true`) and NO subscriber
/// installed at all (e.g. `check_health_never_fails_even_when_webhook_
/// delivery_fails`), running concurrently on other threads. the earlier
/// fix used `tracing::subscriber::with_default` (a THREAD-LOCAL dispatcher)
/// plus a manual `rebuild_interest_cache()` inside that scope -- but that
/// rebuild only re-evaluates against THIS thread's dispatcher, and only
/// while the `with_default` scope is alive. A concurrent, subscriber-less
/// thread can still win the race to be the first anywhere in the process to
/// touch `OrchestrationEngine::notify`'s `tracing::warn!` callsite and
/// re-cache `never` afterward. Measured: ~1 failure in 12 runs even with
/// that mitigation in place.
///
/// The actual fix: install exactly ONE subscriber for the entire life of
/// the test BINARY via `tracing::subscriber::set_global_default`, guarded
/// by a `OnceLock` so it runs exactly once no matter which test gets there
/// first. Unlike `with_default`, `set_global_default` unconditionally
/// triggers its own `rebuild_interest_cache()` internally -- so the first
/// call, from whichever test wins the `OnceLock`, corrects any callsite
/// already mis-cached as `never` by an earlier subscriber-less thread. From
/// then on the global default is permanently installed for the rest of the
/// process; nothing here ever calls `rebuild_interest_cache()` again with
/// no subscriber active, so the interest can never regress to `never`. The
/// race is gone by construction, not by timing.
///
/// Per-test isolation (so the NEGATIVE test below still sees nothing from a
/// concurrently running positive test, and vice versa) comes from routing
/// the one global subscriber's writer through a THREAD-LOCAL buffer instead
/// of a thread-local dispatcher -- libtest gives every `#[test]` fn its own
/// fresh OS thread, so each test's buffer starts empty and is invisible to
/// every other thread.
#[derive(Clone, Default)]
struct ThreadLocalTracingWriter;

thread_local! {
    static TRACING_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl std::io::Write for ThreadLocalTracingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        TRACING_BUF.with(|b| b.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalTracingWriter {
    type Writer = ThreadLocalTracingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ThreadLocalTracingWriter
    }
}

static GLOBAL_TRACING: OnceLock<()> = OnceLock::new();

fn capture_tracing(f: impl FnOnce()) -> String {
    GLOBAL_TRACING.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ThreadLocalTracingWriter)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("exactly one global default for this test binary, guarded by OnceLock");
    });

    TRACING_BUF.with(|b| b.borrow_mut().clear());
    f();
    TRACING_BUF.with(|b| String::from_utf8(b.borrow().clone()).unwrap())
}

#[test]
fn check_health_emits_a_tracing_warn_event_with_the_status_line_when_systemd_notify_is_enabled() {
    // The POSITIVE side of the discriminator: the free, default-on
    // local channel must reach something that lands in the journal without
    // any operator-set RUST_LOG -- `tracing::warn!`, not merely the
    // `systemd-notify` subprocess call (which the sibling test below
    // already covers and which does not actually reach `journalctl` for a
    // oneshot unit).
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md0");
    let engine = seeded_engine(&runner, state_store, three_disks());

    let output = capture_tracing(|| {
        engine.check_health().unwrap();
    });

    assert!(
        output.contains("DEGRADED"),
        "expected the status line in the tracing output, got: {output}"
    );
    assert!(
        output.contains("WARN"),
        "expected a WARN-level event (visible without RUST_LOG), got: {output}"
    );
}

#[test]
fn check_health_emits_no_tracing_event_when_systemd_notify_is_disabled() {
    // The NEGATIVE side of the same discriminator (per this project's rule:
    // a green light that looks identical on both sides verifies nothing) --
    // an operator who explicitly turns the local channel off must not still
    // get journal spam from it.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md0");
    let engine = seeded_engine(&runner, state_store, three_disks()).with_notify_policy(NotifyPolicy {
        webhook_url: None,
        systemd_notify: false,
    });

    let output = capture_tracing(|| {
        engine.check_health().unwrap();
    });

    assert!(
        output.trim().is_empty(),
        "systemd_notify=false must emit nothing to the journal, got: {output}"
    );
}

#[test]
fn check_health_never_fails_even_when_webhook_delivery_fails() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        degraded_only_for: Some("md0".to_string()),
        fail_forever_trigger: Some("curl".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks()).with_notify_policy(NotifyPolicy {
        webhook_url: Some("https://dead.example.com".to_string()),
        systemd_notify: true,
    });

    engine
        .check_health()
        .expect("a dead webhook must not fail check_health");

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c.starts_with("curl ")),
        "must still have attempted webhook delivery: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("systemd-notify")),
        "the OTHER channel must still fire even though webhook delivery failed: {cmds:?}"
    );
}

#[test]
fn check_health_fires_smart_worsened_and_persists_the_new_total_when_reallocated_rises() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        smartctl_response: r#"{"smart_status":{"passed":true},"temperature":{"current":35},
              "ata_smart_attributes":{"table":[{"id":5,"raw":{"value":5}}]}}"#
            .to_string(),
        ..FailingRunner::healthy()
    };
    let engine =
        seeded_engine(&runner, state_store.clone(), three_disks()).with_notify_policy(NotifyPolicy {
            webhook_url: Some("https://hooks.example.com/x".to_string()),
            systemd_notify: false,
        });

    // Seed a prior reading of 0 so this run's smartctl-reported total of
    // (5 per disk * 3 member disks =) 15 computes a real, positive delta.
    let mut state = state_store.load().unwrap().unwrap();
    state.groups[0].bands[0].last_smart_reallocated = Some(0);
    state_store.save(&state).unwrap();

    engine.check_health().unwrap();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c.starts_with("curl ")
            && c.contains("smart_worsened")
            && c.contains("\"reallocated_delta\":15")),
        "{cmds:?}"
    );
    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups[0].bands[0].last_smart_reallocated,
        Some(15),
        "the new absolute total must be persisted so the NEXT check computes a fresh delta"
    );
}

#[test]
fn check_health_does_not_notify_a_healthy_unchanged_array() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks()).with_notify_policy(NotifyPolicy {
        webhook_url: Some("https://hooks.example.com/x".to_string()),
        systemd_notify: true,
    });

    engine.check_health().unwrap();

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("curl ") || c.starts_with("systemd-notify")),
        "a healthy, unchanged array must not notify anything: {cmds:?}"
    );
}

#[test]
fn check_health_notifies_array_missing_instead_of_aborting_when_the_array_is_not_assembled() {
    // Real-guest repro: `state.toml` intact, but `/dev/md0` was never
    // assembled (a reboot came back without its member devices, or an
    // operator ran `mdadm --stop` by hand). `degraded_count`'s `cat
    // /sys/block/md0/md/degraded` has no file to read and fails -- measured
    // on the guest as `cat: ... No such file or directory`, exit 1. The old
    // code let that `?` propagate straight out of `check_health()`, so the
    // periodic timer -- the ONE mechanism that notices trouble when no
    // human is looking -- died with an execution error and notified
    // NOTHING, in exactly the situation (total loss of a band) that most
    // needs an alert.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        degraded_array_missing_for: Some("md0".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks());

    engine
        .check_health()
        .expect("a missing array must not abort the whole health-check tick");

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("systemd-notify --status=") && c.contains("no live mdadm array")),
        "a vanished array is the worst case worth alerting on -- must notify, not stay silent: {cmds:?}"
    );
}

#[test]
fn check_health_still_reaches_a_later_groups_genuinely_degraded_band_after_one_groups_array_is_missing() {
    // Mutation guard: a fix that merely stops the `?` from aborting
    // but still SKIPS every later group without checking it would pass a
    // naive "check_health() returns Ok" test. Two groups: shr1's md0 has no
    // live array at all; shr2's md1 is genuinely, live-and-reduced
    // degraded. Only a tick that keeps looping after shr1 ever reaches
    // shr2's band and fires its `Degraded` notification.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        degraded_array_missing_for: Some("md0".to_string()),
        degraded_only_for: Some("md1".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);
    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    engine
        .check_health()
        .expect("one group's missing array must not abort the tick for the rest");

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("systemd-notify --status=") && c.contains("no live mdadm array")),
        "shr1's missing array must still notify: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("systemd-notify --status=") && c.contains("DEGRADED")),
        "shr2's genuinely degraded band must still be reached -- proves the tick did not stop at shr1: {cmds:?}"
    );
}

#[test]
fn unparseable_degraded_is_not_reported_as_array_missing() {
    // Boundary (coordinator review): `degraded_count` fails in TWO
    // structurally different ways -- the real guest's `NonZeroExit` with
    // "No such file or directory" (array genuinely not assembled -- the
    // case above), and its own `Prerequisite` when `cat` SUCCEEDS (array IS
    // assembled) but `/sys/block/<md>/md/degraded`'s contents don't parse
    // as a number. `Err(_)` would have reported `ArrayMissing` for BOTH --
    // a false, specific claim about a live machine for the second case.
    // shr1's md0 hits the unparseable case; shr2's md1 is genuinely
    // degraded, proving the sweep still reaches it either way.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        degraded_unparseable_for: Some("md0".to_string()),
        degraded_only_for: Some("md1".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);
    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    engine
        .check_health()
        .expect("an unparseable-but-assembled band must not abort the tick either");

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("systemd-notify --status=") && c.contains("no live mdadm array")),
        "unparseable degraded content is NOT evidence the array is missing -- must not claim it is: {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("systemd-notify --status=") && c.contains("DEGRADED")),
        "shr2's genuinely degraded band must still be reached: {cmds:?}"
    );
}

#[test]
fn scrub_status_never_fabricates_history_for_a_band_that_was_never_scrubbed() {
    // Critical correctness guard: a band that has ALWAYS been idle (no
    // scrub ever started here) must not have `scrub_status` invent a "0
    // errors, just completed" record just because `sync_action == "idle"`
    // is indistinguishable from "just finished" at the raw sysfs level.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let report = engine.scrub_status(None).unwrap();
    assert!(!report.running);

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands[0].last_scrub.is_none(),
        "must never fabricate a scrub result for a band that was never actually scrubbed"
    );
}

#[test]
fn expand_is_blocked_when_no_band_has_ever_been_scrubbed() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: false,
        })
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("scrub"), "{err}");
}

#[test]
fn expand_succeeds_when_every_band_has_a_recent_completed_scrub() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    seed_scrub_history(&state_store, fresh_completed_scrub());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: false,
        })
        .expect("a fresh, completed scrub history must satisfy the freshness gate");
}

#[test]
fn expand_succeeds_despite_missing_scrub_history_when_skip_scrub_check_is_set() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: true,
        })
        .expect("--skip-scrub-check must bypass the freshness gate explicitly");
}

#[test]
fn expand_is_blocked_when_the_last_scrub_is_stale() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    let stale = StateScrubResult {
        finished_at: (chrono::Utc::now() - chrono::Duration::days(31)).to_rfc3339(),
        outcome: ScrubOutcome::Completed,
        error_count: 0,
    };
    seed_scrub_history(&state_store, stale);

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: false,
        })
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("scrub"), "{err}");
}

#[test]
fn expand_is_blocked_when_the_last_scrub_was_cancelled_not_completed() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    let cancelled = StateScrubResult {
        finished_at: chrono::Utc::now().to_rfc3339(),
        outcome: ScrubOutcome::Cancelled,
        error_count: 0,
    };
    seed_scrub_history(&state_store, cancelled);

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .expand(ExpandRequest {
            name: None,
            new_disks: vec![new_disk],
            system_disks: vec!["sda".to_string()],
            skip_scrub_check: false,
        })
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
}

// ---------------------------------------------------------------------
// Disk replace, recompress
// ---------------------------------------------------------------------

#[test]
fn recompress_issues_the_expected_command_when_healthy() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    engine.recompress(None, "zstd:9").unwrap();

    // `defragment -c` only accepts a bare algorithm name -- the level
    // must NOT be appended, real btrfs-progs v6.12 rejects `-czstd:9`.
    let recorded = runner.get_recorded();
    assert!(
        recorded
            .iter()
            .any(|c| c.starts_with("btrfs filesystem defragment -r -czstd ")),
        "{recorded:?}"
    );
    assert!(
        !recorded.iter().any(|c| c.contains("-czstd:9")),
        "defragment must not receive the mount-option level: {recorded:?}"
    );
    // The level lives in the remount, issued before defragment so the
    // rewritten extents actually pick it up.
    assert!(
        recorded.iter().any(|c| c.contains("remount,compress=zstd:9")),
        "{recorded:?}"
    );

    let state = state_store.load().unwrap().unwrap();
    assert_eq!(state.groups[0].filesystem.compression, "zstd:9");
}

#[test]
fn recompress_updates_fstab_managed_block() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    engine.recompress(None, "zstd:9").unwrap();

    let fstab = std::fs::read_to_string(dir.path().join("fstab")).unwrap();
    assert!(fstab.contains("compress=zstd:9"), "{fstab}");
}

#[test]
fn recompress_rejects_an_invalid_compression_string_before_touching_anything() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let err = engine.recompress(None, "bogus").unwrap_err();
    assert!(matches!(err, OrchestrateError::Exec(_)), "{err:?}");
    assert!(!runner
        .get_recorded()
        .iter()
        .any(|c| c.contains("defragment") || c.contains("remount")));

    let state = state_store.load().unwrap().unwrap();
    assert_eq!(
        state.groups[0].filesystem.compression, "zstd:3",
        "must not be updated on rejection"
    );
}

#[test]
fn recompress_is_blocked_when_degraded() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md0");
    let engine = seeded_engine(&runner, state_store, three_disks());

    let err = engine.recompress(None, "zstd:9").unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(!runner.get_recorded().iter().any(|c| c.contains("defragment")));
}

// ---------------------------------------------------------------------
// @/@snapshots layout + fs snapshot create
// ---------------------------------------------------------------------

#[test]
fn create_builds_the_at_and_at_snapshots_subvolumes_and_ends_up_mounted_on_at() {
    // `create` must build the `@`/`@snapshots` layout from the
    // start -- mount the filesystem's default (top-level) subvolume just
    // long enough to create both subvolumes, then swap to the REAL,
    // ongoing `subvol=@` mount. No migration/legacy-layout branch exists
    //: this is the only path `create()` has ever taken.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let state = engine.create(create_req(three_disks())).unwrap();
    let mount_point = state.filesystem.mount_point.clone();

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c == &format!("btrfs subvolume create {mount_point}/@")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c == &format!("btrfs subvolume create {mount_point}/@snapshots")),
        "{cmds:?}"
    );

    let mount_cmds: Vec<&String> = cmds.iter().filter(|c| c.starts_with("mount ")).collect();
    assert_eq!(
        mount_cmds.len(),
        2,
        "expected the top-level mount THEN the subvol=@ mount: {cmds:?}"
    );
    assert!(
        !mount_cmds[0].contains("subvol="),
        "the first mount must be the default subvolume: {mount_cmds:?}"
    );
    assert!(
        mount_cmds[1].contains("subvol=@") && !mount_cmds[1].contains("subvol=@snapshots"),
        "{mount_cmds:?}"
    );

    // The top-level mount must be torn down before the real `subvol=@`
    // mount replaces it -- Btrfs only honors `subvol=` on a fresh mount.
    let unmount_pos = cmds
        .iter()
        .position(|c| c == &format!("umount {mount_point}"))
        .expect("must unmount");
    let mount_at_pos = cmds.iter().position(|c| c == mount_cmds[1]).unwrap();
    assert!(
        unmount_pos < mount_at_pos,
        "must unmount before remounting subvol=@: {cmds:?}"
    );
}

#[test]
fn create_writes_subvol_at_into_the_fstab_managed_block() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let fstab_path = dir.path().join("fstab");
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store)
        .with_conf_paths(dir.path().join("mdadm.conf"), &fstab_path)
        .with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req(three_disks())).unwrap();

    let fstab = std::fs::read_to_string(&fstab_path).unwrap();
    assert!(fstab.contains("subvol=@,"), "{fstab}");
}

#[test]
fn preview_create_shows_the_same_at_snapshots_subvolume_commands_as_real_execution() {
    // Preview fidelity: what the confirmation screen shows must be exactly
    // what `create()` actually runs, or an operator confirms one thing and
    // gets another.
    let dir = tempdir().unwrap();
    let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let (state, commands) = shr_orchestrate::preview_create(store, create_req(three_disks())).unwrap();
    let mount_point = &state.filesystem.mount_point;

    assert!(
        commands
            .iter()
            .any(|c| c == &format!("btrfs subvolume create {mount_point}/@")),
        "{commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c == &format!("btrfs subvolume create {mount_point}/@snapshots")),
        "{commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c.starts_with("mount ") && c.contains("subvol=@") && !c.contains("subvol=@snapshots")),
        "{commands:?}"
    );
}

#[test]
fn snapshot_create_mounts_the_top_level_subvolume_and_snapshots_at_into_snapshots_name() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();
    let mark = runner.get_recorded().len();

    engine.snapshot_create(None, "before-upgrade").unwrap();

    let cmds = &runner.get_recorded()[mark..];
    let scratch = "/run/shr-rs/snapshot-mount-default";
    assert!(
        cmds.iter().any(|c| c.starts_with("mount ")
            && c.ends_with(&format!(" {scratch}"))
            && !c.contains("subvol=")),
        "must mount the default subvolume, not subvol=@: {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c
                == &format!("btrfs subvolume snapshot -r {scratch}/@ {scratch}/@snapshots/before-upgrade")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c == &format!("umount {scratch}")),
        "must unmount the scratch mount: {cmds:?}"
    );
}

#[test]
fn snapshot_create_rejects_a_name_containing_a_slash_before_touching_anything() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();
    let mark = runner.get_recorded().len();

    let err = engine.snapshot_create(None, "../escape").unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert_eq!(
        &runner.get_recorded()[mark..],
        &[] as &[String],
        "must not touch anything"
    );
}

#[test]
fn snapshot_create_still_unmounts_the_scratch_mount_when_the_snapshot_itself_fails() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let healthy = FailingRunner::healthy();
    let create_engine =
        OrchestrationEngine::new(&healthy, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = create_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let failing = FailingRunner {
        fail_forever_trigger: Some("subvolume snapshot".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&failing, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.snapshot_create(None, "will-fail").unwrap_err();
    assert!(matches!(err, OrchestrateError::Exec(_)), "{err:?}");

    let cmds = failing.get_recorded();
    let scratch = "/run/shr-rs/snapshot-mount-default";
    assert!(
        cmds.iter().any(|c| c == &format!("umount {scratch}")),
        "must still unmount the scratch mount after the snapshot itself failed: {cmds:?}"
    );
}

#[test]
fn snapshot_create_rejects_a_name_that_already_exists_instead_of_letting_btrfs_report_read_only() {
    // Real-guest repro, through Cockpit on a `tank` group: asking for a
    // snapshot name that already existed failed with btrfs's own `ERROR:
    // Could not create subvolume: Read-only file system`, which names
    // neither the cause nor the operator's own input. That message is
    // btrfs behaving correctly -- `subvolume snapshot -r <src> <dest>`
    // treats an EXISTING `dest` as the parent directory to create the new
    // subvolume inside, and every snapshot here is read-only (`-r`), so the
    // create lands in a read-only subvolume and is refused. The collision
    // is knowable before then, so it is caught here instead.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let healthy = FailingRunner::healthy();
    let create_engine =
        OrchestrationEngine::new(&healthy, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = create_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner {
        ls_response: "nightly\nbefore-upgrade\n".to_string(),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.snapshot_create(None, "before-upgrade").unwrap_err();
    match &err {
        OrchestrateError::Validation(message) => {
            assert!(
                message.contains("before-upgrade"),
                "must name the snapshot asked for: {message}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }

    let cmds = runner.get_recorded();
    assert!(
        !cmds.iter().any(|c| c.starts_with("btrfs subvolume snapshot")),
        "must not reach btrfs once the collision is known: {cmds:?}"
    );
    // The collision is only visible with the top-level subvolume mounted,
    // so the scratch mount still has to come down on this path too.
    assert!(
        cmds.iter()
            .any(|c| c == "umount /run/shr-rs/snapshot-mount-default"),
        "must still unmount the scratch mount: {cmds:?}"
    );
}

#[test]
fn snapshot_create_still_proceeds_when_other_snapshots_exist_under_a_different_name() {
    // The collision check must key off the requested name, not off
    // "@snapshots is non-empty" -- every group past its first snapshot
    // would otherwise be unable to take another.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let healthy = FailingRunner::healthy();
    let create_engine =
        OrchestrationEngine::new(&healthy, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = create_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner {
        ls_response: "nightly\nbefore-upgrade\n".to_string(),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.snapshot_create(None, "after-upgrade").unwrap();

    let cmds = runner.get_recorded();
    let scratch = "/run/shr-rs/snapshot-mount-default";
    assert!(
        cmds.iter()
            .any(|c| c
                == &format!("btrfs subvolume snapshot -r {scratch}/@ {scratch}/@snapshots/after-upgrade")),
        "{cmds:?}"
    );
}

#[test]
fn snapshot_create_rejects_the_reserved_auto_prefix() {
    // The `auto-` namespace is reserved for `snapshot_auto_run`'s own
    // automated snapshots -- an operator (or a script) must never be able
    // to manually create one there, or pruning could no longer tell
    // "shr-rs made this" apart from "an operator made this and merely
    // reused the reserved prefix".
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let mark = runner.get_recorded().len();
    let err = engine.snapshot_create(None, "auto-hand-crafted").unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("reserved"), "{err}");
    assert!(
        runner.get_recorded()[mark..].is_empty(),
        "must reject before touching anything"
    );
}

#[test]
fn snapshot_auto_run_creates_one_auto_snapshot_per_group() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    seed_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();
    seed_engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let summary = engine.snapshot_auto_run(7).unwrap();
    assert_eq!(summary.len(), 2, "{summary:?}");

    let cmds = runner.get_recorded();
    let snapshot_cmds: Vec<&String> = cmds
        .iter()
        .filter(|c| c.starts_with("btrfs subvolume snapshot"))
        .collect();
    assert_eq!(snapshot_cmds.len(), 2, "{cmds:?}");
    assert!(
        snapshot_cmds
            .iter()
            .all(|c| c.contains(&format!("@snapshots/{AUTO_SNAPSHOT_PREFIX}"))),
        "every created snapshot must carry the reserved auto- prefix: {snapshot_cmds:?}"
    );
}

#[test]
fn snapshot_auto_run_is_a_noop_when_no_active_array_exists() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let summary = engine.snapshot_auto_run(7).unwrap();
    assert!(summary.is_empty(), "{summary:?}");
    assert!(
        runner.get_recorded().is_empty(),
        "a host with no state.toml has nothing to snapshot yet"
    );
}

/// `state.toml` can outlive the array it describes -- real-guest
/// repro was an unplanned power-cycle before mdadm/LVM reassembled the
/// group, `state.toml` still on disk. `create_snapshot_now`'s scratch mount
/// then fails with the exact real-guest shape ("special device ... does not
/// exist", exit 32); the old code `?`-propagated that straight out of the
/// `for group_name` loop, which both failed the whole systemd unit every
/// tick (this fn's own doc comment says a host with no active array must be
/// a silent no-op -- an existing-but-unassembled array is the same class of
/// "nothing to do here yet") AND cost every group AFTER the broken one its
/// snapshot too. `shr1`'s scratch mount is made to fail here; `shr2` must
/// still get its snapshot, and the summary must say `shr1` was skipped
/// rather than just omitting it silently.
#[test]
fn snapshot_auto_run_skips_a_group_whose_array_is_not_assembled_but_still_snapshots_the_rest() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    seed_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();
    seed_engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    let runner = FailingRunner {
        mount_missing_device_for: Some("snapshot-mount-shr1".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let summary = engine.snapshot_auto_run(7).unwrap();
    assert_eq!(
        summary.len(),
        2,
        "one line per group, even the skipped one: {summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|l| l.contains("shr1") && l.contains("SKIPPED")),
        "shr1's line must say it was skipped, not silently vanish: {summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|l| l.contains("shr2") && l.contains("created")),
        "shr2 must still get its snapshot despite shr1's failure: {summary:?}"
    );

    let cmds = runner.get_recorded();
    let snapshot_cmds: Vec<&String> = cmds
        .iter()
        .filter(|c| c.starts_with("btrfs subvolume snapshot"))
        .collect();
    assert_eq!(
        snapshot_cmds.len(),
        1,
        "only shr2's snapshot must actually have been taken: {cmds:?}"
    );
    assert!(snapshot_cmds[0].contains("snapshot-mount-shr2"), "{cmds:?}");
}

/// (tightening, pins the boundary `is_missing_array_device` must never
/// re-cross): real mount also exits 32 with `mount: <point>: mount point
/// does not exist.` when the SCRATCH DIRECTORY itself is missing/unusable
/// -- a DIFFERENT failure from `special device ... does not exist.`
/// (device absent) that `snapshot_auto_run` tolerates. `create_snapshot_now`
/// `mkdir -p`s that directory immediately before mounting it, so this is a
/// genuine, actionable failure (stale non-directory at the path, read-only
/// `/run`, tmpfs pressure), not "array not assembled yet". Confusing the
/// two would silently reclassify a real backup failure as an expected
/// no-op -- this must stay a hard error, never a SKIPPED summary line.
#[test]
fn mount_point_missing_is_not_treated_as_absent_array() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    seed_engine.create(create_req_named("g1", three_disks())).unwrap();

    let runner = FailingRunner {
        mount_missing_mountpoint_for: Some("snapshot-mount-g1".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.snapshot_auto_run(7).unwrap_err();
    assert!(matches!(err, OrchestrateError::Exec(_)), "{err:?}");
    assert!(format!("{err}").contains("mount point does not exist"), "{err}");
}

/// Pruning deletes only the OLDEST
/// `auto-`-prefixed entries beyond `keep`, in ascending (oldest-first)
/// order, and must never touch a manually-named snapshot even though it
/// sits in the very same `@snapshots` directory.
#[test]
fn snapshot_auto_run_prunes_oldest_auto_snapshots_beyond_keep_and_never_a_manual_one() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    seed_engine.create(create_req(three_disks())).unwrap();

    // Four existing auto-snapshots (already sorted oldest-first by their
    // embedded timestamp) plus one an operator created by hand -- `keep=2`
    // must delete exactly the two oldest auto- ones and leave the rest,
    // manual snapshot included, alone.
    let runner = FailingRunner {
        ls_response: "auto-20260101T000000Z\n\
                      auto-20260102T000000Z\n\
                      auto-20260103T000000Z\n\
                      manual-keep-me-forever\n\
                      auto-20260104T000000Z\n"
            .to_string(),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let summary = engine.snapshot_auto_run(2).unwrap();
    assert!(summary[0].contains("pruned 2"), "{summary:?}");

    let cmds = runner.get_recorded();
    let deletes: Vec<&String> = cmds
        .iter()
        .filter(|c| c.starts_with("btrfs subvolume delete"))
        .collect();
    assert_eq!(deletes.len(), 2, "{deletes:?}");
    assert!(
        deletes.iter().any(|c| c.contains("auto-20260101T000000Z")),
        "{deletes:?}"
    );
    assert!(
        deletes.iter().any(|c| c.contains("auto-20260102T000000Z")),
        "{deletes:?}"
    );
    assert!(
        !deletes
            .iter()
            .any(|c| c.contains("auto-20260103T000000Z") || c.contains("auto-20260104T000000Z")),
        "must keep the {} newest auto-snapshots: {deletes:?}",
        2
    );
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("btrfs subvolume delete") && c.contains("manual-keep-me-forever")),
        "a manually created snapshot must never be pruned, regardless of its age: {cmds:?}"
    );
}

#[test]
fn replace_disk_partitions_the_new_disk_and_issues_mdadm_replace() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let result = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap();

    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        result.disks
    );
    assert!(
        !result.disks.iter().any(|d| d.id == "ata-DISK1"),
        "old disk must be gone: {:?}",
        result.disks
    );

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c.contains("mklabel gpt") && c.contains("ata-DISK4-NEW")),
        "{cmds:?}"
    );
    // Mdadm's `--replace old --with new` requires
    // `new` to ALREADY be a member of the array -- `--with`'s target must
    // have been `--add`-ed FIRST, as a distinct, EARLIER command, not just
    // present somewhere in the log.
    let add_pos = cmds
        .iter()
        .position(|c| c.starts_with("mdadm") && c.contains("--add"))
        .expect("no --add command before --replace: {cmds:?}");
    let replace = cmds
        .iter()
        .find(|c| c.starts_with("mdadm") && c.contains("--replace") && c.contains("--with"))
        .expect("no --replace --with command");
    let replace_pos = cmds.iter().position(|c| c == replace).unwrap();
    assert!(add_pos < replace_pos, "--add must precede --replace: {cmds:?}");

    // The device `--add`-ed and the device named after `--with` must be the
    // SAME path -- otherwise `--add` proves nothing about what `--replace`
    // actually targets.
    let added_device = cmds[add_pos].rsplit(' ').next().unwrap();
    let with_device = replace.rsplit(' ').next().unwrap();
    assert_eq!(added_device, with_device, "{cmds:?}");

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups[0].disks.len(),
        3,
        "member count must be unchanged, just one identity swapped"
    );
}

/// `replace_disk` updated the DISK's own
/// `partitions` list with the new part_uuid, but never touched the BAND's
/// `member_partitions` list, which kept naming the OLD (now physically gone)
/// part_uuid. `status` never notices (it reads the live kernel and the array
/// really is healthy), but every path that rebuilds the logical layout from
/// state.toml -- `snapshot_from_state`, which `expand` calls -- resolves each
/// `member_partitions` entry back to an owning disk and fails outright once
/// the old uuid can't be found on any disk anymore. Asserting only that
/// `member_partitions` contains the new uuid would be too weak (order
/// matters, and it wouldn't prove the resolver actually works), so this
/// exercises the real consequence: a later `expand()` must succeed.
#[test]
fn replace_disk_keeps_the_band_resolvable_for_a_later_expand() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap();

    // The real symptom, checked FIRST: a later expand must resolve every
    // band member back to a disk. Before the fix this fails with "band 0
    // member partition <old uuid> not found on any disk".
    let another_disk = resolved_disk("ata-DISK5-NEW", "sdf", 4_000_000_000_000);
    let result = engine.expand(expand_req(vec![another_disk]));
    assert!(
        result.is_ok(),
        "expand after replace must succeed, not fail resolving member_partitions: {:?}",
        result.err()
    );

    // Weaker, but pinned too: the band's member list must name the NEW
    // disk's part_uuid, in the SAME slot the old one occupied -- member
    // order mirrors mdadm's device order, so this isn't just "contains it
    // somewhere".
    let saved = state_store.load().unwrap().unwrap();
    let new_disk_state = saved.groups[0]
        .disks
        .iter()
        .find(|d| d.id == "ata-DISK4-NEW")
        .unwrap();
    let new_part_uuid = new_disk_state.partitions[0].part_uuid.clone();
    assert!(
        saved.groups[0].bands[0]
            .member_partitions
            .contains(&new_part_uuid),
        "band must reference the new disk's part_uuid: {:?}",
        saved.groups[0].bands[0].member_partitions
    );
}

/// `disk replace --old`'s doc comment promises by-id, kernel name, OR
/// serial, but the engine used to match ONLY the literal by-id string --
/// Cockpit's replace dialog sent a kernel name and silently failed
/// forever. This pins the serial form: `resolved_disk` stamps `SN-<kernel>`
/// as `ata-DISK1`'s serial at `create()` time (`three_disks()`), so
/// `"SN-sdb"` must resolve to the same member `"ata-DISK1"` does, WITHOUT
/// touching the runner at all -- matched purely against `state.toml`,
/// because the disk being replaced is very often already physically gone,
/// which is exactly when a live lookup can't help.
#[test]
fn replace_disk_accepts_the_recorded_serial_with_no_live_system_call() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    // No `by_id_kernel_names` configured -- a `readlink -e` against ANY
    // by-id path (what the live kernel-name fallback would issue to resolve
    // `--old`) would hit `simulated_failure`, so a pass here proves the
    // serial match never reached that fallback. (A LEGITIMATE, unrelated
    // `readlink -e /dev/disk/by-partuuid/...` still happens later in
    // `replace_disk` -- checking whether the old member's partition symlink
    // still resolves post-replace -- so this only excludes by-id lookups,
    // not `readlink` entirely.)
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let result = engine
        .replace_disk(None, "SN-sdb", &new_disk, &["sda".to_string()])
        .unwrap();

    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        result.disks
    );
    assert!(
        !result.disks.iter().any(|d| d.id == "ata-DISK1"),
        "old disk must be gone: {:?}",
        result.disks
    );
    assert!(
        !runner
            .get_recorded()
            .iter()
            .any(|c| c.starts_with("readlink") && c.contains("by-id")),
        "serial matching must be resolvable from state.toml alone, with no by-id live lookup: {:?}",
        runner.get_recorded()
    );
}

/// Kernel-name form: `--old sdc` (no by-id/serial match against
/// state.toml) must still resolve to whichever recorded member's by-id
/// symlink actually points at `sdc` right now. The mock maps THREE distinct
/// by-id names to THREE distinct kernel names, deliberately not in list
/// order (`ata-DISK2` -> `sdc`, the others left unmapped so they'd fail if
/// queried) -- picking `ata-DISK1` (first in the group) would be wrong here,
/// so this only passes if the match is genuinely path-specific, not "first
/// disk in the group".
#[test]
fn replace_disk_accepts_a_live_kernel_name_by_resolving_the_right_disks_by_id_symlink() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner {
        by_id_kernel_names: std::collections::HashMap::from([("ata-DISK2".to_string(), "sdc".to_string())]),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let result = engine
        .replace_disk(None, "sdc", &new_disk, &["sda".to_string()])
        .unwrap();

    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        result.disks
    );
    assert!(
        !result.disks.iter().any(|d| d.id == "ata-DISK2"),
        "the disk mapped to sdc must be gone: {:?}",
        result.disks
    );
    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK1"),
        "ata-DISK1 (never mapped to sdc) must be untouched, proving the match wasn't just \"first in group\": {:?}",
        result.disks
    );
}

/// A reference that resolves to NOTHING (no by-id/serial match in
/// state.toml, and no live disk's by-id symlink points at it either) must
/// name the identifier forms `--old` actually accepts -- pinning the
/// PROPERTY (both forms named), not one exact sentence, so a rewrite can't
/// satisfy this by accident while still only documenting one form (the
/// original defect).
#[test]
fn replace_disk_unknown_reference_names_the_accepted_identifier_forms() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner::healthy(); // no by-id/readlink mapping -- every live lookup fails
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "loop99", &new_disk, &["sda".to_string()])
        .unwrap_err();

    let msg = err.to_string().to_lowercase();
    assert!(msg.contains("by-id"), "{msg}");
    assert!(msg.contains("serial"), "{msg}");
}

#[test]
fn replace_disk_removes_the_old_member_once_the_replace_copy_has_already_finished() {
    // `mdadm --replace old --with new` marks `old`
    // faulty once the copy finishes but never detaches it -- a separate
    // `--remove` is required. `FailingRunner::healthy()`'s `sync_action`
    // reads back `idle` immediately after `--replace` (no `--grow`/
    // `--replace` currently in flight is tracked as still running), the
    // same as the tiny loop-device repro where the copy finishes before
    // the CLI command even returns -- so this must remove the old member
    // right away, not just leave it attached like before this fix.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap();

    let cmds = runner.get_recorded();
    let replace = cmds
        .iter()
        .find(|c| c.starts_with("mdadm") && c.contains("--replace") && c.contains("--with"))
        .expect("no --replace --with command");
    let old_device = replace
        .split("--replace ")
        .nth(1)
        .and_then(|rest| rest.split(" --with").next())
        .expect("old device between --replace and --with");

    let remove_cmd = cmds
        .iter()
        .find(|c| c.starts_with("mdadm") && c.contains("--remove") && c.ends_with(old_device))
        .unwrap_or_else(|| panic!("expected a cleanup `mdadm --remove` for the old member: {cmds:?}"));
    let replace_pos = cmds.iter().position(|c| c == replace).unwrap();
    let remove_pos = cmds.iter().position(|c| c == remove_cmd).unwrap();
    assert!(
        replace_pos < remove_pos,
        "the old member must be removed AFTER --replace: {cmds:?}"
    );
}

#[test]
fn replace_disk_treats_removal_as_success_when_mdstat_no_longer_lists_the_member_even_though_its_partition_symlink_still_resolves(
) {
    // Real-guest repro: `reconcile` (and, via the same code path,
    // `replace_disk`'s own synchronous cleanup) reported a successful
    // `mdadm --remove` as a FAILURE. Root cause: the post-remove check
    // used `MdadmExecutor::resolve_member_kernel_name`, which is just
    // `readlink -e <by-partuuid path>` -- it checks whether the partition
    // still exists on disk, not whether it's still an array member.
    // `mdadm --remove` detaches a member WITHOUT deleting its partition,
    // so that symlink resolves fine forever after a real, successful
    // removal. This mock reproduces exactly that: `readlink_kernel_name`
    // stays `Some` (the partition is still there) while `mdstat_content`
    // already shows the array without it (the kernel's real post-removal
    // state) -- the fix must trust `/proc/mdstat`, not `readlink`.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner {
        readlink_kernel_name: Some("sdb1".to_string()),
        mdstat_content: Mutex::new(
            "Personalities : [raid5]\nmd0 : active raid5 sde1[3] sdc1[1] sdd1[2]\n      \
             8378368 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/3] [UUU]\n"
                .to_string(),
        ),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);

    let result = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .expect(
            "a real removal must not be reported as a failure just because its by-partuuid \
                 symlink still resolves",
        );
    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        result.disks
    );

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands[0].pending_member_removal.is_none(),
        "a genuinely completed removal must not be left recorded as pending forever"
    );
}

#[test]
fn replace_disk_surfaces_a_failed_old_member_removal_instead_of_reporting_plain_success() {
    // If the copy has already finished (safe to remove) but the
    // cleanup `mdadm --remove` itself fails, this must not report a plain
    // success -- the operator needs to know the old member may still be
    // attached and reusable-elsewhere is not actually true yet.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner::failing_once_on("--remove");
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("attached"), "{err}");

    // The physical replacement (partition + --add + --replace) already
    // happened for real and state.toml already reflects it -- ONLY the
    // cleanup step failed, so the record must not be rolled back.
    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        saved.groups[0].disks
    );
    assert!(
        !saved.groups[0].disks.iter().any(|d| d.id == "ata-DISK1"),
        "{:?}",
        saved.groups[0].disks
    );
}

#[test]
fn replace_disk_defers_old_member_removal_while_the_copy_is_still_running() {
    // `--replace` only STARTS the copy -- removing `old` while
    // `sync_action` still reads back something other than `idle` would
    // detach the live source of an in-progress recovery. This must not be
    // attempted; it also must not be reported as a failure (a real,
    // large disk's copy taking hours is the expected case, not an error),
    // but the operator still needs telling what remains.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner::reshaping();
    let progress = RecordingProgressSink::new();
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_progress_sink(&progress)
        .with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let result = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .expect("a copy still running must not be reported as a failure");
    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        result.disks
    );

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("mdadm") && c.contains("--remove")),
        "must not remove the old member while its copy is still running: {cmds:?}"
    );
    let updates = progress.updates();
    assert!(
        updates
            .iter()
            .any(|u| u.message.contains("stays attached") && u.message.contains("shr-rs reconcile")),
        "must tell the operator the old disk remains attached and how to finish removing it \
         once the copy is done: {updates:?}"
    );
}

#[test]
fn reconcile_completes_a_deferred_replace_member_removal_once_the_copy_finishes() {
    // `replace_disk` alone cannot always finish
    // removing the old member -- a real disk's copy can still be running
    // when the command returns (proven above by
    // `replace_disk_defers_old_member_removal_while_the_copy_is_still_
    // running`). Before this fix, NOTHING ever finished that cleanup
    // afterward: the stale, faulty old member stayed attached to the live
    // array forever, `state.toml` and the kernel diverged, and the disk
    // could never be reused. `reconcile()` must complete it once the copy
    // is actually done -- the same self-heal-against-real-kernel-state
    // pattern an earlier fix already uses for `scrub_in_progress`, and it must
    // actually issue the `--remove` and re-verify against the kernel, not
    // just clear the bookkeeping.
    //
    // (real-guest repro, found running this exact scenario against a
    // real VM after the earlier fix): `shr-rs reconcile` DID remove the old
    // member from the live array here -- kernel membership went from 4
    // members (`loop13p1[4] loop12p1[3](F) loop11p1[1] loop10p1[0]`) down
    // to 3, and `pending_member_removal` was cleared from `state.toml` --
    // but the CLI printed only `Reconcile: nothing pending.` This test now
    // also asserts on `reconcile()`'s returned `performed` list, which is
    // exactly what the CLI's report is built from.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    // `readlink_kernel_name` simulates the old member (kernel name `sdb1`,
    // the original partition on `ata-DISK1`/`sdb`) still resolving --
    // i.e. still physically attached -- for as long as it hasn't been
    // `--remove`d (see `removed_member_paths`'s doc comment).
    let runner = FailingRunner {
        readlink_kernel_name: Some("sdb1".to_string()),
        ..FailingRunner::reshaping()
    };
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .expect("a copy still running must not be reported as a failure");

    let deferred = state_store.load().unwrap().unwrap();
    let old_member_path = deferred.groups[0].bands[0]
        .pending_member_removal
        .clone()
        .expect("replace_disk must record which device is left for reconcile to remove later");

    // The copy finishes on its own, and the kernel now shows the old
    // member still attached but marked faulty -- exactly the real-guest
    // repro's `cat /proc/mdstat` shape (its own repro line).
    runner.finish_reshape();
    runner.set_mdstat_content(
        "Personalities : [raid5]\nmd0 : active raid5 sde1[3] sdc1[1] sdd1[2] sdb1[0](F)\n      \
         8378368 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/3] [UUU]\n",
    );
    let mark = runner.get_recorded().len();

    let outcome = engine
        .reconcile()
        .unwrap()
        .expect("an active array must be found");
    assert!(
        outcome.state.groups[0].bands[0].pending_member_removal.is_none(),
        "the deferred removal must be cleared once actually completed"
    );
    // The removal itself must come back as a reportable fact, not
    // just be inferable (or not) from a diff against the pre-call state --
    // this is the exact piece of information the real-guest repro's
    // `Reconcile: nothing pending.` line failed to convey.
    assert_eq!(
        outcome.performed,
        vec![ReconcileAction::MemberRemoved {
            group: outcome.state.groups[0].name.clone(),
            band_index: outcome.state.groups[0].bands[0].index,
            md_name: outcome.state.groups[0].bands[0].md_name.clone(),
            member_path: old_member_path.clone(),
        }],
        "reconcile() must report exactly which member it removed: {:?}",
        outcome.performed
    );

    let cmds = &runner.get_recorded()[mark..];
    let remove_cmd = cmds
        .iter()
        .find(|c| c.starts_with("mdadm") && c.contains("--remove") && c.ends_with(old_member_path.as_str()))
        .unwrap_or_else(|| panic!("reconcile must issue the deferred `mdadm --remove`: {cmds:?}"));
    // Trust the kernel, not just the exit code (its own rule, reused
    // here): after issuing `--remove`, reconcile must re-resolve the same
    // path to confirm it's actually gone before clearing the flag.
    let remove_pos = cmds.iter().position(|c| c == remove_cmd).unwrap();
    let reverify_pos = cmds
        .iter()
        .rposition(|c| c == &format!("readlink -e {old_member_path}"))
        .expect("reconcile must re-check the kernel after removing");
    assert!(
        reverify_pos > remove_pos,
        "re-verification must happen AFTER --remove: {cmds:?}"
    );

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands[0].pending_member_removal.is_none(),
        "the self-heal must be persisted, not just returned"
    );
}

#[test]
fn reconcile_does_not_touch_the_array_when_no_replace_removal_is_pending() {
    // Cost guard (mirrors `reconcile_does_not_probe_scrub_status_for_a_
    // group_that_was_never_scrubbed`): a group with no deferred replace
    // cleanup pending must issue ZERO extra commands for it -- `reconcile()`
    // must stay a true no-op when there's nothing to reconcile.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    engine.reconcile().unwrap();

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("--remove") || c.starts_with("readlink")),
        "must not probe for a stray replaced member when none is pending: {cmds:?}"
    );
}

#[test]
fn check_health_also_completes_a_deferred_replace_member_removal() {
    // The periodic health-check timer (installed by `shr-rs
    // schedule install`, running every 15 minutes) is what actually
    // guarantees a real, multi-hour replace copy's cleanup completes even
    // if the operator never runs `expand`/`reconcile` again -- proven
    // separately from `reconcile()` itself since `check_health` is a
    // distinct entrypoint that doesn't call `reconcile()`.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let seed_runner = FailingRunner::healthy();
    let seed_engine =
        OrchestrationEngine::new(&seed_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let created = seed_engine.create(create_req(three_disks())).unwrap();
    state_store.save(&StateFile::new(vec![created])).unwrap();

    let runner = FailingRunner {
        readlink_kernel_name: Some("sdb1".to_string()),
        ..FailingRunner::reshaping()
    };
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap();

    runner.finish_reshape();
    runner.set_mdstat_content(
        "Personalities : [raid5]\nmd0 : active raid5 sde1[3] sdc1[1] sdd1[2] sdb1[0](F)\n      \
         8378368 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/3] [UUU]\n",
    );

    engine.check_health().unwrap();

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands[0].pending_member_removal.is_none(),
        "check_health's periodic tick must also finish the deferred removal"
    );
}

#[test]
fn replace_disk_undoes_the_added_spare_and_reports_want_replacement_when_replace_fails() {
    // If `--replace old --with new` itself fails AFTER `--add` already
    // succeeded, this must not leave a silent partial application -- the
    // spare this call added must be removed (best-effort) and the operator
    // must be told the array may still carry mdadm's own internal
    // `want_replacement` marker on the old member.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner {
        fail_forever_trigger: Some("--replace".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    let msg = format!("{err}");
    assert!(msg.contains("want_replacement"), "{msg}");

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c.starts_with("mdadm") && c.contains("--add")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("mdadm") && c.contains("--remove")),
        "must attempt to undo the spare it just added: {cmds:?}"
    );
}

#[test]
fn replace_disk_rejects_a_smaller_replacement() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let mark = runner.get_recorded().len();
    let smaller = resolved_disk("ata-SMALL", "sde", 2_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &smaller, &["sda".to_string()])
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("smaller"), "{err}");
    assert_eq!(
        runner.get_recorded().len(),
        mark,
        "must never touch any disk once blocked"
    );
}

#[test]
fn replace_disk_accepts_an_equal_size_replacement() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let same_size = resolved_disk("ata-DISK1-NEW", "sde", 4_000_000_000_000);
    engine
        .replace_disk(None, "ata-DISK1", &same_size, &["sda".to_string()])
        .expect("equal size must be allowed");
}

#[test]
fn replace_disk_rejects_a_disk_that_already_belongs_to_another_group() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let already_a_member = resolved_disk("ata-DISK2", "sdc", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &already_a_member, &["sda".to_string()])
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("already belongs"), "{err}");
}

#[test]
fn replace_disk_rejects_a_system_disk() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store, three_disks());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sde".to_string()])
        .unwrap_err();
    assert!(format!("{err}").contains("system disk"), "{err:?}");
}

#[test]
fn replace_disk_is_blocked_when_a_different_member_is_degraded() {
    // The Stage C brief's "any degraded band
    // blocks replace" was wrong -- replace is often exactly how an operator
    // fixes a failed member. What must still block it is a failure the
    // replace target DOESN'T explain: here, `old_disk`'s own member
    // (`sdb1`) resolves fine and reports healthy, but `sdc1` -- a
    // DIFFERENT member of the same band -- is the one marked faulty.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_by_a_different_member("md0", "sdb1");
    let engine = seeded_engine(&runner, state_store, three_disks());
    // Set only AFTER seeding -- see `degraded_by_a_different_member`'s doc
    // comment for why baking this into the runner up front would have
    // made `create()` itself allocate a different `md_name`.
    runner.set_mdstat_content(
        "Personalities : [raid5]\n\
         md0 : active raid5 sdb1[0] sdc1[1](F) sdd1[2]\n      \
         7813894144 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/2] [U_U]\n\n\
         unused devices: <none>\n",
    );

    let mark = runner.get_recorded().len();
    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("OTHER than"), "{err}");
    // The block itself needs read-only commands (degraded_count, readlink,
    // `cat /proc/mdstat`) to make its determination -- what must never
    // happen is any MUTATING command against a disk/array, from this call
    // onward (excluding whatever `seeded_engine`'s own `create()` recorded).
    let cmds = &runner.get_recorded()[mark..];
    assert!(
        !cmds.iter().any(|c| c.contains("mklabel")
            || c.contains("mkpart")
            || c.contains("--replace")
            || c.contains("--add")
            || c.contains("--remove")),
        "must never touch any disk once blocked: {cmds:?}"
    );
}

#[test]
fn replace_disk_rebuilds_via_add_when_old_disk_has_already_failed() {
    // `old_disk` itself is the (only) failed member of its band --
    // exactly the scenario replace exists to recover from. `--replace`
    // requires `old` to be a LIVE member, which it no longer is (simulated
    // here by a dangling `readlink -e`, i.e. `readlink_kernel_name: None`,
    // `degraded_band`'s default) -- so this must go through `--remove` +
    // `--add` (rebuild) instead, and must NOT be blocked.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::degraded_band("md0");
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());

    let new_disk = resolved_disk("ata-DISK4-NEW", "sde", 4_000_000_000_000);
    let result = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .expect("old_disk itself failing must not block its own replace");

    assert!(
        result.disks.iter().any(|d| d.id == "ata-DISK4-NEW"),
        "{:?}",
        result.disks
    );

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("mdadm") && c.contains("--remove")),
        "must remove the already-failed old member first: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("mdadm") && c.contains("--add")),
        "must rebuild via --add, not --replace: {cmds:?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("mdadm") && c.contains("--replace")),
        "must not attempt --replace against a member that's already gone: {cmds:?}"
    );

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups[0].disks.len(),
        3,
        "member count must be unchanged, just one identity swapped"
    );
}

#[test]
fn replace_disk_is_blocked_by_reject_confirm_sink_before_any_destructive_command() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    // seeded_engine's own confirm sink already approved the `create()` used
    // to seed the fixture; build a SEPARATE engine over the same store with
    // the fail-closed DEFAULT confirm sink (no `.with_confirm_sink` call)
    // to prove `replace_disk` is gated by it too.
    let _ = seeded_engine(&runner, state_store.clone(), three_disks());
    let engine = OrchestrationEngine::new(&runner, state_store.clone());

    let new_disk = resolved_disk("ata-DISK4", "sde", 4_000_000_000_000);
    let err = engine
        .replace_disk(None, "ata-DISK1", &new_disk, &["sda".to_string()])
        .unwrap_err();
    assert!(matches!(err, OrchestrateError::Rejected(_)), "{err:?}");

    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].disks.iter().any(|d| d.id == "ata-DISK1"),
        "old disk must be untouched"
    );
}

// --- Destroy ------------------------------------------------------

#[test]
fn destroy_removes_only_the_target_group_leaving_the_other_groups_and_configs_intact() {
    // The core correctness requirement, same shape as Phase 4's
    // `write_managed_configs` multi-group trap: destroying one group must
    // never touch another group's state.toml entry OR its mdadm.conf/fstab
    // lines.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let mdadm_conf = dir.path().join("mdadm.conf");
    let fstab = dir.path().join("fstab");
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_conf_paths(&mdadm_conf, &fstab)
        .with_confirm_sink(&ALWAYS_CONFIRM);

    let shr1 = engine.create(create_req_named("shr1", three_disks())).unwrap();
    let shr2 = engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    engine.destroy(Some("shr1"), false).unwrap();

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(saved.groups.len(), 1, "{:?}", saved.groups);
    assert_eq!(saved.groups[0].name, "shr2");

    let mdadm_conf_after = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(
        !mdadm_conf_after.contains(&format!("ARRAY /dev/{}", shr1.bands[0].md_name)),
        "shr1's ARRAY line must be gone: {mdadm_conf_after}"
    );
    assert!(
        mdadm_conf_after.contains(&format!("ARRAY /dev/{}", shr2.bands[0].md_name)),
        "shr2's ARRAY line must survive: {mdadm_conf_after}"
    );

    let fstab_after = std::fs::read_to_string(&fstab).unwrap();
    assert!(
        !fstab_after.contains(&shr1.filesystem.mount_point),
        "{fstab_after}"
    );
    assert!(
        fstab_after.contains(&shr2.filesystem.mount_point),
        "{fstab_after}"
    );

    let cmds = runner.get_recorded();
    assert!(
        cmds.iter()
            .any(|c| c == &format!("umount {}", shr1.filesystem.mount_point)),
        "{cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("mdadm --stop") && c.contains(&shr1.bands[0].md_name)),
        "{cmds:?}"
    );
}

/// A destroyed group's own `shr-rs-scrub-*`
/// systemd timer/service used to be left behind forever, permanently
/// firing `fs scrub start --name <group-that-no-longer-exists>` -- the
/// exact orphan class an earlier fix already addressed for mdadm.conf/fstab, reproduced
/// in systemd. Proves BOTH halves via the command log: `shr1`'s timer gets
/// `systemctl disable --now`d (and its files actually deleted), while
/// `shr2`'s own unit pair -- seeded the same way `schedule install` would
/// for a real multi-group host -- is never even mentioned.
#[test]
fn destroy_removes_only_the_target_groups_scrub_unit_leaving_other_groups_units_intact() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let unit_dir = dir.path().join("systemd-units");
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_unit_dir(&unit_dir)
        .with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    // Seed both groups' scrub units for real, exactly like `shr-rs schedule
    // install` would -- proves cleanup is scoped by the SAME path-deriving
    // function (`scrub_unit_paths`) `write_scrub_timer_units` uses, not a
    // second, independently-hand-rolled naming scheme that could drift.
    let state = state_store.load().unwrap().unwrap();
    write_scrub_timer_units(&unit_dir, &state, std::path::Path::new("/usr/local/bin/shr-rs")).unwrap();
    let (shr1_service, shr1_timer) = scrub_unit_paths(&unit_dir, "shr1");
    let (shr2_service, shr2_timer) = scrub_unit_paths(&unit_dir, "shr2");
    assert!(shr1_service.exists() && shr1_timer.exists() && shr2_service.exists() && shr2_timer.exists());

    let mark = runner.get_recorded().len();
    engine.destroy(Some("shr1"), false).unwrap();
    let cmds = &runner.get_recorded()[mark..];

    assert!(
        cmds.iter()
            .any(|c| c == "systemctl disable --now shr-rs-scrub-shr1.timer"),
        "must disable shr1's own timer: {cmds:?}"
    );
    assert!(cmds.iter().any(|c| c == "systemctl daemon-reload"), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.contains("shr2")),
        "shr2's unit must never be mentioned by a destroy of shr1: {cmds:?}"
    );

    assert!(
        !shr1_service.exists(),
        "shr1's service file must actually be deleted"
    );
    assert!(!shr1_timer.exists(), "shr1's timer file must actually be deleted");
    assert!(shr2_service.exists(), "shr2's service file must survive");
    assert!(shr2_timer.exists(), "shr2's timer file must survive");
}

/// A same-named unit an operator hand-wrote (no shr-rs ownership
/// marker) must never be touched by `destroy()`'s cleanup -- proven via the
/// command log showing NO `systemctl disable`/file deletion for it, and the
/// file itself surviving on disk untouched.
#[test]
fn destroy_never_touches_a_hand_written_unit_that_merely_shares_the_groups_unit_name() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let unit_dir = dir.path().join("systemd-units");
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_unit_dir(&unit_dir)
        .with_confirm_sink(&ALWAYS_CONFIRM);
    engine.create(create_req_named("shr1", three_disks())).unwrap();

    let (hand_written, _timer) = scrub_unit_paths(&unit_dir, "shr1");
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(
        &hand_written,
        "[Unit]\nDescription=an operator wrote this by hand, not shr-rs\n",
    )
    .unwrap();

    let mark = runner.get_recorded().len();
    engine.destroy(Some("shr1"), false).unwrap();
    let cmds = &runner.get_recorded()[mark..];

    assert!(
        !cmds.iter().any(|c| c.starts_with("systemctl")),
        "an unowned lookalike must never trigger any systemctl call: {cmds:?}"
    );
    assert!(
        hand_written.exists(),
        "the operator's own unit must never be deleted"
    );
    let content = std::fs::read_to_string(&hand_written).unwrap();
    assert!(
        content.contains("an operator wrote this by hand"),
        "content must be untouched: {content}"
    );
}

#[test]
fn destroy_without_zeroing_records_the_array_so_it_is_never_auto_assembled_again() {
    // Leaving the superblocks is the recoverable choice, but on its own it
    // also means the kernel's incremental assembly finds those members at
    // the next boot and resurrects the dead array -- observed on a real
    // guest, where a destroyed group returned as `/dev/md6` owning a device
    // number and belonging to no group. Recording it is what puts an
    // `ARRAY <ignore>` line in mdadm.conf: metadata kept, assembly stopped.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let mdadm_conf = dir.path().join("mdadm.conf");
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_conf_paths(&mdadm_conf, dir.path().join("fstab"))
        .with_confirm_sink(&ALWAYS_CONFIRM);

    let created = engine.create(create_req_named("shr1", three_disks())).unwrap();
    let md_uuid = created.bands[0]
        .md_uuid
        .clone()
        .expect("band must have a real md_uuid");

    engine.destroy(Some("shr1"), false).unwrap();

    let state = state_store.load().unwrap().unwrap();
    assert!(state.groups.is_empty());
    assert_eq!(state.retired_arrays.len(), created.bands.len());
    assert!(state.retired_arrays.iter().all(|r| r.group_name == "shr1"));
    assert!(state.retired_arrays.iter().any(|r| r.md_uuid == md_uuid));

    let conf = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(
        conf.contains(&format!("ARRAY <ignore> UUID={md_uuid}")),
        "the destroyed array must be marked never-assemble: {conf}"
    );
}

#[test]
fn destroy_with_zeroing_records_nothing_because_there_is_no_metadata_left_to_ignore() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let mdadm_conf = dir.path().join("mdadm.conf");
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_conf_paths(&mdadm_conf, dir.path().join("fstab"))
        .with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine.destroy(Some("shr1"), true).unwrap();

    let state = state_store.load().unwrap().unwrap();
    assert!(
        state.retired_arrays.is_empty(),
        "zeroed superblocks leave nothing for an <ignore> line to match: {:?}",
        state.retired_arrays
    );
    let conf = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(!conf.contains("<ignore>"), "{conf}");
}

#[test]
fn reusing_a_retired_arrays_disks_prunes_its_ignore_entry() {
    // Once `create` repartitions those same disks and `mdadm --create`
    // writes fresh superblocks over them, the old array has no physical
    // trace left to suppress -- keeping the entry would leave mdadm.conf
    // accumulating `<ignore>` lines forever.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let mdadm_conf = dir.path().join("mdadm.conf");
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_conf_paths(&mdadm_conf, dir.path().join("fstab"))
        .with_confirm_sink(&ALWAYS_CONFIRM);

    engine.create(create_req_named("shr1", three_disks())).unwrap();
    engine.destroy(Some("shr1"), false).unwrap();
    assert!(!state_store.load().unwrap().unwrap().retired_arrays.is_empty());

    // The SAME disks, handed to a new group.
    engine.create(create_req_named("shr2", three_disks())).unwrap();

    let state = state_store.load().unwrap().unwrap();
    assert!(
        state.retired_arrays.is_empty(),
        "reusing the disks must prune the stale entry: {:?}",
        state.retired_arrays
    );
    let conf = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(!conf.contains("<ignore>"), "{conf}");
}

#[test]
fn destroy_is_blocked_by_reject_confirm_sink_before_touching_anything() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let _ = seeded_engine(&runner, state_store.clone(), three_disks());
    let engine = OrchestrationEngine::new(&runner, state_store.clone());

    let mark = runner.get_recorded().len();
    let err = engine.destroy(None, false).unwrap_err();
    assert!(matches!(err, OrchestrateError::Rejected(_)), "{err:?}");
    // The background-activity guard right before confirm legitimately reads
    // `sync_action` (read-only) -- what must never happen is anything that
    // actually touches the filesystem/array/LVM stack.
    let cmds = &runner.get_recorded()[mark..];
    assert!(
        !cmds.iter().any(|c| c.starts_with("umount")
            || c.starts_with("lvremove")
            || c.starts_with("vgremove")
            || c.starts_with("pvremove")
            || c.contains("--stop")
            || c.contains("--zero-superblock")),
        "must never touch anything before confirmation: {cmds:?}"
    );

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(saved.groups.len(), 1, "group must still exist");
}

#[test]
fn destroy_is_blocked_while_a_band_has_background_activity() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let engine = seeded_engine(&runner, state_store.clone(), three_disks());
    // A running scrub reports `sync_action == "check"` -- the same
    // non-idle signal a reshape would -- and is far simpler to simulate
    // through the shared `FailingRunner` than a real reshape.
    engine.scrub_start(None).unwrap();

    let err = engine.destroy(None, false).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("background activity"), "{err}");

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(saved.groups.len(), 1, "group must still exist");
}

#[test]
fn destroy_leaves_state_and_configs_unchanged_when_stopping_the_array_fails() {
    // Partial teardown must never drop the bookkeeping: if the array
    // couldn't actually be stopped, state.toml/mdadm.conf/fstab must still
    // list the group -- otherwise a live, un-torn-down array would be
    // forgotten by every record this project keeps, the exact opposite of
    // an orphan this function exists to prevent.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let mdadm_conf = dir.path().join("mdadm.conf");
    let fstab = dir.path().join("fstab");
    let create_runner = FailingRunner::healthy();
    let create_engine = OrchestrationEngine::new(&create_runner, state_store.clone())
        .with_conf_paths(&mdadm_conf, &fstab)
        .with_confirm_sink(&ALWAYS_CONFIRM);
    create_engine.create(create_req(three_disks())).unwrap();

    let runner = FailingRunner {
        fail_forever_trigger: Some("mdadm --stop".to_string()),
        ..FailingRunner::healthy()
    };
    let engine = OrchestrationEngine::new(&runner, state_store.clone())
        .with_conf_paths(&mdadm_conf, &fstab)
        .with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.destroy(None, false).unwrap_err();
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(format!("{err}").contains("UNCHANGED"), "{err}");

    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(
        saved.groups.len(),
        1,
        "group must still be recorded: destroy did not fully succeed"
    );
    let mdadm_conf_after = std::fs::read_to_string(&mdadm_conf).unwrap();
    assert!(
        mdadm_conf_after.contains(&format!("ARRAY /dev/{}", saved.groups[0].bands[0].md_name)),
        "{mdadm_conf_after}"
    );
}

#[test]
fn preview_destroy_never_persists_and_records_the_same_shape_of_commands_as_real_destroy() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = FailingRunner::healthy();
    let _ = seeded_engine(&runner, state_store.clone(), three_disks());
    let mount_point = create_req(three_disks()).mount_point;

    let commands = preview_destroy(state_store.clone(), None, false).unwrap();

    assert!(
        commands.iter().any(|c| c == &format!("umount {mount_point}")),
        "{commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.starts_with("mdadm --stop")),
        "{commands:?}"
    );
    let saved = state_store.load().unwrap().unwrap();
    assert_eq!(saved.groups.len(), 1, "a preview must never persist the destroy");
}

// -- NoActiveArray must not name an operation the caller didn't run --

#[test]
fn no_active_array_message_does_not_name_an_unrelated_operation() {
    // `NoActiveArray` is returned by SEVEN different operations (destroy,
    // expand, recompress, replace_disk, scrub_cancel, scrub_start,
    // scrub_status). The old text ("No array to expand or resume") only
    // ever describes two of them -- an operator running `scrub status` on
    // an empty host got told about "expand or resume", which names
    // commands they never ran. Exercise two operations from opposite ends
    // of that list (scrub_status, destroy) against a completely unseeded
    // store and check the resulting text doesn't claim either is the other.
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let runner = DryRunRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let scrub_err = engine.scrub_status(None).unwrap_err().to_string();
    let destroy_err = engine.destroy(None, false).unwrap_err().to_string();

    for (op, msg) in [("scrub_status", &scrub_err), ("destroy", &destroy_err)] {
        let lower = msg.to_lowercase();
        assert!(
            !lower.contains("expand"),
            "{op} error names 'expand', which it isn't: {msg}"
        );
        assert!(
            !lower.contains("resume"),
            "{op} error names 'resume', which it isn't: {msg}"
        );
    }

    // Both callers hit the exact same precondition (no group recorded in
    // state.toml yet) -- the text they get back must be identical, not two
    // different operation-specific misnomers.
    assert_eq!(
        scrub_err, destroy_err,
        "same precondition must produce the same message"
    );
}

// -- Tick_active_reshapes must not let one band's absent array abort
// the whole sweep --

/// Standalone `CommandRunner` for these tests, independent of the shared
/// `FailingRunner` above (whose `sync_action_response` is a single GLOBAL
/// answer shared by every band -- it cannot make two different bands'
/// `sync_action` reads answer differently, which is exactly what this
/// defect needs). `missing_md`'s `sync_action` read fails with the real-
/// guest ENOENT signature (`cat: .../sync_action: No such file or
/// directory`, exit 1); every other band's `sync_action` read reports
/// `reshape`; every other command (speed_limit_max reads/writes, the live
/// metrics sampler's smartctl/proc reads) is a harmless no-op success --
/// this test cares about which bands get ticked, not the resulting
/// throttle decision.
struct SyncActionEnoentRunner {
    missing_md: String,
    recorded: Mutex<Vec<String>>,
}

impl CommandRunner for SyncActionEnoentRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let cmd = format!("{program} {}", args.join(" "));
        self.recorded.lock().unwrap().push(cmd.clone());

        if program == "cat" && cmd.contains(&format!("/sys/block/{}/md/sync_action", self.missing_md)) {
            return Err(ExecError::NonZeroExit {
                program: "cat".to_string(),
                exit_code: 1,
                stdout: String::new(),
                stderr: format!(
                    "cat: /sys/block/{}/md/sync_action: No such file or directory",
                    self.missing_md
                ),
            });
        }
        if program == "cat" && cmd.contains("/md/sync_action") {
            return Ok(CommandOutput {
                stdout: "reshape\n".to_string(),
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

impl SyncActionEnoentRunner {
    fn new(missing_md: &str) -> Self {
        Self {
            missing_md: missing_md.to_string(),
            recorded: Mutex::new(Vec::new()),
        }
    }

    fn get_recorded(&self) -> Vec<String> {
        self.recorded.lock().unwrap().clone()
    }
}

/// A `CommandRunner` that fails EVERY `sync_action` read (any band) with a
/// genuine, non-ENOENT shape (permission denied) -- used to prove the
/// fix's guard is precise, not a blanket swallow of every `sync_action`
/// error.
struct PermissionDeniedRunner {
    recorded: Mutex<Vec<String>>,
}

impl CommandRunner for PermissionDeniedRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let cmd = format!("{program} {}", args.join(" "));
        self.recorded.lock().unwrap().push(cmd.clone());

        if program == "cat" && cmd.contains("/md/sync_action") {
            return Err(ExecError::NonZeroExit {
                program: "cat".to_string(),
                exit_code: 1,
                stdout: String::new(),
                stderr: "cat: /sys/block/md0/md/sync_action: Permission denied".to_string(),
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

impl PermissionDeniedRunner {
    fn new() -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
        }
    }
}

/// A `CommandRunner` whose `sync_action` read fails with ENOENT-shaped
/// STDERR TEXT but a DIFFERENT `program` field than `cat` -- there is no
/// real code path that produces this today (`sync_action` only ever runs
/// `cat`), but it proves the fix's guard checks `program == "cat"` and not
/// stderr text alone, so a future command added to `sync_action`'s read
/// path that happens to fail with a similarly-worded message is not
/// silently swallowed by this arm.
struct NonCatEnoentRunner {
    recorded: Mutex<Vec<String>>,
}

impl CommandRunner for NonCatEnoentRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let cmd = format!("{program} {}", args.join(" "));
        self.recorded.lock().unwrap().push(cmd.clone());

        if program == "cat" && cmd.contains("/md/sync_action") {
            return Err(ExecError::NonZeroExit {
                program: "not-cat".to_string(),
                exit_code: 1,
                stdout: String::new(),
                stderr: "not-cat: /sys/block/md0/md/sync_action: No such file or directory".to_string(),
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

impl NonCatEnoentRunner {
    fn new() -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
        }
    }
}

/// Precision guard, part 2: ENOENT-shaped stderr text ALONE is not
/// enough to skip a band -- the `program` must also be `cat` (NonCat
/// EnoentRunner returns `program: "not-cat"` despite the caller having
/// asked to run `cat`, standing in for a future non-`cat` command in this
/// read path). A guard gated on stderr text only would skip this band too,
/// silently -- exactly the "no `program` check" gap this test pins.
#[test]
fn enoent_from_a_different_program_still_aborts_the_tick() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();

    let runner = NonCatEnoentRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.tick_active_reshapes().unwrap_err();
    assert!(
        format!("{err}").contains("No such file or directory"),
        "an ENOENT-shaped message from a non-cat program must still be reported, not silently skipped: {err}"
    );
}

/// Real-guest repro: `state.toml` has two groups, shr1's `md0` was
/// never assembled (reboot came back without its members, or an operator
/// ran `mdadm --stop` by hand), shr2's `md1` is genuinely mid-reshape. The
/// old code's bare `mdadm.sync_action(&md_name)?` let band 0's ENOENT
/// abort the ENTIRE tick via `?` -- band 1's throttle decision, the one
/// thing `shr-rs-throttle-tick.timer` exists to apply, never ran, 9 times
/// in the guest's real journal. Proves both halves of the fix: band 1 is
/// still ticked (a real, actionable reshape did not silently lose its
/// throttle), and `ticked` reports the true count (1, not 0 and not an
/// error) -- the number `shr-rs internal reshape-throttle-tick` prints.
#[test]
fn a_missing_arrays_band_is_skipped_and_a_later_reshaping_bands_band_is_still_ticked() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();
    setup_engine
        .create(create_req_named("shr2", other_two_disks()))
        .unwrap();

    let runner = SyncActionEnoentRunner::new("md0");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let ticked = engine
        .tick_active_reshapes()
        .expect("a missing array on one band must not abort the whole throttle tick");

    assert_eq!(
        ticked, 1,
        "band 0 (no live array) must be skipped, band 1 (genuinely reshaping) must still be ticked"
    );
    let cmds = runner.get_recorded();
    assert!(
        cmds.iter().any(|c| c == "cat /sys/block/md0/md/sync_action"),
        "band 0 must still be visited (its ENOENT is caught, not skipped over entirely): {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c == "cat /sys/block/md1/md/sync_action"),
        "band 1 must be reached -- proves the sweep did not die at band 0: {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("sh -c") && c.contains("speed_limit_max")),
        "band 1's throttle decision must actually reach the kernel parameter: {cmds:?}"
    );
}

/// Precision guard: a `sync_action` read failing for a reason OTHER
/// than "array not assembled" (measured here as `Permission denied`, a
/// stand-in for any non-ENOENT read failure) must NOT be swallowed the same
/// way -- the fix's guard is gated on the exact ENOENT text, not "any
/// error reading sync_action means skip". A fix that blanket-swallowed
/// every error would turn a genuinely actionable failure (something is
/// wrong reading a LIVE array's state) into permanent, silent 0-band ticks
/// -- a worse defect than the one being fixed.
#[test]
fn a_non_enoent_sync_action_failure_still_aborts_the_tick_instead_of_being_swallowed() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();

    let runner = PermissionDeniedRunner::new();
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.tick_active_reshapes().unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("permission denied"),
        "a genuine, non-ENOENT sync_action failure must still be reported, not silently skipped: {err}"
    );
}

// ---------------------------------------------------------------------
// Scrub_start's degraded_count/sync_action guards must name the
// array as "not assembled" instead of leaking `cat`'s raw ENOENT text.
// Measured on the real guest: `mdadm --stop /dev/md0` (state.toml intact),
// then the weekly `fs scrub start --name <group>` timer failed as
// `error: Execution error: Command 'cat' failed with exit code 1: cat:
// /sys/block/md0/md/degraded: No such file or directory`.
// ---------------------------------------------------------------------

/// Its own tiny `CommandRunner`, deliberately NOT another field on the
/// shared `FailingRunner` -- that struct is being edited concurrently by
/// the and earlier fixes right now, so this keeps the tests
/// independent of that churn. Only ever used for the `scrub_start` call
/// under test; group setup goes through a plain `FailingRunner::healthy()`
/// first (the same "separate setup vs. under-test runner" split
/// `scrub_cancel_writes_idle_and_clears_scrub_in_progress_even_when_degraded`
/// above and the ENOENT runners below already use), so this only needs to answer
/// the small set of commands `scrub_start` itself issues.
struct AbsentArrayRunner {
    recorded: Mutex<Vec<String>>,
    md_name: String,
    /// `/sys/block/<md_name>/md/<leaf>` ENOENTs -- "degraded" or
    /// "sync_action". `None` selects neither (the two residual-shape flags
    /// below apply instead).
    enoent_leaf: Option<&'static str>,
    /// Degraded read SUCCEEDS (array present) but stdout doesn't parse --
    /// `degraded_count`'s own `Prerequisite` error, structurally distinct
    /// from ENOENT and must NOT be relabeled "not assembled".
    degraded_unparseable: bool,
    /// sync_action read FAILS, but with a genuine non-ENOENT shape
    /// (permission denied) -- must still propagate unchanged, not be
    /// swallowed into "not assembled".
    sync_action_permission_denied: bool,
}

impl AbsentArrayRunner {
    fn enoent(md_name: &str, leaf: &'static str) -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
            md_name: md_name.to_string(),
            enoent_leaf: Some(leaf),
            degraded_unparseable: false,
            sync_action_permission_denied: false,
        }
    }

    fn degraded_unparseable(md_name: &str) -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
            md_name: md_name.to_string(),
            enoent_leaf: None,
            degraded_unparseable: true,
            sync_action_permission_denied: false,
        }
    }

    fn sync_action_permission_denied(md_name: &str) -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
            md_name: md_name.to_string(),
            enoent_leaf: None,
            degraded_unparseable: false,
            sync_action_permission_denied: true,
        }
    }

    fn get_recorded(&self) -> Vec<String> {
        self.recorded.lock().unwrap().clone()
    }
}

impl CommandRunner for AbsentArrayRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        let cmd = format!("{program} {}", args.join(" "));
        self.recorded.lock().unwrap().push(cmd);

        let degraded_path = format!("/sys/block/{}/md/degraded", self.md_name);
        let sync_action_path = format!("/sys/block/{}/md/sync_action", self.md_name);

        if program == "cat" && args.contains(&degraded_path.as_str()) {
            if self.enoent_leaf == Some("degraded") {
                return Err(ExecError::NonZeroExit {
                    program: "cat".to_string(),
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("cat: {degraded_path}: No such file or directory\n"),
                });
            }
            if self.degraded_unparseable {
                return Ok(CommandOutput {
                    stdout: "not-a-number\n".to_string(),
                    stderr: String::new(),
                });
            }
            return Ok(CommandOutput {
                stdout: "0\n".to_string(),
                stderr: String::new(),
            });
        }
        if program == "cat" && args.contains(&sync_action_path.as_str()) {
            if self.enoent_leaf == Some("sync_action") {
                return Err(ExecError::NonZeroExit {
                    program: "cat".to_string(),
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("cat: {sync_action_path}: No such file or directory\n"),
                });
            }
            if self.sync_action_permission_denied {
                return Err(ExecError::NonZeroExit {
                    program: "cat".to_string(),
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: format!("cat: {sync_action_path}: Permission denied\n"),
                });
            }
            return Ok(CommandOutput {
                stdout: "idle\n".to_string(),
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

/// RED-worthy value test: the degraded guard's `cat .../md/degraded`
/// ENOENTing (array not assembled) must produce a message naming that
/// condition -- not the raw `cat: ... No such file or directory` the old
/// unconditional `?` let straight through.
#[test]
fn scrub_start_degraded_read_enoent_names_array_not_assembled() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();

    let runner = AbsentArrayRunner::enoent("md0", "degraded");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.scrub_start(None).unwrap_err();
    let msg = format!("{err}");
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(
        msg.contains("not assembled"),
        "must name the actual condition: {msg}"
    );
    assert!(msg.contains("md0"), "must name the md device: {msg}");
    assert!(msg.contains('0'), "must name the band index: {msg}");
    assert!(
        !msg.contains("cat:"),
        "must not leak the raw `cat` plumbing: {msg}"
    );
    assert!(
        !msg.contains("No such file or directory"),
        "must not leak raw ENOENT text: {msg}"
    );
}

/// Same value test, for the second abort point (the `sync_action` guard)
/// -- this project's recurring "the same guard exists on one path and not
/// its sibling" defect face, so it needs its own regression pin, not just
/// coverage-by-association with the degraded guard above.
#[test]
fn scrub_start_sync_action_read_enoent_names_array_not_assembled() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();

    let runner = AbsentArrayRunner::enoent("md0", "sync_action");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.scrub_start(None).unwrap_err();
    let msg = format!("{err}");
    assert!(matches!(err, OrchestrateError::Validation(_)), "{err:?}");
    assert!(
        msg.contains("not assembled"),
        "must name the actual condition: {msg}"
    );
    assert!(msg.contains("md0"), "must name the md device: {msg}");
    assert!(
        !msg.contains("cat:"),
        "must not leak the raw `cat` plumbing: {msg}"
    );
    assert!(
        !msg.contains("No such file or directory"),
        "must not leak raw ENOENT text: {msg}"
    );
}

/// Safety property: an absent array must produce ONLY the clear error --
/// never a scrub write, never `btrfs scrub start`, never a `state.toml`
/// save claiming `scrub_in_progress`. Proves this isn't "fixed" by quietly
/// tolerating the missing array and skipping the band -- that would report
/// success while verifying nothing and record a scrub that never ran.
#[test]
fn scrub_start_absent_array_never_issues_scrub_write_or_saves_state() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.toml");
    let state_store = Arc::new(StateStore::new(state_path.clone()));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();
    let before_toml = std::fs::read_to_string(&state_path).unwrap();

    let runner = AbsentArrayRunner::enoent("md0", "degraded");
    let engine = OrchestrationEngine::new(&runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);

    engine.scrub_start(None).unwrap_err();

    let cmds = runner.get_recorded();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("sync_action") && c.contains("check")),
        "must never issue the scrub write once the array is found absent: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.starts_with("btrfs scrub start")),
        "must never start the btrfs half of the scrub either: {cmds:?}"
    );

    let after_toml = std::fs::read_to_string(&state_path).unwrap();
    assert_eq!(
        before_toml, after_toml,
        "state.toml must not be touched on an aborted scrub_start"
    );
    let saved = state_store.load().unwrap().unwrap();
    assert!(
        saved.groups[0].bands.iter().all(|b| !b.scrub_in_progress),
        "must not record a scrub that never ran"
    );
}

/// Distinguishing test (degraded side): `degraded_count`'s OTHER failure
/// mode -- `cat` SUCCEEDS (array present) but stdout doesn't parse -- must
/// keep propagating as-is, not get relabeled "array not assembled". A
/// blanket `Err(_) =>` match here would make this false statement about
/// the machine; the fix must gate on the `cat` + ENOENT shape specifically.
#[test]
fn degraded_unparseable_is_not_reported_as_array_not_assembled() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();

    let runner = AbsentArrayRunner::degraded_unparseable("md0");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.scrub_start(None).unwrap_err();
    let msg = format!("{err}");
    assert!(
        !msg.contains("not assembled"),
        "a parse failure on an assembled array must not be reported as absent: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("degraded count") || msg.to_lowercase().contains("parse"),
        "{msg}"
    );
}

/// Distinguishing test (sync_action side): a genuine non-ENOENT
/// `sync_action` read failure (permission denied) must still propagate,
/// not be swallowed into "not assembled" -- the twin of the guard
/// above (`a_non_enoent_sync_action_failure_still_aborts_the_tick_
/// instead_of_being_swallowed`), applied to `scrub_start` instead of
/// `tick_active_reshapes`.
#[test]
fn sync_action_permission_denied_is_not_reported_as_array_not_assembled() {
    let dir = tempdir().unwrap();
    let state_store = Arc::new(StateStore::new(dir.path().join("state.toml")));
    let setup_runner = FailingRunner::healthy();
    let setup_engine =
        OrchestrationEngine::new(&setup_runner, state_store.clone()).with_confirm_sink(&ALWAYS_CONFIRM);
    setup_engine
        .create(create_req_named("shr1", three_disks()))
        .unwrap();

    let runner = AbsentArrayRunner::sync_action_permission_denied("md0");
    let engine = OrchestrationEngine::new(&runner, state_store).with_confirm_sink(&ALWAYS_CONFIRM);

    let err = engine.scrub_start(None).unwrap_err();
    let msg = format!("{err}");
    assert!(
        !msg.contains("not assembled"),
        "a genuine non-ENOENT read failure must not be reported as absent: {msg}"
    );
    assert!(msg.to_lowercase().contains("permission denied"), "{msg}");
}
