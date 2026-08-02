//! shr-command tests: status composition from an inspector, and dry-run plans.

use std::collections::{BTreeMap, HashMap};

use shr_command::{
    build_fs_df, build_plan_report, build_status, render, system_disk_aliases, Health, SmartState,
};
use shr_core::{Disk, RedundancyMode};
use shr_inspect::StaticInspector;
use shr_state::{
    ArrayState, ScrubOutcome, StateBand, StateDisk, StateExpansion, StateFile, StateFilesystem,
    StateScrubResult,
};

const TB: u64 = 1_000_000_000_000;

const LSBLK: &str = r#"{"blockdevices":[
  {"name":"sda","size":4000000000000,"type":"disk","model":"WD Red","serial":"A",
   "children":[{"name":"sda1","type":"part","fstype":"linux_raid_member"}]},
  {"name":"sdb","size":4000000000000,"type":"disk","model":"WD Red","serial":"B",
   "children":[{"name":"sdb1","type":"part","fstype":"linux_raid_member"}]}
]}"#;

const MDSTAT: &str = "Personalities : [raid1]
md0 : active raid1 sda1[0] sdb1[1]
      3900000000 blocks super 1.2 [2/2] [UU]
";

fn inspector() -> StaticInspector {
    let mut smart = HashMap::new();
    smart.insert(
        "sda".to_string(),
        r#"{"smart_status":{"passed":true},"temperature":{"current":38}}"#.to_string(),
    );
    smart.insert(
        "sdb".to_string(),
        r#"{"smart_status":{"passed":true},"temperature":{"current":40},
           "ata_smart_attributes":{"table":[{"id":197,"raw":{"value":1}}]}}"#
            .to_string(),
    );
    let mut by_id = shr_inspect::ByIdIndex::empty();
    by_id.insert("sda", "ata-WD_A");
    by_id.insert("sdb", "ata-WD_B");
    StaticInspector::from_raw(LSBLK, MDSTAT, smart)
        .unwrap()
        .with_by_id(by_id)
}

#[test]
fn status_composes_disks_arrays_and_membership() {
    let report = build_status(&inspector(), None).unwrap();

    assert_eq!(report.health, Health::Healthy);
    assert_eq!(report.disks.len(), 2);

    let sda = report.disks.iter().find(|d| d.name == "sda").unwrap();
    assert_eq!(sda.arrays, vec!["md0"]);
    assert_eq!(sda.smart.state, SmartState::Ok);
    assert_eq!(sda.smart.temperature_c, Some(38));
    assert_eq!(sda.id.as_deref(), Some("ata-WD_A"));
    assert!(!sda.system_disk);

    let sdb = report.disks.iter().find(|d| d.name == "sdb").unwrap();
    assert_eq!(sdb.smart.state, SmartState::Warning); // pending sector
    assert_eq!(sdb.id.as_deref(), Some("ata-WD_B"));

    assert_eq!(report.arrays.len(), 1);
    let md0 = &report.arrays[0];
    assert_eq!(md0.name, "md0");
    assert_eq!(md0.level.as_deref(), Some("raid1"));
    assert!(!md0.degraded);
    assert_eq!(md0.members, vec!["sda1", "sdb1"]);
}

#[test]
fn status_marks_system_disks() {
    let insp = StaticInspector::from_raw(
        r#"{"blockdevices":[
          {"name":"sda","size":500000000000,"type":"disk",
           "children":[{"name":"sda1","type":"part","mountpoint":"/"}]},
          {"name":"sdb","size":4000000000000,"type":"disk"}
        ]}"#,
        "",
        HashMap::new(),
    )
    .unwrap();
    let report = build_status(&insp, None).unwrap();
    let sda = report.disks.iter().find(|d| d.name == "sda").unwrap();
    assert!(sda.system_disk);
    assert_eq!(sda.system_mounts, vec!["/".to_string()]);
    let sdb = report.disks.iter().find(|d| d.name == "sdb").unwrap();
    assert!(!sdb.system_disk);
}

#[test]
fn status_serializes_to_json() {
    let report = build_status(&inspector(), None).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["health"], "healthy");
    assert_eq!(json["disks"][0]["arrays"][0], "md0");
    assert!(json["arrays"][0]["members"].is_array());
}

