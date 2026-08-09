use shr_exec::{
    BtrfsExecutor, CommandOutput, CommandRunner, DryRunRunner, ExecError, LvmExecutor, MdadmExecutor,
    PartedExecutor,
};
use std::sync::Mutex;

/// Records commands like `DryRunRunner`, but reports `is_dry_run() == false`
/// and always succeeds. Used to exercise the REAL-execution branch of
/// `ensure_supported()`/rollback primitives (which must actually issue a
/// command, unlike their dry-run no-op branches) without needing a real
/// Linux host -- these tests only assert on which commands get recorded,
/// never on real system state.
#[derive(Default)]
struct NonDryRunRunner {
    recorded: Mutex<Vec<String>>,
}

impl NonDryRunRunner {
    fn get_recorded(&self) -> Vec<String> {
        self.recorded.lock().unwrap().clone()
    }
}

impl CommandRunner for NonDryRunRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
        self.recorded
            .lock()
            .unwrap()
            .push(format!("{} {}", program, args.join(" ")));
        // ensure_supported()'s btrfs check parses this for "btrfs" support;
        // degraded_count() parses the sysfs "degraded" file as an integer.
        let stdout = if program == "cat" && args.contains(&"/proc/filesystems") {
            "nodev\tsysfs\nbtrfs\n".to_string()
        } else if program == "cat" && args.iter().any(|a| a.ends_with("/md/degraded")) {
            "0\n".to_string()
        } else if program == "cat" && args.iter().any(|a| a.ends_with("/md/sync_action")) {
            "reshape\n".to_string()
        } else if program == "cat" && args.iter().any(|a| a.ends_with("/md/mismatch_cnt")) {
            "3\n".to_string()
        } else if program == "mdadm" && args.contains(&"--export") {
            "MD_LEVEL=raid5\nMD_DEVICES=3\nMD_METADATA=1.2\nMD_UUID=12345678:abcdef01:23456789:0abcdef1\n"
                .to_string()
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

/// What a single `ScriptedRunner::run` call should do, in call order.
/// Models the transient-failure-then-recovery shape of the real-guest bug
/// (`blkid` exit 2 under I/O load, succeeding once the settle race clears)
/// without needing a real Linux host: the last scripted outcome repeats for
/// any call past the end of the list, so "always fails"/"always empty" can
/// be expressed with a single entry.
enum ScriptedOutcome {
    /// A command that fails outright, like `blkid` exiting 2 ("nothing found").
    Fail,
    /// A command that exits 0 but prints nothing -- the empty-output hole.
    Empty,
    /// A command that succeeds with a real value. For `mdadm`, wrapped as an
    /// `MD_UUID=` export line; for everything else, used as raw stdout.
    Value(&'static str),
}

/// Test double for the retry paths added to `read_partuuid`/`read_uuid`:
/// returns a caller-scripted outcome based on call count, and counts calls
/// so tests can assert exactly how many attempts a retry made.
struct ScriptedRunner {
    calls: Mutex<u32>,
    outcomes: Vec<ScriptedOutcome>,
}

impl ScriptedRunner {
    fn new(outcomes: Vec<ScriptedOutcome>) -> Self {
        Self {
            calls: Mutex::new(0),
            outcomes,
        }
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(&self, program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let idx = ((*calls - 1) as usize).min(self.outcomes.len() - 1);
        match &self.outcomes[idx] {
            ScriptedOutcome::Fail => Err(ExecError::NonZeroExit {
                program: program.to_string(),
                exit_code: 2,
                stdout: String::new(),
                stderr: String::new(),
            }),
            ScriptedOutcome::Empty => Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            }),
            ScriptedOutcome::Value(v) => {
                let stdout = if program == "mdadm" {
                    format!("MD_UUID={v}\n")
                } else {
                    format!("{v}\n")
                };
                Ok(CommandOutput {
                    stdout,
                    stderr: String::new(),
                })
            }
        }
    }

    fn is_dry_run(&self) -> bool {
        false
    }
}

#[test]
fn read_partuuid_retries_past_transient_blkid_failures_then_succeeds() {
    // Reproduces the real-guest bug: blkid exits 2 ("nothing found") on the
    // first two attempts (settle race under I/O load from other resyncing
    // arrays), then succeeds once udev catches up -- the caller must see
    // the real value, not an error.
    let runner = ScriptedRunner::new(vec![
        ScriptedOutcome::Fail,
        ScriptedOutcome::Fail,
        ScriptedOutcome::Value("11111111-2222-4333-8444-555555555555"),
    ]);
    let parted = PartedExecutor::new(&runner);

    let uuid = parted.read_partuuid("/dev/loop10p1").unwrap();

    assert_eq!(uuid, "11111111-2222-4333-8444-555555555555");
    assert_eq!(runner.call_count(), 3);
}

#[test]
fn read_partuuid_surfaces_a_clear_retried_error_when_blkid_always_fails() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Fail]);
    let parted = PartedExecutor::new(&runner);

    let err = parted.read_partuuid("/dev/loop10p1").unwrap_err().to_string();

    assert!(
        runner.call_count() > 1,
        "must retry, not fail on the first attempt: {}",
        runner.call_count()
    );
    assert!(err.contains("/dev/loop10p1"), "{err}");
    assert!(
        err.to_lowercase().contains("retr"),
        "error should make the retry evident: {err}"
    );
}

