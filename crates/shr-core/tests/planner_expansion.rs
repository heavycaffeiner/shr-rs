//! Expansion planner tests, including the
//! adversarial cases Codex flagged: smaller-disk rejection, malformed
//! snapshots, and unusable-region reporting.

use shr_core::{
    plan_expansion, plan_initial, Disk, ExpansionStep, LayoutSnapshot, PlanError, PlannerInput,
    RaidLevel, RedundancyMode, RedundantBand, DEFAULT_RESERVED_HEAD, DEFAULT_RESERVED_TAIL,
};

const TB: u64 = 1_000_000_000_000;

fn disks(sizes: &[u64]) -> Vec<Disk> {
    sizes
        .iter()
        .enumerate()
        .map(|(i, s)| Disk::new(format!("d{i}"), *s))
        .collect()
}

/// Build a starting snapshot from an exact initial plan. Carries the SAME
/// (exact) grid params so expansion recomputes on the same boundaries.
fn snapshot(sizes: &[u64], mode: RedundancyMode) -> LayoutSnapshot {
    let ds = disks(sizes);
    let out = plan_initial(&PlannerInput::exact(ds.clone(), mode)).unwrap();
    LayoutSnapshot {
        disks: ds,
        bands: out.bands,
        mode,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: 0,
        reserved_tail: 0,
    }
}

/// A new disk that continues the `dN` id sequence.
fn new_disk(index: usize, size: u64) -> Disk {
    Disk::new(format!("d{index}"), size)
}

#[test]
fn shr_raid1_promotes_to_raid5_on_third_disk() {
    let cur = snapshot(&[4 * TB, 4 * TB], RedundancyMode::Shr);
    let plan = plan_expansion(&cur, &[new_disk(2, 4 * TB)]).unwrap();

    assert_eq!(plan.target_layout_version, 2);
    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        ExpansionStep::LevelUp {
            band_index,
            from,
            to,
            add_members,
        } => {
            assert_eq!(*band_index, 0);
            assert_eq!(*from, RaidLevel::Raid1);
            assert_eq!(*to, RaidLevel::Raid5);
            assert_eq!(add_members.len(), 1);
        }
        other => panic!("expected LevelUp, got {other:?}"),
    }
}

#[test]
fn shr_raid5_grows_on_fourth_disk() {
    let cur = snapshot(&[4 * TB, 4 * TB, 4 * TB], RedundancyMode::Shr);
    let plan = plan_expansion(&cur, &[new_disk(3, 4 * TB)]).unwrap();

    assert_eq!(plan.steps.len(), 1);
    match &plan.steps[0] {
        ExpansionStep::GrowBand {
            band_index,
            add_members,
        } => {
            assert_eq!(*band_index, 0);
            assert_eq!(add_members.len(), 1);
        }
        other => panic!("expected GrowBand, got {other:?}"),
    }
}

#[test]
fn shr2_raid6_grows_and_reports_unusable_upper_slice() {
    let cur = snapshot(&[4 * TB, 4 * TB, 4 * TB, 4 * TB], RedundancyMode::Shr2);
    let plan = plan_expansion(&cur, &[new_disk(4, 6 * TB)]).unwrap();

    // band0 grows to 5 members; the top 2 TB of the 6 TB disk is reported
    // unusable (strict SHR-2), not silently dropped.
    let grew = plan.steps.iter().any(|s| {
        matches!(
            s,
            ExpansionStep::GrowBand {
                band_index: 0,
                add_members,
            } if add_members.len() == 1
        )
    });
    assert!(grew, "expected band0 to grow: {:?}", plan.steps);

    let unusable = plan.steps.iter().find_map(|s| match s {
        ExpansionStep::MarkUnusable { size, .. } => Some(*size),
        _ => None,
    });
    assert_eq!(unusable, Some(2 * TB), "expected 2 TB reported unusable");
    assert_eq!(plan.steps.len(), 2);
}

#[test]
fn adding_nothing_is_a_no_op() {
    let cur = snapshot(&[4 * TB, 4 * TB, 4 * TB], RedundancyMode::Shr);
    let plan = plan_expansion(&cur, &[]).unwrap();
    assert!(plan.steps.is_empty());
    // A no-op must not advance the layout version.
    assert_eq!(plan.target_layout_version, cur.layout_version);
}