/// `status --json` must carry the `state.toml` path the caller
/// resolved so the dashboard can show it, but `build_status` itself never
/// invents one (no filesystem access -- see `state`'s doc comment on
/// `build_status`) and the field is omitted from the JSON entirely when
/// unknown, matching `DiskStatus::id`'s precedent for additive optional
/// fields rather than serializing a `null` that could be confused with a
/// deliberately-cleared value.
#[test]
fn status_report_state_path_defaults_to_none_and_is_omitted_from_json() {
    let report = build_status(&inspector(), None).unwrap();
    assert_eq!(report.state_path, None);
    let json = serde_json::to_value(&report).unwrap();
    assert!(
        json.get("state_path").is_none(),
        "state_path key must be omitted, not null, when unknown: {json}"
    );
}

/// The caller (shr-cli) is expected to stamp the real resolved path onto an
/// already-built report -- this asserts the JSON contract actually carries
/// whatever the caller set, verbatim, once it does.
#[test]
fn status_report_state_path_serializes_verbatim_when_the_caller_set_one() {
    let mut report = build_status(&inspector(), None).unwrap();
    report.state_path = Some("/var/lib/shr-rs/state.toml".to_string());
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["state_path"], "/var/lib/shr-rs/state.toml");
}

#[test]
fn status_json_has_schema_version_and_smart_detail() {
    let json = serde_json::to_value(build_status(&inspector(), None).unwrap()).unwrap();
    assert_eq!(json["schema_version"], 2);
    let disks = json["disks"].as_array().unwrap();
    let sdb = disks.iter().find(|d| d["name"] == "sdb").unwrap();
    assert_eq!(sdb["smart"]["pending_sectors"], 1);
}

#[test]
fn inactive_array_is_not_healthy() {
    let insp = StaticInspector::from_raw(
        r#"{"blockdevices":[{"name":"sda","size":1000000000000,"type":"disk"},
            {"name":"sdb","size":1000000000000,"type":"disk"}]}"#,
        "md0 : inactive sda1[0](S) sdb1[1](S)\n",
        HashMap::new(),
    )
    .unwrap();
    let report = build_status(&insp, None).unwrap();
    assert_eq!(report.health, Health::Degraded);
}

#[test]
fn read_only_array_is_not_healthy() {
    let insp = StaticInspector::from_raw(
        LSBLK,
        "md0 : active (read-only) raid1 sda1[0] sdb1[1]\n\
         3900000000 blocks super 1.2 [2/2] [UU]\n",
        HashMap::new(),
    )
    .unwrap();
    let report = build_status(&insp, None).unwrap();
    assert_eq!(report.health, Health::Degraded);
    assert!(report.arrays[0].read_only);
}

#[test]
fn impossible_three_member_raid6_is_not_healthy() {
    let insp = StaticInspector::from_raw(
        r#"{"blockdevices":[
          {"name":"sda","size":1000000000000,"type":"disk"},
          {"name":"sdb","size":1000000000000,"type":"disk"},
          {"name":"sdc","size":1000000000000,"type":"disk"}
        ]}"#,
        "md0 : active raid6 sda1[0] sdb1[1] sdc1[2]\n\
         2900000000 blocks super 1.2 [3/3] [UUU]\n",
        HashMap::new(),
    )
    .unwrap();
    let report = build_status(&insp, None).unwrap();
    assert_eq!(report.health, Health::Degraded);
}

#[test]
fn status_renders_without_panic() {
    let text = render::render_status(&build_status(&inspector(), None).unwrap());
    assert!(text.contains("HEALTHY"));
    assert!(text.contains("md0"));
}

#[test]
fn no_array_is_unknown_health() {
    let empty = StaticInspector::from_raw(
        r#"{"blockdevices":[{"name":"sdz","size":1000000000000,"type":"disk"}]}"#,
        "Personalities : [raid1]\nunused devices: <none>\n",
        HashMap::new(),
    )
    .unwrap();
    let report = build_status(&empty, None).unwrap();
    assert_eq!(report.health, Health::Unknown);
    // A disk with no SMART data reports Unknown, not Ok.
    assert_eq!(report.disks[0].smart.state, SmartState::Unknown);
}

