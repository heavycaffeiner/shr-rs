//! Write-safety gates: system-mount detection and preflight blockers.

use shr_inspect::{parse_lsblk, preflight_write_targets, system_mounts_on, ByIdIndex, WriteBlocker};

const LSBLK: &str = r#"{"blockdevices":[
  {"name":"sda","size":500000000000,"type":"disk","model":"SystemDisk","serial":"SYS",
   "children":[
     {"name":"sda1","type":"part","fstype":"vfat","mountpoint":"/boot/efi"},
     {"name":"sda2","type":"part","fstype":"xfs","mountpoint":"/"}
   ]},
  {"name":"sdb","size":4000000000000,"type":"disk","model":"DataA","serial":"DA",
   "children":[{"name":"sdb1","type":"part","fstype":"linux_raid_member"}]},
  {"name":"sdc","size":4000000000000,"type":"disk","model":"DataB","serial":"DB"},
  {"name":"sdd","size":4000000000000,"type":"disk","model":"NoId","serial":"N1"}
]}"#;

fn index() -> ByIdIndex {
    let mut idx = ByIdIndex::empty();
    idx.insert("sda", "ata-SystemDisk_SYS");
    idx.insert("sdb", "ata-DataA_DA");
    idx.insert("sdc", "ata-DataB_DB");
    // sdd intentionally missing
    idx
}

#[test]
fn detects_system_mounts_on_root_disk() {
    let lsblk = parse_lsblk(LSBLK).unwrap();
    let sda = lsblk.disks().find(|d| d.name == "sda").unwrap();
    let mounts = system_mounts_on(sda);
    assert!(mounts.iter().any(|m| m == "/"));
    assert!(mounts.iter().any(|m| m == "/boot/efi"));
}

#[test]
fn preflight_blocks_system_and_missing_id() {
    let lsblk = parse_lsblk(LSBLK).unwrap();
    let idx = index();
    let report = preflight_write_targets(&["sda".into(), "sdc".into(), "sdd".into()], &lsblk, &idx, false);
    assert!(!report.ok);
    assert!(report
        .blockers
        .iter()
        .any(|b| matches!(b, WriteBlocker::SystemDisk { name, .. } if name == "sda")));
    assert!(report
        .blockers
        .iter()
        .any(|b| matches!(b, WriteBlocker::NoStableId { name } if name == "sdd")));
    // sdc alone would be fine
    let ok = preflight_write_targets(&["sdc".into()], &lsblk, &idx, false);
    assert!(ok.ok);
}

#[test]
fn preflight_blocks_a_disk_with_existing_content_unless_forced() {
    // An earlier review finding: `has_content` used to be a warning only, never
    // an actual blocker -- `create`/`expand` could silently wipe a disk
    // that already held partitions or a filesystem.
    let lsblk = parse_lsblk(LSBLK).unwrap();
    let idx = index();
    // sdb has a `linux_raid_member` partition -- has_content() == true.
    let blocked = preflight_write_targets(&["sdb".into()], &lsblk, &idx, false);
    assert!(!blocked.ok);
    assert!(blocked
        .blockers
        .iter()
        .any(|b| matches!(b, WriteBlocker::HasContent { name } if name == "sdb")));

    let forced = preflight_write_targets(&["sdb".into()], &lsblk, &idx, true);
    assert!(
        forced.ok,
        "force_content=true must let a content-bearing disk through"
    );
    assert!(
        forced.warnings.iter().any(|w| w.contains("sdb")),
        "forcing past it must still leave a warning: {:?}",
        forced.warnings
    );
}

/// `shr-inspect` cannot know which frontend renders `WriteBlocker`'s
/// `Display` text -- naming any one frontend's override control here (the
/// CLI's `--force-content` flag, the TUI's `o` key, Cockpit's checkbox) is
/// wrong for the other two. Caught live: the TUI showed "pass --force-content"
/// right next to its own, different, `o` hint. The message must still say an
/// override exists, though -- otherwise an operator reading only this line
/// concludes the disk is simply unusable, which is the opposite error.
#[test]
fn has_content_blocker_message_does_not_name_a_frontend_specific_control() {
    let msg = WriteBlocker::HasContent { name: "sdb".into() }.to_string();
    let lower = msg.to_lowercase();
    assert!(
        !lower.contains("--force-content"),
        "must not name the CLI's flag spelling: {msg}"
    );
    assert!(
        !lower.contains("pass ") && !lower.contains("press "),
        "must not instruct a specific keystroke/flag action: {msg}"
    );
    assert!(
        lower.contains("overrid") || lower.contains("unless"),
        "must still communicate that an override exists: {msg}"
    );
}
