//! /proc/mdstat parsing tests.

use shr_inspect::parse_mdstat;

const FIXTURE: &str = "Personalities : [raid1] [raid6] [raid5] [raid4]
md0 : active raid6 sdd1[3] sdc1[2] sdb1[1] sda1[0]
      7813770240 blocks super 1.2 level 6, 512k chunk, algorithm 2 [4/4] [UUUU]
      [==>..................]  reshape = 12.6% (123456/987654) finish=34.5min speed=12345K/sec
      bitmap: 0/8 pages [0KB], 65536KB chunk

md1 : active raid1 sdf1[1] sde1[0](F)
      1953382400 blocks super 1.2 [2/1] [U_]

unused devices: <none>
";

#[test]
fn parses_personalities_and_two_arrays() {
    let md = parse_mdstat(FIXTURE);
    assert_eq!(md.personalities, vec!["raid1", "raid6", "raid5", "raid4"]);
    assert_eq!(md.arrays.len(), 2);
}

#[test]
fn parses_healthy_raid6_with_reshape() {
    let md = parse_mdstat(FIXTURE);
    let a = &md.arrays[0];
    assert_eq!(a.name, "md0");
    assert_eq!(a.state, "active");
    assert_eq!(a.level.as_deref(), Some("raid6"));
    assert_eq!(a.members.len(), 4);
    assert_eq!(a.blocks, Some(7813770240));
    assert_eq!(a.raid_disks, Some(4));
    assert_eq!(a.active_disks, Some(4));
    assert_eq!(a.health.as_deref(), Some("UUUU"));
    assert!(!a.is_degraded());

    let sync = a.sync.as_ref().expect("reshape in progress");
    assert_eq!(sync.action, "reshape");
    assert!((sync.percent.unwrap() - 12.6).abs() < 1e-9);
    assert_eq!(sync.speed_kb, Some(12345));
    assert_eq!(sync.finish_min, Some(34.5));
    assert!(!a.read_only);

    // Members carry their role index.
    let sda1 = a.members.iter().find(|m| m.name == "sda1").unwrap();
    assert_eq!(sda1.role, Some(0));
}

#[test]
fn detects_degraded_and_faulty_member() {
    let md = parse_mdstat(FIXTURE);
    let a = &md.arrays[1];
    assert_eq!(a.name, "md1");
    assert_eq!(a.level.as_deref(), Some("raid1"));
    assert_eq!(a.raid_disks, Some(2));
    assert_eq!(a.active_disks, Some(1));
    assert_eq!(a.health.as_deref(), Some("U_"));
    assert!(a.is_degraded());

    let faulty = a.members.iter().find(|m| m.name == "sde1").unwrap();
    assert!(faulty.faulty);
}

#[test]
fn empty_mdstat_has_no_arrays() {
    let md = parse_mdstat("Personalities : \nunused devices: <none>\n");
    assert!(md.arrays.is_empty());
}

#[test]
fn captures_auto_read_only_state() {
    let text = "md2 : active (auto-read-only) raid1 sda1[0] sdb1[1]\n      1000 blocks super 1.2 [2/2] [UU]\n";
    let md = parse_mdstat(text);
    let a = &md.arrays[0];
    assert!(a.read_only);
    assert_eq!(a.level.as_deref(), Some("raid1"));
    assert_eq!(a.members.len(), 2);
}

#[test]
fn captures_replacement_member() {
    let text = "md3 : active raid5 sda1[0] sdb1[1] sdc1[2] sdd1[4](R)\n      1000 blocks super 1.2 [3/3] [UUU]\n";
    let md = parse_mdstat(text);
    let rep = md.arrays[0]
        .members
        .iter()
        .find(|m| m.name == "sdd1")
        .unwrap();
    assert!(rep.replacement);
    assert!(!rep.faulty);
}

#[test]
fn captures_pending_resync_without_percentage() {
    let text = "md4 : active raid1 sda1[0] sdb1[1]\n      1000 blocks super 1.2 [2/2] [UU]\n      \tresync=PENDING\n";
    let md = parse_mdstat(text);
    let sync = md.arrays[0].sync.as_ref().expect("pending resync");
    assert_eq!(sync.action, "resync");
    assert_eq!(sync.percent, None);
}

#[test]
fn degraded_by_counts_without_health_group() {
    // Detail line has [3/2] counts but no [UU_] group.
    let text = "md5 : active raid5 sda1[0] sdb1[1]\n      1000 blocks super 1.2 [3/2]\n";
    let md = parse_mdstat(text);
    let a = &md.arrays[0];
    assert_eq!(a.raid_disks, Some(3));
    assert_eq!(a.active_disks, Some(2));
    assert!(a.is_degraded());
}