#[test]
fn plan_report_reflects_shr_layout() {
    let disks = vec![
        Disk::new("d0", 4 * TB),
        Disk::new("d1", 4 * TB),
        Disk::new("d2", 6 * TB),
    ];
    let report = build_plan_report(RedundancyMode::Shr, disks).unwrap();

    assert_eq!(report.mode, "shr");
    assert_eq!(report.bands.len(), 1);
    assert_eq!(report.bands[0].level, "raid5");
    assert_eq!(report.bands[0].members.len(), 3);
    // The 6 TB disk strands ~2 TB.
    assert_eq!(report.unusable_per_disk.len(), 1);
    assert!(report.metrics.total_usable > 0);

    // JSON round-trips.
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("\"raid5\""));

    // Renders without panic.
    let text = render::render_plan(&report);
    assert!(text.contains("band0"));
}

#[test]
fn system_disk_aliases_include_kernel_name_and_by_id_but_not_data_disks() {
    // D5's fix (CLI wiring `expand` to real preflight) relies on this: the
    // SafetyGuard exact-match check (D4) needs every known alias of a
    // system disk, not just its kernel name, or a by-id reference to the
    // same disk slips through un-blocked.
    let insp = StaticInspector::from_raw(
        r#"{"blockdevices":[
          {"name":"sda","size":500000000000,"type":"disk",
           "children":[{"name":"sda1","type":"part","mountpoint":"/"}]},
          {"name":"sdb","size":4000000000000,"type":"disk"}
        ]}"#,
        "",
        HashMap::new(),
    )
    .unwrap()
    .with_by_id({
        let mut idx = shr_inspect::ByIdIndex::empty();
        idx.insert("sda", "ata-WDC_SYS");
        idx.insert("sdb", "ata-DATA");
        idx
    });

    let aliases = system_disk_aliases(&insp).unwrap();

    assert!(aliases.contains(&"sda".to_string()));
    assert!(aliases.contains(&"ata-WDC_SYS".to_string()));
    assert!(!aliases.contains(&"sdb".to_string()));
    assert!(!aliases.contains(&"ata-DATA".to_string()));
}

#[test]
fn system_disk_aliases_scan_the_whole_host_not_just_requested_targets() {
    // An earlier review finding: an earlier version derived this list from
    // `preflight_write_targets`, which only inspects the disks a
    // create/expand request targets. A request for data-only disks (the
    // normal, successful case) never mentions sda, so that version came
    // back empty -- which then made SafetyGuard's "empty list is an error"
    // rule (D4) reject every legitimate request. This must scan the whole
    // host's inventory, independent of what's being requested.
    let insp = StaticInspector::from_raw(
        r#"{"blockdevices":[
          {"name":"sda","size":500000000000,"type":"disk",
           "children":[{"name":"sda1","type":"part","mountpoint":"/"}]},
          {"name":"sdb","size":4000000000000,"type":"disk"}
        ]}"#,
        "",
        HashMap::new(),
    )
    .unwrap()
    .with_by_id({
        let mut idx = shr_inspect::ByIdIndex::empty();
        idx.insert("sda", "ata-WDC_SYS");
        idx.insert("sdb", "ata-DATA");
        idx
    });

    // Note: nothing here even mentions "sda" -- it must still show up.
    let aliases = system_disk_aliases(&insp).unwrap();
    assert!(aliases.contains(&"sda".to_string()));
    assert!(aliases.contains(&"ata-WDC_SYS".to_string()));
}

#[test]
fn plan_rejects_too_few_disks() {
    let err = build_plan_report(RedundancyMode::Shr2, vec![Disk::new("d0", 4 * TB)]);
    assert!(err.is_err());
}