#[test]
fn read_partuuid_treats_empty_output_as_not_ready_and_never_returns_ok_empty() {
    // blkid can exit 0 with no output; that must never be mistaken for a
    // real (empty) PARTUUID -- it would flow into
    // /dev/disk/by-partuuid/{uuid} member paths and state.toml untested.
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Empty]);
    let parted = PartedExecutor::new(&runner);

    let result = parted.read_partuuid("/dev/loop10p1");

    assert!(
        result.is_err(),
        "empty blkid output must be an error, got {result:?}"
    );
}

#[test]
fn read_partuuid_dry_run_does_not_retry_or_touch_the_runner() {
    let runner = DryRunRunner::new();
    let parted = PartedExecutor::new(&runner);

    parted.read_partuuid("/dev/loop10p1").unwrap();

    assert!(
        runner.get_recorded().is_empty(),
        "dry-run must return its simulated value without ever calling the runner: {:?}",
        runner.get_recorded()
    );
}

#[test]
fn btrfs_read_uuid_retries_past_transient_blkid_failures_then_succeeds() {
    let runner = ScriptedRunner::new(vec![
        ScriptedOutcome::Fail,
        ScriptedOutcome::Value("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"),
    ]);
    let btrfs = BtrfsExecutor::new(&runner);

    let uuid = btrfs.read_uuid("/dev/shr_vg/data").unwrap();

    assert_eq!(uuid, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
    assert_eq!(runner.call_count(), 2);
}

#[test]
fn btrfs_read_uuid_surfaces_a_clear_retried_error_when_blkid_always_fails() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Fail]);
    let btrfs = BtrfsExecutor::new(&runner);

    let err = btrfs.read_uuid("/dev/shr_vg/data").unwrap_err().to_string();

    assert!(runner.call_count() > 1, "must retry: {}", runner.call_count());
    assert!(
        err.to_lowercase().contains("retr"),
        "error should make the retry evident: {err}"
    );
}

#[test]
fn btrfs_read_uuid_treats_empty_output_as_not_ready_and_never_returns_ok_empty() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Empty]);
    let btrfs = BtrfsExecutor::new(&runner);

    let result = btrfs.read_uuid("/dev/shr_vg/data");

    assert!(
        result.is_err(),
        "empty blkid output must be an error, got {result:?}"
    );
}

#[test]
fn btrfs_read_uuid_dry_run_does_not_retry_or_touch_the_runner() {
    let runner = DryRunRunner::new();
    let btrfs = BtrfsExecutor::new(&runner);

    btrfs.read_uuid("/dev/shr_vg/data").unwrap();

    assert!(
        runner.get_recorded().is_empty(),
        "dry-run must never call the runner: {:?}",
        runner.get_recorded()
    );
}

