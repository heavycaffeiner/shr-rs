//! `SafetyGuard` exact-match regression suite (D4).
//!
//! The old implementation matched with `disk_path.contains(sys_disk) ||
//! sys_disk.contains(disk_path)`, which produced both directions of failure:
//! - false negative: a by-id path for the system disk does not contain its
//!   kernel name anywhere (`ata-WDC_...` doesn't contain `sda`), so it passed.
//! - false positive: `/dev/loop10` contains `loop1` as a substring, so a
//!   loopback test disk was wrongly treated as the system disk `loop1`.
//!
//! These tests pin the normalized-exact-match replacement.

use shr_exec::{ExecError, SafetyGuard};

#[test]
fn blocks_bare_kernel_name_of_a_system_disk() {
    let system_disks = vec!["sda".to_string()];
    assert!(SafetyGuard::validate_disk_target("sda", &system_disks).is_err());
}

#[test]
fn blocks_dev_path_and_partition_of_a_system_disk() {
    let system_disks = vec!["sda".to_string()];
    assert!(SafetyGuard::validate_disk_target("/dev/sda", &system_disks).is_err());
    assert!(SafetyGuard::validate_disk_target("/dev/sda1", &system_disks).is_err());
}

#[test]
fn by_id_alias_of_a_system_disk_is_blocked_by_exact_match() {
    // A caller (CLI/engine) that resolved disk identity via shr-inspect must
    // list every known alias -- kernel name *and* by-id name -- of a
    // protected disk in `system_disks` (see
    // `shr_command::system_disk_aliases`). Exact matching against the by-id
    // alias closes the false negative: the old substring check never caught
    // this, because "ata-WDC_WD40EFRX_SYS" does not contain "sda" anywhere.
    let system_disks = vec!["sda".to_string(), "ata-WDC_WD40EFRX_SYS".to_string()];
    let res =
        SafetyGuard::validate_disk_target("/dev/disk/by-id/ata-WDC_WD40EFRX_SYS", &system_disks);
    assert!(res.is_err());
}

#[test]
fn loop10_is_not_confused_with_system_disk_loop1() {
    // Old substring check: "loop10".contains("loop1") == true -> false positive.
    let system_disks = vec!["loop1".to_string()];
    assert!(SafetyGuard::validate_disk_target("/dev/loop10", &system_disks).is_ok());
}

#[test]
fn loop_partition_of_a_protected_loop_disk_is_blocked_but_other_loops_are_not() {
    let system_disks = vec!["loop10".to_string()];
    assert!(SafetyGuard::validate_disk_target("/dev/loop10p1", &system_disks).is_err());
    assert!(SafetyGuard::validate_disk_target("/dev/loop11p1", &system_disks).is_ok());
}

#[test]
fn nvme_partition_naming_does_not_collide_with_a_similarly_prefixed_disk() {
    let system_disks = vec!["nvme0n1".to_string()];
    assert!(SafetyGuard::validate_disk_target("/dev/nvme0n1p1", &system_disks).is_err());
    assert!(SafetyGuard::validate_disk_target("/dev/nvme0n10p1", &system_disks).is_ok());
}

#[test]
fn non_system_disk_is_allowed() {
    let system_disks = vec!["sda".to_string()];
    assert!(SafetyGuard::validate_disk_target("/dev/sdb", &system_disks).is_ok());
}

#[test]
fn non_ascii_disk_reference_does_not_panic() {
    // An earlier review finding: `looks_like_kernel_name` byte-slices its input
    // (`&s[..2]`), which panics if a multi-byte UTF-8 character straddles
    // that boundary. A malformed/malicious disk reference must be rejected
    // as non-kernel-name-shaped, not crash the safety layer.
    let system_disks = vec!["sda".to_string()];
    let res = SafetyGuard::validate_disk_target("日x", &system_disks);
    assert!(res.is_ok());
}

#[test]
fn empty_system_disk_list_is_an_error_not_a_pass() {
    // Absence of a confirmed exclusion list must never be interpreted as "safe".
    let res = SafetyGuard::validate_disk_target("/dev/sdb", &[]);
    assert!(matches!(res, Err(ExecError::SafetyViolation(_))));
}