/// A minimal but structurally-real `ArrayState` fixture -- band `usable_bytes`
/// and `resize_pending` values are made up numbers/flags, not zero-value
/// placeholders, so a test asserting on them can't pass by accident.
fn sample_group(name: &str) -> ArrayState {
    ArrayState {
        name: name.to_string(),
        mode: "shr".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        layout_version: 3,
        disks: vec![
            StateDisk {
                id: "ata-WD_A".to_string(),
                size_bytes: 4 * TB,
                serial: Some("A".to_string()),
                model: Some("WD Red".to_string()),
                added_at: "2026-01-01T00:00:00Z".to_string(),
                partitions: vec![],
            },
            StateDisk {
                id: "ata-WD_B".to_string(),
                size_bytes: 6 * TB,
                serial: Some("B".to_string()),
                model: Some("WD Red".to_string()),
                added_at: "2026-01-01T00:00:00Z".to_string(),
                partitions: vec![],
            },
        ],
        bands: vec![
            StateBand {
                index: 0,
                level: "raid1".to_string(),
                md_name: "md0".to_string(),
                md_uuid: Some("12345678:9abcdef0:12345678:9abcdef0".to_string()),
                member_partitions: vec!["ata-WD_A-part1".to_string(), "ata-WD_B-part1".to_string()],
                usable_bytes: 4 * TB,
                resize_pending: false,
                last_smart_reallocated: None,
                last_scrub: None,
            scrub_in_progress: false,
                pending_member_removal: None,
                reshape_priority: None,
            },
            StateBand {
                index: 1,
                level: "raid1".to_string(),
                md_name: "md1".to_string(),
                md_uuid: Some("87654321:0fedcba9:87654321:0fedcba9".to_string()),
                member_partitions: vec!["ata-WD_B-part2".to_string()],
                usable_bytes: 2 * TB,
                resize_pending: true,
                last_smart_reallocated: None,
                last_scrub: None,
            scrub_in_progress: false,
                pending_member_removal: None,
                reshape_priority: None,
            },
        ],
        filesystem: StateFilesystem {
            fs_uuid: Some("11111111-2222-4333-8444-555555555555".to_string()),
            mount_point: "/mnt/shr_data".to_string(),
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
        },
        expansion: StateExpansion::default(),
    }
}

#[test]
fn status_has_no_groups_when_state_toml_does_not_exist() {
    // The whole point of `Option<&StateFile>`: a fresh host (or Cockpit
    // polling before anything has ever been `create`d) must not error and
    // must not invent a group -- `None` in, `groups: []` out.
    let report = build_status(&inspector(), None).unwrap();
    assert_eq!(report.groups, vec![]);
    // The pre-existing inventory-derived sections are unaffected.
    assert_eq!(report.disks.len(), 2);
    assert_eq!(report.arrays.len(), 1);
}

#[test]
fn status_projects_state_toml_groups_alongside_live_inventory() {
    let state = StateFile::new(vec![sample_group("shr1"), sample_group("shr2-hetero")]);
    let report = build_status(&inspector(), Some(&state)).unwrap();

    // Live inventory (disks/arrays) is untouched by state.toml's presence --
    // it still comes purely from the inspector. The two sections stay
    // independent by design.
    assert_eq!(report.disks.len(), 2);
    assert_eq!(report.arrays.len(), 1);

    assert_eq!(report.groups.len(), 2);
    let shr1 = &report.groups[0];
    assert_eq!(shr1.name, "shr1");
    assert_eq!(shr1.mode, "shr");
    assert_eq!(shr1.layout_version, 3);
    assert_eq!(shr1.mount_point, "/mnt/shr_data");
    assert_eq!(
        shr1.fs_uuid.as_deref(),
        Some("11111111-2222-4333-8444-555555555555")
    );
    // Sum of both bands' usable_bytes (4 TB + 2 TB), not just the first band.
    assert_eq!(shr1.usable_bytes, 6 * TB);
    // band1 has resize_pending: true -- the group-level flag must surface it.
    assert!(shr1.resize_pending);
    assert_eq!(shr1.disks, vec!["ata-WD_A".to_string(), "ata-WD_B".to_string()]);

    assert_eq!(shr1.bands.len(), 2);
    assert_eq!(shr1.bands[0].index, 0);
    assert_eq!(shr1.bands[0].level, "raid1");
    assert_eq!(shr1.bands[0].md_name, "md0");
    assert_eq!(shr1.bands[0].usable_bytes, 4 * TB);
    assert!(!shr1.bands[0].resize_pending);
    assert_eq!(shr1.bands[1].usable_bytes, 2 * TB);
    assert!(shr1.bands[1].resize_pending);

    assert_eq!(report.groups[1].name, "shr2-hetero");

    // JSON round-trips with the expected shape.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["groups"][0]["name"], "shr1");
    assert_eq!(json["groups"][0]["mode"], "shr");
    assert_eq!(json["groups"][0]["resize_pending"], true);
    assert_eq!(json["groups"][0]["bands"][1]["resize_pending"], true);
}