#[test]
fn empty_band_snapshot_is_rejected() {
    let cur = LayoutSnapshot {
        disks: disks(&[4 * TB, 4 * TB, 4 * TB]),
        bands: vec![],
        mode: RedundancyMode::Shr,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: 0,
        reserved_tail: 0,
    };
    let err = plan_expansion(&cur, &[new_disk(3, 4 * TB)]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}

#[test]
fn duplicate_disk_id_snapshot_is_rejected() {
    // Duplicate disk id even with an otherwise-valid band present: the
    // duplicate-disk check must fire regardless of the band list.
    let ds = vec![
        Disk::new("a", 4 * TB),
        Disk::new("a", 4 * TB),
        Disk::new("b", 4 * TB),
        Disk::new("c", 4 * TB),
    ];
    let band = RedundantBand::new(
        0,
        0,
        TB,
        vec!["a".into(), "b".into(), "c".into()],
        RaidLevel::Raid5,
        RedundancyMode::Shr,
    )
    .unwrap();
    let cur = LayoutSnapshot {
        disks: ds,
        bands: vec![band],
        mode: RedundancyMode::Shr,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: 0,
        reserved_tail: 0,
    };
    let err = plan_expansion(&cur, &[]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}

#[test]
fn duplicate_band_index_is_rejected() {
    // Two structurally-valid bands sharing band_index (isolated from the
    // empty-band path).
    let ds = disks(&[4 * TB, 4 * TB, 4 * TB]);
    let members: Vec<_> = ds.iter().map(|d| d.id.clone()).collect();
    let a = RedundantBand::new(
        0,
        0,
        TB,
        members.clone(),
        RaidLevel::Raid5,
        RedundancyMode::Shr,
    )
    .unwrap();
    let b = RedundantBand::new(
        0,
        TB,
        TB,
        members,
        RaidLevel::Raid5,
        RedundancyMode::Shr,
    )
    .unwrap();
    let cur = LayoutSnapshot {
        disks: ds,
        bands: vec![a, b],
        mode: RedundancyMode::Shr,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: 0,
        reserved_tail: 0,
    };
    let err = plan_expansion(&cur, &[]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}

#[test]
fn duplicate_new_disk_is_rejected() {
    let cur = snapshot(&[4 * TB, 4 * TB, 4 * TB], RedundancyMode::Shr);
    let err = plan_expansion(&cur, &[new_disk(3, 4 * TB), new_disk(3, 4 * TB)]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}

#[test]
fn markunusable_only_plan_keeps_version() {
    // [4,6] already strands 2 TB; adding nothing yields a MarkUnusable-only
    // plan that must NOT advance the layout version.
    let cur = snapshot(&[4 * TB, 6 * TB], RedundancyMode::Shr);
    let plan = plan_expansion(&cur, &[]).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert!(matches!(plan.steps[0], ExpansionStep::MarkUnusable { .. }));
    assert_eq!(plan.target_layout_version, cur.layout_version);
}

#[test]
fn band_level_inconsistent_with_snapshot_mode_is_rejected() {
    // A structurally-valid 4-member RAID5 band must not sit inside a strict
    // SHR-2 snapshot (SHR-2 mandates RAID6).
    let ds = disks(&[4 * TB, 4 * TB, 4 * TB, 4 * TB]);
    let raid5 = RedundantBand::new(
        0,
        0,
        TB,
        ds.iter().map(|d| d.id.clone()).collect(),
        RaidLevel::Raid5,
        RedundancyMode::Shr,
    )
    .unwrap();
    let cur = LayoutSnapshot {
        disks: ds,
        bands: vec![raid5],
        mode: RedundancyMode::Shr2,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: 0,
        reserved_tail: 0,
    };
    let err = plan_expansion(&cur, &[]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}

#[test]
fn adding_a_smaller_disk_is_refused_not_mis_planned() {
    // [4,6] RAID1; adding a 3 TB disk would force a new lower boundary and
    // re-slice band0 — an unsafe overlap. Must be refused.
    let cur = snapshot(&[4 * TB, 6 * TB], RedundancyMode::Shr);
    let err = plan_expansion(&cur, &[new_disk(2, 3 * TB)]).unwrap_err();
    assert!(matches!(err, PlanError::UnsafeExpansion(_)), "got {err:?}");
}

#[test]
fn member_size_check_uses_usable_not_raw_disk_size() {
    // `b.end()` is in usable-space coordinates (0-based, right after
    // reserved_head); the member-size check must compare against
    // size_bytes - reserved_head - reserved_tail, not raw size_bytes.
    //
    // reserved_head=128MiB, reserved_tail=8MiB => 136MiB reserved. A disk
    // whose RAW size is 200MiB has only 200-136=64MiB usable. A band whose
    // extent ends at 100MiB (usable coords) fits in the raw 200MiB (100 <
    // 200) but NOT in the 64MiB actually usable (100 > 64) -- this is
    // exactly the gap the old `sz < b.end()` check (comparing against raw
    // size_bytes) missed: it does not fire here.
    //
    // Note on what this proves: pre-fix, `plan_expansion` as a whole still
    // errors on this input, but via a DIFFERENT, coincidental check --
    // recomputing the ideal layout from the same (too-small) disks derives
    // a smaller band and the geometry-diff step rejects the mismatch with
    // "geometry would change", not "member too small". That is itself the
    // project's recurring "adjacent, not the same" defect: the message an
    // operator would see does not name the real problem. This test asserts
    // the PRECISE error (`InvalidSnapshot` from the member-size check
    // itself), which only the fix produces.
    const MIB: u64 = 1024 * 1024;
    let raw_size = 200 * MIB;
    let band_end = 100 * MIB;
    assert!(
        band_end < raw_size,
        "premise: old raw-size check must NOT catch this"
    );
    let usable = raw_size - DEFAULT_RESERVED_HEAD - DEFAULT_RESERVED_TAIL;
    assert!(
        usable < band_end,
        "premise: the band must genuinely overrun usable space"
    );

    let ds = vec![Disk::new("d0", raw_size), Disk::new("d1", raw_size)];
    let band = RedundantBand::new(
        0,
        0,
        band_end, // size == end since offset is 0
        vec!["d0".into(), "d1".into()],
        RaidLevel::Raid1,
        RedundancyMode::Shr,
    )
    .unwrap();
    let cur = LayoutSnapshot {
        disks: ds,
        bands: vec![band],
        mode: RedundancyMode::Shr,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: DEFAULT_RESERVED_HEAD,
        reserved_tail: DEFAULT_RESERVED_TAIL,
    };
    let err = plan_expansion(&cur, &[]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}

#[test]
fn member_size_check_still_accepts_a_band_that_genuinely_fits() {
    // Companion to the test above: fixing the false negative must not
    // introduce a false positive. Disk raw size is exactly
    // band_end + reserved_head + reserved_tail -- the tightest size that
    // still fits (usable == band_end exactly).
    const MIB: u64 = 1024 * 1024;
    let band_end = 100 * MIB;
    let raw_size = band_end + DEFAULT_RESERVED_HEAD + DEFAULT_RESERVED_TAIL;

    let ds = vec![Disk::new("d0", raw_size), Disk::new("d1", raw_size)];
    let band = RedundantBand::new(
        0,
        0,
        band_end,
        vec!["d0".into(), "d1".into()],
        RaidLevel::Raid1,
        RedundancyMode::Shr,
    )
    .unwrap();
    let cur = LayoutSnapshot {
        disks: ds,
        bands: vec![band],
        mode: RedundancyMode::Shr,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: DEFAULT_RESERVED_HEAD,
        reserved_tail: DEFAULT_RESERVED_TAIL,
    };
    // Must succeed: adding nothing is a no-op, but validate_snapshot runs
    // first and must not reject a band that genuinely fits.
    let plan = plan_expansion(&cur, &[]).unwrap();
    assert!(plan.steps.is_empty());
}

#[test]
fn non_contiguous_snapshot_is_rejected() {
    // A hand-crafted band that does not start at offset 0 is incoherent.
    let ds = disks(&[4 * TB, 4 * TB, 4 * TB]);
    let bogus = RedundantBand::new(
        0,
        100,
        TB,
        ds.iter().map(|d| d.id.clone()).collect(),
        RaidLevel::Raid5,
        RedundancyMode::Shr,
    )
    .unwrap();
    let cur = LayoutSnapshot {
        disks: ds,
        bands: vec![bogus],
        mode: RedundancyMode::Shr,
        layout_version: 1,
        band_alignment: 1,
        reserved_head: 0,
        reserved_tail: 0,
    };
    let err = plan_expansion(&cur, &[]).unwrap_err();
    assert!(matches!(err, PlanError::InvalidSnapshot(_)), "got {err:?}");
}