#[test]
fn mdadm_read_uuid_retries_past_transient_failures_then_succeeds() {
    // Same class of race as blkid, but for mdadm --detail --export right
    // after create_array: the array exists but its detail isn't fully
    // queryable yet under I/O load.
    let runner = ScriptedRunner::new(vec![
        ScriptedOutcome::Fail,
        ScriptedOutcome::Fail,
        ScriptedOutcome::Value("12345678:abcdef01:23456789:0abcdef1"),
    ]);
    let mdadm = MdadmExecutor::new(&runner);

    let uuid = mdadm.read_uuid("md0").unwrap();

    assert_eq!(uuid, "12345678:abcdef01:23456789:0abcdef1");
    assert_eq!(runner.call_count(), 3);
}

#[test]
fn mdadm_read_uuid_surfaces_a_clear_retried_error_when_always_failing() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Fail]);
    let mdadm = MdadmExecutor::new(&runner);

    let err = mdadm.read_uuid("md0").unwrap_err().to_string();

    assert!(runner.call_count() > 1, "must retry: {}", runner.call_count());
    assert!(err.contains("md0") || err.contains("/dev/md0"), "{err}");
    assert!(
        err.to_lowercase().contains("retr"),
        "error should make the retry evident: {err}"
    );
}

#[test]
fn mdadm_read_uuid_treats_missing_md_uuid_line_as_not_ready_and_never_returns_ok_empty() {
    // `Empty` here means "mdadm exits 0 but the export blob has no MD_UUID=
    // line at all" (ScriptedRunner only special-cases stdout for the
    // `Value` outcome) -- parse_export_field returns None, which the retry
    // helper must treat the same as an empty string: not ready yet, then a
    // clear error, never Ok("").
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Empty]);
    let mdadm = MdadmExecutor::new(&runner);

    let result = mdadm.read_uuid("md0");

    assert!(
        result.is_err(),
        "missing MD_UUID must be an error, got {result:?}"
    );
}

#[test]
fn mdadm_read_uuid_dry_run_does_not_retry_or_touch_the_runner() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);

    mdadm.read_uuid("md0").unwrap();

    assert!(
        runner.get_recorded().is_empty(),
        "dry-run must never call the runner: {:?}",
        runner.get_recorded()
    );
}

#[test]
fn dry_run_parted_records_commands() {
    let runner = DryRunRunner::new();
    let parted = PartedExecutor::new(&runner);

    parted.create_gpt("/dev/disk/by-id/ata-TEST1").unwrap();
    parted
        .add_partition("/dev/disk/by-id/ata-TEST1", 134217728, 4000000000000)
        .unwrap();

    let cmds = runner.get_recorded();
    assert_eq!(cmds.len(), 2);
    assert_eq!(cmds[0], "parted -s /dev/disk/by-id/ata-TEST1 mklabel gpt");
    assert_eq!(
        cmds[1],
        "parted -s /dev/disk/by-id/ata-TEST1 mkpart primary 134217728B 4000000000000B"
    );
}

#[test]
fn dry_run_partuuid_is_stable_but_obviously_not_a_real_uuid() {
    // Must NOT be shaped like a real UUID -- a preview's `mdadm
    // --create .../by-partuuid/<value>` line must not look like a command
    // that will run byte-for-byte when the real PARTUUID is fundamentally
    // unknowable before the partition actually exists.
    let runner = DryRunRunner::new();
    let parted = PartedExecutor::new(&runner);

    let first = parted.read_partuuid("/dev/loop10p1").unwrap();
    let second = parted.read_partuuid("/dev/loop10p1").unwrap();

    assert_eq!(
        first, second,
        "must stay stable across repeated reads within one preview"
    );
    assert!(!uuid_like(&first), "{first} must not look like a real PARTUUID");
    assert!(first.starts_with("pending-"), "{first}");
}

#[test]
fn dry_run_partition_path_for_read_matches_plain_heuristic_without_touching_filesystem() {
    // Found running Step 3's real-VM smoke test: on real execution this
    // method canonicalizes disk_path via the filesystem (a synthetic by-id
    // symlink like a smoke-test fixture's `ata-LOOP_DISK_10` has no matching
    // `-part1` by-id symlink -- only real udev-managed by-id names do, and
    // even those are populated asynchronously). Dry-run must never touch
    // the filesystem, so for a disk_path that doesn't exist anywhere (as is
    // always true on the Windows dev host), this must fall back to the
    // plain string heuristic instead of erroring.
    let runner = DryRunRunner::new();
    let parted = PartedExecutor::new(&runner);

    let path = parted.partition_path_for_read("/dev/disk/by-id/ata-TEST1", 2);
    assert_eq!(path, shr_exec::partition_dev_path("/dev/disk/by-id/ata-TEST1", 2));
}