/// `health` used to be computed purely from the flat live-array list
/// (`md.arrays`), with zero reference to `state.groups`. Two groups: `shr1`
/// has its one band's `md_name` ("md0") matching `inspector()`'s live,
/// healthy raid1 array; `shr-gone` has its one band's `md_name` ("md9")
/// matching nothing live at all. Under the bug, `md.arrays.is_empty()` is
/// false (md0 keeps it non-empty) and `array_needs_attention` never even
/// looks at `shr-gone` (its array doesn't exist in `md.arrays`), so the
/// report claimed `Healthy` while `shr-gone` is entirely unassembled. A
/// single-group test would NOT reproduce this: with only `shr-gone`,
/// `md.arrays` would be globally empty and the (already-correct) `is_empty`
/// branch would produce `Unknown`, masking the bug.
#[test]
fn health_is_not_healthy_when_one_of_two_groups_is_entirely_unassembled() {
    let mut live_group = sample_group("shr1");
    live_group.bands.truncate(1); // keep only band0 (md0, live under inspector())

    let mut gone_group = sample_group("shr-gone");
    gone_group.bands.truncate(1);
    gone_group.bands[0].md_name = "md9".to_string(); // no live array anywhere

    let state = StateFile::new(vec![live_group, gone_group]);
    let report = build_status(&inspector(), Some(&state)).unwrap();

    assert!(
        report.groups[1].bands[0].members.is_empty(),
        "sanity: shr-gone's band really has no live array"
    );
    assert_ne!(
        report.health,
        Health::Healthy,
        "shr-gone has no live array at all -- health must not be Healthy: {:?}",
        report.health
    );
}

#[test]
fn status_renders_groups_without_panic_and_shows_unfinished_expansion() {
    let state = StateFile::new(vec![sample_group("shr1")]);
    let report = build_status(&inspector(), Some(&state)).unwrap();
    let text = render::render_status(&report);
    assert!(text.contains("shr1"));
    assert!(text.contains("expansion unfinished"));
}

// --- Status --detail band correlation, --watch, fs df -----------------

const MDSTAT_RECOVERY: &str = "Personalities : [raid1]
md0 : active raid1 sda1[0] sdb1[1]
      3900000000 blocks super 1.2 [2/2] [UU]
      [===>.................]  recovery = 15.0% (600000000/3900000000) finish=45.0min speed=12345K/sec
";

fn inspector_with_recovery() -> StaticInspector {
    let mut smart = HashMap::new();
    smart.insert(
        "sda".to_string(),
        r#"{"smart_status":{"passed":true},"temperature":{"current":38}}"#.to_string(),
    );
    smart.insert(
        "sdb".to_string(),
        r#"{"smart_status":{"passed":true},"temperature":{"current":40}}"#.to_string(),
    );
    let mut by_id = shr_inspect::ByIdIndex::empty();
    by_id.insert("sda", "ata-WD_A");
    by_id.insert("sdb", "ata-WD_B");
    StaticInspector::from_raw(LSBLK, MDSTAT_RECOVERY, smart)
        .unwrap()
        .with_by_id(by_id)
}

