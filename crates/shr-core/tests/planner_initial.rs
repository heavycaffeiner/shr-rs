//! Initial-layout planner unit tests.

use shr_core::{plan_initial, Disk, PlanError, PlannerInput, RaidLevel, RedundancyMode};

const TB: u64 = 1_000_000_000_000;

fn disks(sizes: &[u64]) -> Vec<Disk> {
    sizes
        .iter()
        .enumerate()
        .map(|(i, s)| Disk::new(format!("d{i}"), *s))
        .collect()
}

fn plan(sizes: &[u64], mode: RedundancyMode) -> shr_core::PlannerOutput {
    plan_initial(&PlannerInput::exact(disks(sizes), mode)).expect("plan should succeed")
}

#[test]
fn shr_2disk_equal_is_raid1() {
    let out = plan(&[4 * TB, 4 * TB], RedundancyMode::Shr);
    assert_eq!(out.bands.len(), 1);
    assert_eq!(out.bands[0].level(), RaidLevel::Raid1);
    assert_eq!(out.bands[0].members().len(), 2);
    assert_eq!(out.metrics.total_usable, 4 * TB);
    assert!(out.unusable_per_disk.is_empty());
}

#[test]
fn shr_3disk_equal_is_raid5() {
    let out = plan(&[4 * TB, 4 * TB, 4 * TB], RedundancyMode::Shr);
    assert_eq!(out.bands.len(), 1);
    assert_eq!(out.bands[0].level(), RaidLevel::Raid5);
    assert_eq!(out.bands[0].members().len(), 3);
    // RAID5 over 3 members => 2 members' worth of data.
    assert_eq!(out.metrics.total_usable, 8 * TB);
}

#[test]
fn shr_4disk_3346_is_raid5_plus_raid1() {
    let out = plan(&[3 * TB, 3 * TB, 4 * TB, 6 * TB], RedundancyMode::Shr);
    assert_eq!(out.bands.len(), 2);

    let b0 = &out.bands[0];
    assert_eq!(b0.level(), RaidLevel::Raid5);
    assert_eq!(b0.members().len(), 4);
    assert_eq!(b0.size(), 3 * TB);

    let b1 = &out.bands[1];
    assert_eq!(b1.level(), RaidLevel::Raid1);
    assert_eq!(b1.members().len(), 2);
    assert_eq!(b1.size(), TB);

    // The lone 6 TB disk strands 2 TB (6 - 3 in band0 - 1 in band1).
    let six_tb = out.unusable_per_disk.iter().find(|(_, &b)| b == 2 * TB);
    assert!(six_tb.is_some(), "expected a disk with 2 TB unusable");

    // band0: 3*(4-1)=9 TB usable; band1: 1 TB usable => 10 TB total.
    assert_eq!(out.metrics.total_usable, 10 * TB);
    // Overhead: band0 3 TB parity + band1 1 TB mirror = 4 TB.
    assert_eq!(out.metrics.redundancy_overhead, 4 * TB);
}

#[test]
fn shr_2disk_unequal_strands_tail() {
    let out = plan(&[4 * TB, 6 * TB], RedundancyMode::Shr);
    assert_eq!(out.bands.len(), 1);
    assert_eq!(out.bands[0].level(), RaidLevel::Raid1);
    assert_eq!(out.bands[0].size(), 4 * TB);
    assert_eq!(out.metrics.total_usable, 4 * TB);
    assert_eq!(out.unusable_per_disk.values().sum::<u64>(), 2 * TB);
}

#[test]
fn shr2_4disk_equal_is_raid6() {
    let out = plan(&[4 * TB, 4 * TB, 4 * TB, 4 * TB], RedundancyMode::Shr2);
    assert_eq!(out.bands.len(), 1);
    assert_eq!(out.bands[0].level(), RaidLevel::Raid6);
    assert_eq!(out.bands[0].members().len(), 4);
    // RAID6 over 4 => 2 members' worth of data.
    assert_eq!(out.metrics.total_usable, 8 * TB);
}

