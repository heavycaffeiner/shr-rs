//! lsblk parsing tests, driven by realistic `lsblk -J -b` fixtures.

use shr_inspect::parse_lsblk;

const FIXTURE: &str = r#"{
  "blockdevices": [
    {"name":"sda","size":4000787030016,"type":"disk","model":"WDC WD40EFRX-68N32N0 ","serial":"WD-WCC7K1ABCDEF","rota":true,"tran":"sata",
     "children":[{"name":"sda1","size":4000651313152,"type":"part","partuuid":"e8a37c1a-4f22-4b8d-9c1e-abc123def456","fstype":"linux_raid_member"}]},
    {"name":"sdb","size":6000000000000,"type":"disk","model":"ST6000VN001","serial":"ZGY12345","rota":true,"tran":"sata","children":[]},
    {"name":"nvme0n1","size":512110190592,"type":"disk","model":"Samsung SSD 980","serial":"S1234","rota":false,"tran":"nvme",
     "children":[{"name":"nvme0n1p1","size":536870912,"type":"part","fstype":"vfat","mountpoint":"/boot/efi"}]}
  ]
}"#;

#[test]
fn parses_three_disks() {
    let out = parse_lsblk(FIXTURE).unwrap();
    assert_eq!(out.disks().count(), 3);
}

#[test]
fn extracts_size_model_serial() {
    let out = parse_lsblk(FIXTURE).unwrap();
    let sda = out.disks().find(|d| d.name == "sda").unwrap();
    assert_eq!(sda.size, Some(4000787030016));
    assert_eq!(sda.model_trimmed().as_deref(), Some("WDC WD40EFRX-68N32N0"));
    assert_eq!(sda.serial_trimmed().as_deref(), Some("WD-WCC7K1ABCDEF"));
}

#[test]
fn detects_existing_content() {
    let out = parse_lsblk(FIXTURE).unwrap();
    // sda has an mdadm member partition; sdb is blank.
    assert!(out.disks().find(|d| d.name == "sda").unwrap().has_content());
    assert!(!out.disks().find(|d| d.name == "sdb").unwrap().has_content());
}

#[test]
fn detects_rotational_flag() {
    let out = parse_lsblk(FIXTURE).unwrap();
    assert_eq!(
        out.disks().find(|d| d.name == "nvme0n1").unwrap().rota,
        Some(false)
    );
    assert_eq!(
        out.disks().find(|d| d.name == "sda").unwrap().rota,
        Some(true)
    );
}

#[test]
fn tolerates_string_encoded_size() {
    // Older lsblk emits sizes as strings.
    let json = r#"{"blockdevices":[{"name":"sdz","size":"12345","type":"disk"}]}"#;
    let out = parse_lsblk(json).unwrap();
    assert_eq!(out.blockdevices[0].size, Some(12345));
}

#[test]
fn existing_partition_without_filesystem_counts_as_content() {
    // An empty but partitioned disk must still warn on add.
    let json = r#"{"blockdevices":[{"name":"sdp","size":1000,"type":"disk",
      "children":[{"name":"sdp1","size":900,"type":"part","partuuid":"abc-123"}]}]}"#;
    let out = parse_lsblk(json).unwrap();
    assert!(out.blockdevices[0].has_content());
}

#[test]
fn whitespace_only_fstype_is_not_content() {
    let json = r#"{"blockdevices":[{"name":"sdq","size":1000,"type":"disk","fstype":"  "}]}"#;
    let out = parse_lsblk(json).unwrap();
    assert!(!out.blockdevices[0].has_content());
}

#[test]
fn tolerates_null_fields() {
    let json =
        r#"{"blockdevices":[{"name":"sdz","size":null,"type":"disk","model":null,"serial":null}]}"#;
    let out = parse_lsblk(json).unwrap();
    let d = &out.blockdevices[0];
    assert_eq!(d.size, None);
    assert_eq!(d.model_trimmed(), None);
}