/// `sample_group`'s band0 is `md_name: "md0"` (a live array under
/// `inspector_with_recovery`, mid-recovery) and band1 is `md_name: "md1"`
/// (no live array at all right now -- neither `inspector()` nor
/// `inspector_with_recovery()` reports one). This is exactly the "crash
/// before reconcile re-assembles it" case `GroupBandStatus::members`'s doc
/// comment describes: `build_status` must tell the two apart, never
/// reporting a fabricated "idle" for the band with no live array.
#[test]
fn status_band_detail_correlates_live_sync_and_members_by_md_name() {
    let mut g = sample_group("shr1");
    g.bands[0].last_scrub = Some(StateScrubResult {
        finished_at: "2026-07-01T00:00:00Z".to_string(),
        outcome: ScrubOutcome::Completed,
        error_count: 0,
    });
    g.bands[0].scrub_in_progress = false;
    g.bands[1].scrub_in_progress = true; // scrubbing a band with no live array right now

    let state = StateFile::new(vec![g]);
    let report = build_status(&inspector_with_recovery(), Some(&state)).unwrap();

    let band0 = &report.groups[0].bands[0];
    assert_eq!(band0.members, vec!["sda1".to_string(), "sdb1".to_string()]);
    let sync = band0.sync.as_ref().expect("live mdstat recovery progress");
    assert_eq!(sync.action, "recovery");
    assert_eq!(sync.percent, Some(15.0));
    assert_eq!(sync.finish_min, Some(45.0));
    let scrub = band0.last_scrub.as_ref().expect("scrub history carried through from state.toml");
    assert_eq!(scrub.error_count, 0);
    assert!(!band0.scrub_in_progress);

    let band1 = &report.groups[0].bands[1];
    assert!(band1.members.is_empty(), "no live array named md1 exists");
    assert!(band1.sync.is_none());
    assert!(band1.last_scrub.is_none());
    assert!(band1.scrub_in_progress);
}

// --- Faulty/spare member state -----------------------------------------

const MDSTAT_WITH_FAULTY: &str = "Personalities : [raid5]
md0 : active raid5 sda1[0] sdb1[1] sdc1[2](F)
      2900000000 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/2] [UU_]
";

/// A faulty member must be distinguishable from a healthy one on
/// `ArrayStatus.member_states` -- the exact gap the real-browser repro
/// found: the dashboard counted a faulty member into a 3-disk group's slice
/// math because the JSON contract only ever carried plain member names.
#[test]
fn status_flags_a_faulty_member_on_array_status_without_dropping_it_from_members() {
    let insp = StaticInspector::from_raw(LSBLK, MDSTAT_WITH_FAULTY, HashMap::new()).unwrap();
    let report = build_status(&insp, None).unwrap();
    let md0 = &report.arrays[0];

    // The plain (legacy) field still lists every member, faulty included --
    // additive, backward-compatible (see `ArrayStatus::member_states`'s doc
    // comment).
    assert_eq!(md0.members, vec!["sda1".to_string(), "sdb1".to_string(), "sdc1".to_string()]);

    let faulty = md0.member_states.iter().find(|m| m.name == "sdc1").expect("faulty member present");
    assert!(faulty.faulty);
    assert!(!faulty.spare);
    assert_eq!(faulty.role, Some(2));

    let healthy = md0.member_states.iter().find(|m| m.name == "sda1").expect("healthy member present");
    assert!(!healthy.faulty);
    assert!(!healthy.spare);
}

/// The same per-member detail must propagate onto `GroupBandStatus`
/// (`sample_group`'s band0 is `md_name: "md0"`, matching `MDSTAT_WITH_FAULTY`).
#[test]
fn status_propagates_faulty_member_state_onto_the_matching_group_band() {
    let insp = StaticInspector::from_raw(LSBLK, MDSTAT_WITH_FAULTY, HashMap::new()).unwrap();
    let state = StateFile::new(vec![sample_group("shr1")]);
    let report = build_status(&insp, Some(&state)).unwrap();

    let band0 = &report.groups[0].bands[0];
    let faulty = band0.member_states.iter().find(|m| m.name == "sdc1").expect("band carries member_states too");
    assert!(faulty.faulty);
}

/// `GroupBandStatus::md_uuid` mirrors `StateBand::md_uuid` verbatim -- the
/// value is already known (persisted from `mdadm --detail --export` at
/// band-creation/grow time), so this costs zero extra commands per `status`.
#[test]
fn group_band_status_mirrors_md_uuid_from_state_toml() {
    let state = StateFile::new(vec![sample_group("shr1")]);
    let report = build_status(&inspector(), Some(&state)).unwrap();
    assert_eq!(
        report.groups[0].bands[0].md_uuid.as_deref(),
        Some("12345678:9abcdef0:12345678:9abcdef0")
    );
    assert_eq!(
        report.groups[0].bands[1].md_uuid.as_deref(),
        Some("87654321:0fedcba9:87654321:0fedcba9")
    );
}