#[test]
fn dry_run_settle_udev_records_no_command() {
    let runner = DryRunRunner::new();
    let parted = PartedExecutor::new(&runner);

    parted.settle_udev().unwrap();

    assert!(runner.get_recorded().is_empty());
}

#[test]
fn dry_run_ensure_supported_is_a_noop_for_all_four_executors() {
    let runner = DryRunRunner::new();
    PartedExecutor::new(&runner).ensure_supported().unwrap();
    MdadmExecutor::new(&runner).ensure_supported().unwrap();
    LvmExecutor::new(&runner).ensure_supported().unwrap();
    BtrfsExecutor::new(&runner).ensure_supported().unwrap();

    assert!(
        runner.get_recorded().is_empty(),
        "dry-run prerequisite checks must never touch the system: {:?}",
        runner.get_recorded()
    );
}

#[test]
fn real_ensure_supported_checks_each_tool_version() {
    let runner = NonDryRunRunner::default();
    PartedExecutor::new(&runner).ensure_supported().unwrap();
    MdadmExecutor::new(&runner).ensure_supported().unwrap();
    LvmExecutor::new(&runner).ensure_supported().unwrap();
    BtrfsExecutor::new(&runner).ensure_supported().unwrap();

    let cmds = runner.get_recorded();
    assert!(cmds.contains(&"parted --version".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"mdadm --version".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"pvcreate --version".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"cat /proc/filesystems".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"mkfs.btrfs --version".to_string()), "{cmds:?}");
}

#[test]
fn real_ensure_supported_rejects_kernel_without_btrfs_before_running_mkfs_btrfs() {
    // D11: reading /proc/filesystems through the SAME CommandRunner
    // abstraction as everything else (not a raw std::fs call) is what
    // makes this prerequisite testable without a real Linux host -- and
    // what let Step 4's engine-level tests inject "btrfs unsupported"
    // precisely, the exact scenario D11 exists to catch before any
    // partitioning happens.
    struct NoBtrfsRunner;
    impl CommandRunner for NoBtrfsRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
            let stdout = if program == "cat" && args.contains(&"/proc/filesystems") {
                "nodev\tsysfs\next4\n".to_string() // no btrfs line
            } else {
                panic!("must not run `{program}` after the kernel support check fails");
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

    let err = BtrfsExecutor::new(&NoBtrfsRunner).ensure_supported().unwrap_err();
    assert!(
        matches!(err, ExecError::Prerequisite(_)),
        "expected Prerequisite, got {err:?}"
    );
}

#[test]
fn dry_run_remove_partition_records_command() {
    // Like create_gpt/add_partition, this is an ordinary action method with
    // no internal dry-run branch -- DryRunRunner records instead of
    // executing, same as every other forward-path action. (The engine's
    // rollback() itself is what skips ALL undo actions under dry-run, at a
    // higher level, so this method doesn't need its own guard.)
    let runner = DryRunRunner::new();
    let parted = PartedExecutor::new(&runner);
    parted.remove_partition("/dev/disk/by-id/ata-TEST1", 2).unwrap();
    assert_eq!(
        runner.get_recorded(),
        vec!["parted -s /dev/disk/by-id/ata-TEST1 rm 2"]
    );
}

#[test]
fn dry_run_unmount_records_command() {
    let runner = DryRunRunner::new();
    BtrfsExecutor::new(&runner).unmount("/mnt/data").unwrap();
    assert_eq!(runner.get_recorded(), vec!["umount /mnt/data"]);
}

fn uuid_like(value: &str) -> bool {
    value.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| value.as_bytes()[index] == b'-')
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

#[test]
fn dry_run_mdadm_read_uuid_is_stable_and_four_hex_groups() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);

    let first = mdadm.read_uuid("md0").unwrap();
    let second = mdadm.read_uuid("md0").unwrap();

    assert_eq!(first, second);
    assert!(md_uuid_like(&first), "not MD_UUID-shaped: {first}");
}