#[test]
fn shr2_5disk_strict_strands_upper_slice() {
    // [4,4,4,4,6]: band0 = 4 TB x5 RAID6; the top 2 TB on the 6 TB disk has
    // only one member, which strict SHR-2 cannot make redundant.
    let out = plan(
        &[4 * TB, 4 * TB, 4 * TB, 4 * TB, 6 * TB],
        RedundancyMode::Shr2,
    );
    assert_eq!(out.bands.len(), 1);
    assert_eq!(out.bands[0].level(), RaidLevel::Raid6);
    assert_eq!(out.bands[0].members().len(), 5);
    // 4*(5-2) = 12 TB usable.
    assert_eq!(out.metrics.total_usable, 12 * TB);
    assert_eq!(out.unusable_per_disk.values().sum::<u64>(), 2 * TB);
}

#[test]
fn shr2_three_disks_is_too_few() {
    let err = plan_initial(&PlannerInput::exact(
        disks(&[4 * TB, 4 * TB, 4 * TB]),
        RedundancyMode::Shr2,
    ))
    .unwrap_err();
    assert!(matches!(err, PlanError::TooFewDisks { got: 3, min: 4 }));
}

#[test]
fn shr_one_disk_is_too_few() {
    let err =
        plan_initial(&PlannerInput::exact(disks(&[4 * TB]), RedundancyMode::Shr)).unwrap_err();
    assert!(matches!(err, PlanError::TooFewDisks { got: 1, min: 2 }));
}

#[test]
fn reserves_and_alignment_shrink_usable() {
    // With production defaults, usable size is below raw and 4 GiB-aligned.
    let out = plan_initial(&PlannerInput::new(
        disks(&[4 * TB, 4 * TB]),
        RedundancyMode::Shr,
    ))
    .unwrap();
    assert_eq!(out.bands.len(), 1);
    let band = &out.bands[0];
    assert!(band.size() < 4 * TB);
    assert_eq!(band.size() % DEFAULT_ALIGN, 0);
}

#[test]
fn all_zero_disks_have_no_redundant_capacity() {
    let err = plan_initial(&PlannerInput::exact(disks(&[0, 0]), RedundancyMode::Shr)).unwrap_err();
    assert!(matches!(err, PlanError::NoRedundantCapacity), "got {err:?}");
}

#[test]
fn sub_alignment_disks_have_no_redundant_capacity() {
    // Four disks smaller than the reserved head alone: nothing usable remains,
    // so the count check passes but no band can form.
    let tiny = 1024 * 1024; // 1 MiB
    let err = plan_initial(&PlannerInput::new(
        disks(&[tiny, tiny, tiny, tiny]),
        RedundancyMode::Shr2,
    ))
    .unwrap_err();
    assert!(matches!(err, PlanError::NoRedundantCapacity), "got {err:?}");
}

const DEFAULT_ALIGN: u64 = 4 * 1024 * 1024 * 1024;

#[test]
fn more_than_256_bands_is_rejected_not_wrapped() {
    // Band_index is a u8 (0..=255, 256 values). `plan_initial` used
    // to compute it as `bands.len() as u8`, which wraps silently: the 257th
    // successful band push (bands.len() == 256 at that point) would collide
    // with band 0 inside the returned PlannerOutput.
    //
    // This is built genuinely end-to-end through `plan_initial`, not via a
    // direct `RedundantBand::new` call: N distinct-size disks (byte-exact,
    // no reserves/alignment via `PlannerInput::exact`) produce exactly N-1
    // redundant bands, because the topmost slice always has only 1 member
    // (the single largest disk) and is reported unusable rather than banded.
    // With N=258 disks of sizes 1..=258 bytes, that is exactly 257 bands --
    // one past the 256 a u8 can address -- so the LAST push is the first
    // one to overflow. This is the honest way to exercise the guard: no
    // shortcut, no direct band construction, the real planner loop runs
    // into the real limit.
    let sizes: Vec<u64> = (1..=258).collect();
    let err = plan_initial(&PlannerInput::exact(disks(&sizes), RedundancyMode::Shr)).unwrap_err();
    assert!(
        matches!(err, PlanError::TooManyBands { .. }),
        "got {err:?}"
    );
}