/// `GroupStatus::{vg_name,lv_name,compression}` mirror
/// `StateFilesystem::{vg_name,lv_name,compression}` verbatim -- the Cockpit
/// dashboard needs these and `state.toml` already stores them.
#[test]
fn group_status_carries_vg_name_lv_name_and_compression_from_state_toml() {
    let state = StateFile::new(vec![sample_group("shr1")]);
    let report = build_status(&inspector(), Some(&state)).unwrap();
    let g = &report.groups[0];
    assert_eq!(g.vg_name, "shr_vg");
    assert_eq!(g.lv_name, "data");
    assert_eq!(g.compression, "zstd:3");
}

#[test]
fn status_detail_renders_correlated_band_without_panic() {
    let state = StateFile::new(vec![sample_group("shr1")]);
    let report = build_status(&inspector_with_recovery(), Some(&state)).unwrap();
    let text = render::render_status_detail(&report);
    assert!(text.contains("md0"));
    assert!(text.contains("recovery"));
    assert!(text.contains("no live mdadm array")); // band1 (md1)
}

#[test]
fn status_watch_frame_renders_live_recovery_progress_without_panic() {
    let report = build_status(&inspector_with_recovery(), None).unwrap();
    let meta = render::WatchFrameMeta { width: 72, max_height: 20 };
    let text = render::render_status_watch_frame(&report, &meta);
    assert_eq!(text.lines().count(), meta.max_height);
    assert!(text.contains("recovery"));
}

#[test]
fn fs_df_uses_group_usable_bytes_and_marks_unknown_usage_as_unknown() {
    let state = StateFile::new(vec![sample_group("shr1")]);
    let report = build_status(&inspector(), Some(&state)).unwrap();

    // No live Btrfs usage supplied at all -- nothing here should be
    // fabricated from `usable_bytes` or any other already-known figure.
    let df = build_fs_df(&report.groups, &BTreeMap::new());
    assert_eq!(df.groups.len(), 1);
    assert_eq!(df.groups[0].name, "shr1");
    assert_eq!(df.groups[0].usable_bytes, report.groups[0].usable_bytes);
    assert!(df.groups[0].data_used_bytes.is_none());
    assert!(df.groups[0].unallocated_bytes.is_none());

    let text = render::render_fs_df(&df);
    assert!(text.contains("shr1"));
    assert!(text.contains('?'));
}

// --- Pending_member_removal reaching the JSON report -------------------
//
// Real-guest repro (2026-07-30): after `disk replace`, the old member stays
// attached and reads `(F)` in `/proc/mdstat` while its copy finishes -- the
// `--remove` is deferred on purpose. That is indistinguishable in the
// old JSON contract from a genuine second fault: `GroupBandStatus` never
// carried `StateBand::pending_member_removal` at all. These tests drive the
// real `build_status` (never hand-construct `GroupBandStatus`) to prove the
// fact reaches the report AND lands in a form a frontend can actually match
// against `member_states` -- a bare pass-through of the by-partuuid path
// state.toml stores would not share `member_states`' kernel-name convention
// and so could not be correlated to a specific row at all.

const LSBLK_WITH_PARTUUID: &str = r#"{"blockdevices":[
  {"name":"sda","size":4000000000000,"type":"disk",
   "children":[{"name":"sda1","type":"part","fstype":"linux_raid_member"}]},
  {"name":"sdb","size":4000000000000,"type":"disk",
   "children":[{"name":"sdb1","type":"part","fstype":"linux_raid_member"}]},
  {"name":"sdc","size":4000000000000,"type":"disk",
   "children":[{"name":"sdc1","type":"part","fstype":"linux_raid_member",
                "partuuid":"AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"}]}
]}"#;

const MDSTAT_REPLACE_PENDING: &str = "Personalities : [raid5]
md0 : active raid5 sda1[0] sdb1[1] sdc1[2](F)
      2900000000 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/2] [UU_]