#[test]
fn dry_run_mdadm_read_uuid_differs_for_different_arrays() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);

    let md0 = mdadm.read_uuid("md0").unwrap();
    let md1 = mdadm.read_uuid("md1").unwrap();

    assert_ne!(md0, md1);
}

fn md_uuid_like(value: &str) -> bool {
    let groups: Vec<&str> = value.split(':').collect();
    groups.len() == 4
        && groups.iter().all(|g| {
            g.len() == 8
                && g.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}

#[test]
fn dry_run_btrfs_read_uuid_is_stable_and_uuid_shaped() {
    let runner = DryRunRunner::new();
    let btrfs = BtrfsExecutor::new(&runner);

    let first = btrfs.read_uuid("/dev/shr_vg/data").unwrap();
    let second = btrfs.read_uuid("/dev/shr_vg/data").unwrap();

    assert_eq!(first, second);
    assert!(uuid_like(&first), "not uuid-shaped: {first}");
}

#[test]
fn dry_run_btrfs_read_uuid_differs_for_different_devices() {
    let runner = DryRunRunner::new();
    let btrfs = BtrfsExecutor::new(&runner);

    let a = btrfs.read_uuid("/dev/shr_vg/data").unwrap();
    let b = btrfs.read_uuid("/dev/shr_vg/data2").unwrap();

    assert_ne!(a, b);
}

#[test]
fn dry_run_mdadm_records_create() {
    // D6: --metadata=1.2 must be explicit (the design's confirmed value),
    // not left to mdadm's own default (which has changed across versions).
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);

    mdadm
        .create_array("md0", "raid5", &["/dev/sdb1", "/dev/sdc1", "/dev/sdd1"])
        .unwrap();

    let cmds = runner.get_recorded();
    assert_eq!(cmds.len(), 1);
    assert_eq!(
        cmds[0],
        "mdadm --create /dev/md0 --level=raid5 --raid-devices=3 --metadata=1.2 --bitmap=internal --spare-devices=0 --run /dev/sdb1 /dev/sdc1 /dev/sdd1"
    );
}

#[test]
fn dry_run_mdadm_create_always_includes_an_internal_write_intent_bitmap() {
    // without a bitmap, any unclean shutdown or degraded-member
    // recovery forces a full resync instead of only the changed regions.
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);

    mdadm
        .create_array(
            "md1",
            "raid6",
            &["/dev/sdb2", "/dev/sdc2", "/dev/sdd2", "/dev/sde2"],
        )
        .unwrap();

    let cmds = runner.get_recorded();
    assert!(cmds[0].contains("--bitmap=internal"), "{}", cmds[0]);
}

#[test]
fn dry_run_mdadm_grow_always_includes_backup_file() {
    // D6: every grow (device-count change and/or level change) must carry
    // --backup-file (the design) -- reshape crash safety.
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);

    mdadm
        .grow("md0", None, 4, "/var/lib/shr-rs/backup-md0.bak")
        .unwrap();
    mdadm
        .grow("md0", Some("raid5"), 3, "/var/lib/shr-rs/backup-md0.bak")
        .unwrap();

    let cmds = runner.get_recorded();
    assert_eq!(
        cmds[0],
        "mdadm --grow /dev/md0 --raid-devices=4 --backup-file=/var/lib/shr-rs/backup-md0.bak"
    );
    assert_eq!(
        cmds[1],
        "mdadm --grow /dev/md0 --level=raid5 --raid-devices=3 --backup-file=/var/lib/shr-rs/backup-md0.bak"
    );
}

#[test]
fn dry_run_mdadm_remove_member_issues_remove() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    mdadm.remove_member("md0", "/dev/loop14p1").unwrap();
    assert_eq!(
        runner.get_recorded(),
        vec!["mdadm --remove /dev/md0 /dev/loop14p1"]
    );
}