";

#[test]
fn status_resolves_pending_member_removal_to_the_kernel_name_member_states_uses() {
    let insp =
        StaticInspector::from_raw(LSBLK_WITH_PARTUUID, MDSTAT_REPLACE_PENDING, HashMap::new())
            .unwrap();

    let mut g = sample_group("shr1");
    // Lowercase on purpose -- `StatePartition::part_uuid` and lsblk's own
    // PARTUUID reporting are not guaranteed to share letter case, and this
    // must resolve regardless.
    g.bands[0].pending_member_removal =
        Some("/dev/disk/by-partuuid/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string());
    let state = StateFile::new(vec![g]);

    let report = build_status(&insp, Some(&state)).unwrap();
    let band0 = &report.groups[0].bands[0];

    // The resolved value must be the exact string `member_states` names this
    // member with -- that is what "correlated" means here.
    assert_eq!(band0.pending_member_removal.as_deref(), Some("sdc1"));
    let flagged = band0
        .member_states
        .iter()
        .find(|m| Some(m.name.as_str()) == band0.pending_member_removal.as_deref())
        .expect("the resolved name must match a real member_states entry, not just any string");
    assert!(flagged.faulty, "the correlated member is the one mdstat actually marks (F)");

    // JSON carries it too, under the same key state.toml uses.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["groups"][0]["bands"][0]["pending_member_removal"], "sdc1");
}

#[test]
fn status_falls_back_to_the_raw_partuuid_path_when_it_cannot_be_resolved() {
    // The disk has been physically pulled: no lsblk entry carries this
    // PARTUUID any more. The FACT that a removal is pending must still
    // surface -- silently dropping back to `None` here would look identical
    // to "nothing pending" and reintroduce the exact confusion this closes.
    let mut g = sample_group("shr1");
    g.bands[0].pending_member_removal =
        Some("/dev/disk/by-partuuid/00000000-0000-0000-0000-000000000000".to_string());
    let state = StateFile::new(vec![g]);

    // `inspector()`'s LSBLK constant has no matching PARTUUID anywhere.
    let report = build_status(&inspector(), Some(&state)).unwrap();
    let band0 = &report.groups[0].bands[0];
    assert_eq!(
        band0.pending_member_removal.as_deref(),
        Some("/dev/disk/by-partuuid/00000000-0000-0000-0000-000000000000")
    );
}

#[test]
fn status_reports_no_pending_removal_and_omits_the_json_key_when_state_has_none() {
    // `sample_group` leaves `pending_member_removal: None` on every band --
    // never fabricated as `false`/empty string/present-but-null.
    let report =
        build_status(&inspector(), Some(&StateFile::new(vec![sample_group("shr1")]))).unwrap();
    let band0 = &report.groups[0].bands[0];
    assert_eq!(band0.pending_member_removal, None);

    let json = serde_json::to_value(&report).unwrap();
    assert!(
        json["groups"][0]["bands"][0].get("pending_member_removal").is_none(),
        "key must be omitted, not null, when nothing is pending: {json}"
    );
}

/// The human `status --detail` view must also tell the two cases apart --
/// the operator driving the CLI faces the identical `(F)` marker ambiguity
/// the browser/TUI do (see the module doc comment above).
#[test]
fn status_detail_explains_a_pending_removal_next_to_the_faulty_member() {
    let insp =
        StaticInspector::from_raw(LSBLK_WITH_PARTUUID, MDSTAT_REPLACE_PENDING, HashMap::new())
            .unwrap();
    let mut g = sample_group("shr1");
    g.bands[0].pending_member_removal =
        Some("/dev/disk/by-partuuid/AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".to_string());
    let state = StateFile::new(vec![g]);
    let report = build_status(&insp, Some(&state)).unwrap();

    let text = render::render_status_detail(&report);
    assert!(text.contains("sdc1(F)"), "{text}");
    assert!(
        text.contains("pending-removal: sdc1"),
        "must name which member, not just say something is pending: {text}"
    );
    assert!(
        text.to_lowercase().contains("not a new fault") || text.to_lowercase().contains("not new"),
        "must explain the benign case in-line, not just flag it: {text}"
    );
}