#[test]
fn dry_run_degraded_count_is_zero_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    assert_eq!(mdadm.degraded_count("md0").unwrap(), 0);
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_degraded_count_reads_sysfs_via_the_runner() {
    // Goes through `cat` (the runner), not a raw std::fs read -- same
    // rationale as BtrfsExecutor::ensure_supported's /proc/filesystems fix:
    // mockable by a test double, and doesn't unconditionally IO-error on
    // this project's native Windows test host.
    let runner = NonDryRunRunner::default();
    let mdadm = MdadmExecutor::new(&runner);
    mdadm.degraded_count("md0").unwrap();
    assert!(
        runner
            .get_recorded()
            .contains(&"cat /sys/block/md0/md/degraded".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn dry_run_sync_action_is_idle_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    assert_eq!(mdadm.sync_action("md0").unwrap(), "idle");
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_sync_action_reads_sysfs_via_the_runner() {
    let runner = NonDryRunRunner::default();
    let mdadm = MdadmExecutor::new(&runner);
    assert_eq!(mdadm.sync_action("md0").unwrap(), "reshape");
    assert!(
        runner
            .get_recorded()
            .contains(&"cat /sys/block/md0/md/sync_action".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn dry_run_mdadm_replace_member_records_the_expected_command() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    mdadm
        .replace_member("md0", "/dev/disk/by-partuuid/OLD", "/dev/disk/by-partuuid/NEW")
        .unwrap();
    assert_eq!(
        runner.get_recorded(),
        vec!["mdadm /dev/md0 --replace /dev/disk/by-partuuid/OLD --with /dev/disk/by-partuuid/NEW"]
    );
}

#[test]
fn dry_run_mdadm_scrub_start_and_cancel_touch_nothing() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    mdadm.scrub_start("md0").unwrap();
    mdadm.scrub_cancel("md0").unwrap();
    let recorded = runner.get_recorded();
    assert_eq!(
        recorded,
        vec![
            "sh -c echo check > /sys/block/md0/md/sync_action",
            "sh -c echo idle > /sys/block/md0/md/sync_action"
        ]
    );
}

#[test]
fn real_mdadm_scrub_start_writes_check_to_sync_action_via_the_same_sysfs_convention_as_throttle() {
    // Must reuse Stage B's `sh -c echo VALUE > path` convention
    // verbatim, not invent a second one (`tee`, etc).
    let runner = NonDryRunRunner::default();
    let mdadm = MdadmExecutor::new(&runner);
    mdadm.scrub_start("md0").unwrap();
    assert!(
        runner
            .get_recorded()
            .contains(&"sh -c echo check > /sys/block/md0/md/sync_action".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn real_mdadm_scrub_cancel_writes_idle_to_sync_action() {
    let runner = NonDryRunRunner::default();
    let mdadm = MdadmExecutor::new(&runner);
    mdadm.scrub_cancel("md0").unwrap();
    assert!(
        runner
            .get_recorded()
            .contains(&"sh -c echo idle > /sys/block/md0/md/sync_action".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn dry_run_mdadm_scrub_error_count_is_zero_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    assert_eq!(mdadm.scrub_error_count("md0").unwrap(), 0);
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_mdadm_scrub_error_count_reads_mismatch_cnt_via_the_runner() {
    let runner = NonDryRunRunner::default();
    let mdadm = MdadmExecutor::new(&runner);
    assert_eq!(mdadm.scrub_error_count("md0").unwrap(), 3);
    assert!(
        runner
            .get_recorded()
            .contains(&"cat /sys/block/md0/md/mismatch_cnt".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn dry_run_pvresize_issues_pvresize() {
    let runner = DryRunRunner::new();
    let lvm = LvmExecutor::new(&runner);
    lvm.pvresize("/dev/md0").unwrap();
    assert_eq!(runner.get_recorded(), vec!["pvresize /dev/md0"]);
}

#[test]
fn real_pv_vg_name_reads_via_pvs() {
    // An earlier review finding: before blindly rolling back a failed
    // vgextend, the engine needs to know whether the PV actually joined the
    // VG despite the reported failure (a partial vgextend commit). Goes
    // through the runner so it's mockable, same rationale as every other
    // real-system read in this crate.
    let runner = NonDryRunRunner::default();
    let lvm = LvmExecutor::new(&runner);
    lvm.pv_vg_name("/dev/md1").unwrap();
    assert!(
        runner
            .get_recorded()
            .contains(&"pvs --noheadings -o vg_name /dev/md1".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn dry_run_pv_vg_name_is_empty_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let lvm = LvmExecutor::new(&runner);
    assert_eq!(lvm.pv_vg_name("/dev/md1").unwrap(), "");
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_vg_exists_issues_a_live_vgs_check_for_the_requested_name() {
    // `vg_exists` is the preflight-stage guard backing
    // `OrchestrationEngine::create` -- it must ask the LIVE system, not
    // state.toml, so a hand-created (or another tool's) VG of the same
    // name is still caught.
    let runner = NonDryRunRunner::default();
    let lvm = LvmExecutor::new(&runner);
    let _ = lvm.vg_exists("shr_vg");
    assert!(
        runner
            .get_recorded()
            .contains(&"vgs --noheadings -o vg_name shr_vg".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn real_vg_exists_is_true_when_vgs_succeeds() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Value("shr_vg")]);
    let lvm = LvmExecutor::new(&runner);
    assert!(lvm.vg_exists("shr_vg").unwrap());
}

#[test]
fn real_vg_exists_is_false_when_vgs_reports_not_found() {
    // `vgs` on a name that doesn't exist exits nonzero ("Volume group \"x\"
    // not found") -- that must read as "doesn't exist", not propagate as an
    // error that would block every ordinary create().
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Fail]);
    let lvm = LvmExecutor::new(&runner);
    assert!(!lvm.vg_exists("shr_vg").unwrap());
}

#[test]
fn dry_run_vg_exists_is_false_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let lvm = LvmExecutor::new(&runner);
    assert!(!lvm.vg_exists("shr_vg").unwrap());
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_lv_exists_issues_a_live_lvs_check_for_vg_slash_lv() {
    let runner = NonDryRunRunner::default();
    let lvm = LvmExecutor::new(&runner);
    let _ = lvm.lv_exists("shr_vg", "data");
    assert!(
        runner
            .get_recorded()
            .contains(&"lvs --noheadings -o lv_name shr_vg/data".to_string()),
        "{:?}",
        runner.get_recorded()
    );
}

#[test]
fn real_lv_exists_is_true_when_lvs_succeeds() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Value("data")]);
    let lvm = LvmExecutor::new(&runner);
    assert!(lvm.lv_exists("shr_vg", "data").unwrap());
}

#[test]
fn real_lv_exists_is_false_when_lvs_reports_not_found() {
    let runner = ScriptedRunner::new(vec![ScriptedOutcome::Fail]);
    let lvm = LvmExecutor::new(&runner);
    assert!(!lvm.lv_exists("shr_vg", "data").unwrap());
}

#[test]
fn dry_run_lv_exists_is_false_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let lvm = LvmExecutor::new(&runner);
    assert!(!lvm.lv_exists("shr_vg", "data").unwrap());
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_level_and_device_count_parses_mdadm_detail_export() {
    // An earlier review finding: `mdadm --grow` is not atomic -- a failure
    // can still have partially applied. Before assuming a failed grow left
    // the array unchanged (and so safe to roll back spares from), the
    // engine reads the array's REAL level/device count back.
    let runner = NonDryRunRunner::default();
    let mdadm = MdadmExecutor::new(&runner);
    let (level, count) = mdadm.level_and_device_count("md0").unwrap();
    assert_eq!(level, "raid5");
    assert_eq!(count, 3);
}

#[test]
fn dry_run_level_and_device_count_does_not_touch_the_system() {
    let runner = DryRunRunner::new();
    let mdadm = MdadmExecutor::new(&runner);
    // Dry-run has no real array to introspect; any stable placeholder is
    // fine as long as nothing is actually run.
    let _ = mdadm.level_and_device_count("md0");
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn dry_run_lvm_and_btrfs() {
    let runner = DryRunRunner::new();
    let lvm = LvmExecutor::new(&runner);
    let btrfs = BtrfsExecutor::new(&runner);

    lvm.pvcreate("/dev/md0").unwrap();
    lvm.vgcreate("shr_vg", &["/dev/md0"]).unwrap();
    lvm.lvcreate_max("shr_vg", "data").unwrap();
    btrfs.mkfs("/dev/shr_vg/data", Some("SHR_DATA")).unwrap();
    btrfs
        .mount("/dev/shr_vg/data", "/mnt/data", Some("zstd:3"), None)
        .unwrap();

    let cmds = runner.get_recorded();
    assert_eq!(cmds.len(), 5);
    assert_eq!(cmds[0], "pvcreate -ff -y /dev/md0");
    assert_eq!(cmds[1], "vgcreate shr_vg /dev/md0");
    // Layer 3: without an explicit wipe answer, a non-interactive
    // lvcreate defaults its "existing signature detected, wipe it?"
    // prompt to "no" and aborts. `--wipesignatures y` alone does NOT
    // suppress that prompt on real EL9 lvm2 (measured on the guest) --
    // `--yes` is what does -- see `LvmExecutor::lvcreate_max`'s doc comment.
    assert_eq!(cmds[2], "lvcreate -l 100%FREE -n data -Wy -Zy --yes shr_vg");
    assert_eq!(
        cmds[3],
        "mkfs.btrfs -f -d single -m single -L SHR_DATA /dev/shr_vg/data"
    );
    assert_eq!(cmds[4], "mount -o compress=zstd:3 /dev/shr_vg/data /mnt/data");
}

#[test]
fn dry_run_btrfs_recompress_issues_defragment_with_the_requested_algorithm() {
    // `defragment -c` only accepts a bare algorithm name -- the level
    // must be stripped, real btrfs-progs v6.12 rejects `-czstd:9`.
    let runner = DryRunRunner::new();
    let btrfs = BtrfsExecutor::new(&runner);
    btrfs.recompress("/mnt/data", "zstd:9").unwrap();
    assert_eq!(
        runner.get_recorded(),
        vec!["btrfs filesystem defragment -r -czstd /mnt/data"]
    );
}

#[test]
fn dry_run_btrfs_scrub_start_and_cancel_issue_the_expected_commands() {
    // Unlike mdadm's sysfs-write scrub, `btrfs scrub` is a real subcommand
    // with its own argv -- DryRunRunner records it like any other command
    // (no special no-op branch needed, since there's nothing destructive to
    // simulate away here beyond what DryRunRunner already does universally).
    let runner = DryRunRunner::new();
    let btrfs = BtrfsExecutor::new(&runner);
    btrfs.scrub_start("/mnt/data").unwrap();
    btrfs.scrub_cancel("/mnt/data").unwrap();
    assert_eq!(
        runner.get_recorded(),
        vec!["btrfs scrub start /mnt/data", "btrfs scrub cancel /mnt/data"]
    );
}

#[test]
fn dry_run_btrfs_scrub_status_is_the_default_without_touching_the_system() {
    let runner = DryRunRunner::new();
    let btrfs = BtrfsExecutor::new(&runner);
    assert_eq!(
        btrfs.scrub_status("/mnt/data").unwrap(),
        shr_exec::BtrfsScrubStatus::default()
    );
    assert!(runner.get_recorded().is_empty());
}

#[test]
fn real_btrfs_scrub_status_parses_a_running_scrub_via_the_runner() {
    struct RunningScrubRunner;
    impl CommandRunner for RunningScrubRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
            assert_eq!(program, "btrfs");
            assert_eq!(args, ["scrub", "status", "/mnt/data"]);
            Ok(CommandOutput {
                stdout: "UUID:  abc\nStatus:  running\nError summary:  no errors found\n".to_string(),
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }
    let runner = RunningScrubRunner;
    let btrfs = BtrfsExecutor::new(&runner);
    let status = btrfs.scrub_status("/mnt/data").unwrap();
    assert!(status.running);
    assert_eq!(status.error_count, 0);
}

#[test]
fn real_btrfs_scrub_status_reports_the_error_count_not_just_completion() {
    struct ErroredScrubRunner;
    impl CommandRunner for ErroredScrubRunner {
        fn run(&self, _program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
            Ok(CommandOutput {
                stdout: "Status:  finished\nError summary:  read=2 csum=1 verify=0\n".to_string(),
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }
    let runner = ErroredScrubRunner;
    let btrfs = BtrfsExecutor::new(&runner);
    let status = btrfs.scrub_status("/mnt/data").unwrap();
    assert!(!status.running, "a finished scrub must not report running");
    assert_eq!(
        status.error_count, 3,
        "must sum every error category, not just report 'finished'"
    );
}
