//! Expansion planning.
//!
//! Growing an array must be **safe by construction**: existing bands may only
//! be extended (add members) or promoted to a higher redundancy level, never
//! shrunk, downgraded, re-sliced, or have members removed; new bands may only
//! appear *above* the current top extent. Anything else is refused with a
//! [`PlanError::UnsafeExpansion`] rather than emitted as an impossible plan.
//!
//! The algorithm: validate the current snapshot, recompute the ideal layout for
//! `current + new` disks, then diff **by `band_index`** (not vector position),
//! checking geometry and monotonicity for every existing band.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::band::RedundantBand;
use crate::disk::{Disk, DiskId};
use crate::planner::{plan_initial, PlanError, PlannerInput};
use crate::raid::{RaidLevel, RedundancyMode};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnusableReason {
    /// Members exist but no redundant level applies (strict SHR-2).
    InsufficientRedundancy,
}

/// One ordered, resumable step of an expansion. Bands are grown one at a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExpansionStep {
    /// Promote a band to a higher level while adding members (RAID1→5, 5→6).
    LevelUp {
        band_index: u8,
        from: RaidLevel,
        to: RaidLevel,
        add_members: Vec<DiskId>,
    },
    /// Add members to a band at the same level.
    GrowBand {
        band_index: u8,
        add_members: Vec<DiskId>,
    },
    /// Create a brand-new band (a new upper slice became redundant).
    CreateBand { band: RedundantBand },
    /// Record capacity that cannot (yet) be made redundant.
    MarkUnusable {
        disk: DiskId,
        offset: u64,
        size: u64,
        reason: UnusableReason,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpansionPlan {
    pub steps: Vec<ExpansionStep>,
    pub target_layout_version: u64,
}

/// A minimal, pure snapshot of the live layout. `shr-state` maps its persisted
/// `ArrayState` into this so the planner stays free of I/O and TOML types.
///
/// The alignment/reserve parameters the layout was originally planned with are
/// carried here so expansion recomputes the ideal layout on the *same* grid —
/// otherwise unchanged bands would appear to change geometry and be rejected.
#[derive(Debug, Clone)]
pub struct LayoutSnapshot {
    pub disks: Vec<Disk>,
    pub bands: Vec<RedundantBand>,
    pub mode: RedundancyMode,
    pub layout_version: u64,
    pub band_alignment: u64,
    pub reserved_head: u64,
    pub reserved_tail: u64,
}

/// Validate that a snapshot describes a coherent, contiguous SHR layout in the
/// snapshot's own mode before we diff against it. Catches duplicate disks,
/// empty/hand-crafted layouts, mode/level violations, duplicate or
/// hand-crafted band indices, non-contiguous or overlapping extents, and
/// members that don't exist or are too small.
fn validate_snapshot(s: &LayoutSnapshot) -> Result<(), PlanError> {
    // Disk identities must be unique (otherwise a duplicate silently collapses
    // into the size map and hides a single point of failure).
    let mut disk_size: HashMap<&DiskId, u64> = HashMap::new();
    for d in &s.disks {
        if disk_size.insert(&d.id, d.size_bytes).is_some() {
            return Err(PlanError::InvalidSnapshot(format!(
                "duplicate disk id {}",
                d.id
            )));
        }
    }

    // An existing array always has at least one band; an empty band list means
    // this isn't an expansion at all.
    if s.bands.is_empty() {
        return Err(PlanError::InvalidSnapshot(
            "layout snapshot has no bands".to_string(),
        ));
    }

    // Unique band indices.
    let mut seen: HashSet<u8> = HashSet::new();
    for b in &s.bands {
        if !seen.insert(b.band_index()) {
            return Err(PlanError::InvalidSnapshot(format!(
                "duplicate band_index {}",
                b.band_index()
            )));
        }
    }

    // Extents must tile [0, top) contiguously with no gaps or overlaps, every
    // member must exist and cover the extent, and every band must conform to
    // the snapshot's redundancy mode.
    let mut bands: Vec<&RedundantBand> = s.bands.iter().collect();
    bands.sort_by_key(|b| b.offset());
    let mut expected_offset = 0u64;
    for b in &bands {
        if b.members().len() < s.mode.min_members_per_band() {
            return Err(PlanError::InvalidSnapshot(format!(
                "band {} has {} members, below the {:?} minimum {}",
                b.band_index(),
                b.members().len(),
                s.mode,
                s.mode.min_members_per_band()
            )));
        }
        match s.mode.pick_level(b.members().len()) {
            Some(level) if level == b.level() => {}
            _ => {
                return Err(PlanError::InvalidSnapshot(format!(
                    "band {} level {:?} violates {:?} policy for {} members",
                    b.band_index(),
                    b.level(),
                    s.mode,
                    b.members().len()
                )))
            }
        }
        if b.offset() != expected_offset {
            return Err(PlanError::InvalidSnapshot(format!(
                "band {} offset {} is not contiguous (expected {})",
                b.band_index(),
                b.offset(),
                expected_offset
            )));
        }
        for m in b.members() {
            match disk_size.get(m) {
                None => {
                    return Err(PlanError::InvalidSnapshot(format!(
                        "band {} member {} is not in the disk list",
                        b.band_index(),
                        m
                    )))
                }
                Some(&sz) => {
                    // `b.end()` is in usable-space coordinates (0-based,
                    // right after reserved_head -- see `snapshot_from_state` in
                    // shr-orchestrate, which subtracts reserved_head on the way
                    // in). `sz` is the disk's RAW size, so it must be reduced by
                    // both reserves before comparing, or a band whose extent
                    // runs into the reserved_head/reserved_tail slack passes
                    // this check even though the disk cannot actually offer that
                    // much usable space. Saturating: a disk smaller than the
                    // reserves alone must not underflow to a huge usable value.
                    let usable = sz
                        .saturating_sub(s.reserved_head)
                        .saturating_sub(s.reserved_tail);
                    if usable < b.end() {
                        return Err(PlanError::InvalidSnapshot(format!(
                            "disk {} (size {}, usable {} after {} reserved_head + {} reserved_tail) \
                             is too small for band {} extent {}",
                            m,
                            sz,
                            usable,
                            s.reserved_head,
                            s.reserved_tail,
                            b.band_index(),
                            b.end()
                        )));
                    }
                }
            }
        }
        expected_offset = b.end();
    }
    Ok(())
}

/// Plan how to fold `new_disks` into the existing array — safely or not at all.
///
/// This is deliberately conservative: because the ideal layout is recomputed
/// from scratch, adding a disk that would re-slice an *unrelated* existing band
/// (e.g. SHR `[3,3,4,6]` + a 3.5 TB disk) is refused with
/// [`PlanError::UnsafeExpansion`] even though a smarter additive planner could
/// use it partially. Refusing beats emitting an overlapping/unsafe plan; a
/// future planner may relax this. Never emits a plan that shrinks, downgrades,
/// re-slices, or overlaps existing bands.
pub fn plan_expansion(
    current: &LayoutSnapshot,
    new_disks: &[Disk],
) -> Result<ExpansionPlan, PlanError> {
    validate_snapshot(current)?;

    // New disks must carry fresh, unique ids — not duplicated among themselves
    // and not already part of the array (otherwise a duplicate would collapse
    // silently or surface only as an obscure band error).
    let mut known: HashSet<&DiskId> = current.disks.iter().map(|d| &d.id).collect();
    for d in new_disks {
        if !known.insert(&d.id) {
            return Err(PlanError::InvalidSnapshot(format!(
                "new disk id {} is duplicated or already in the array",
                d.id
            )));
        }
    }

    // 1. Recompute the ideal layout over the full disk set, on the SAME grid
    //    (alignment/reserves) the current layout was planned with.
    let mut all = current.disks.clone();
    all.extend_from_slice(new_disks);
    let ideal = plan_initial(&PlannerInput {
        disks: all,
        mode: current.mode,
        band_alignment: current.band_alignment,
        reserved_head: current.reserved_head,
        reserved_tail: current.reserved_tail,
    })?;

    let ideal_by_index: HashMap<u8, &RedundantBand> =
        ideal.bands.iter().map(|b| (b.band_index(), b)).collect();
    let current_indices: HashSet<u8> = current.bands.iter().map(|b| b.band_index()).collect();

    let mut steps = Vec::new();

    // 2. Every existing band must survive monotonically: same geometry, level
    //    kept or raised, members kept or added.
    let mut current_bands: Vec<&RedundantBand> = current.bands.iter().collect();
    current_bands.sort_by_key(|b| b.band_index());
    for cur in &current_bands {
        let ideal_band = ideal_by_index.get(&cur.band_index()).ok_or_else(|| {
            PlanError::UnsafeExpansion(format!(
                "band {} would disappear after replanning",
                cur.band_index()
            ))
        })?;

        if ideal_band.offset() != cur.offset() || ideal_band.size() != cur.size() {
            return Err(PlanError::UnsafeExpansion(format!(
                "band {} geometry would change ({}+{} -> {}+{}); re-slicing existing bands is not allowed",
                cur.band_index(),
                cur.offset(),
                cur.size(),
                ideal_band.offset(),
                ideal_band.size()
            )));
        }
        if ideal_band.level().rank() < cur.level().rank() {
            return Err(PlanError::UnsafeExpansion(format!(
                "band {} would downgrade {:?} -> {:?}",
                cur.band_index(),
                cur.level(),
                ideal_band.level()
            )));
        }
        for m in cur.members() {
            if !ideal_band.contains(m) {
                return Err(PlanError::UnsafeExpansion(format!(
                    "band {} would lose member {}",
                    cur.band_index(),
                    m
                )));
            }
        }

        let added = added_members(cur.members(), ideal_band.members());
        if ideal_band.level() != cur.level() {
            steps.push(ExpansionStep::LevelUp {
                band_index: cur.band_index(),
                from: cur.level(),
                to: ideal_band.level(),
                add_members: added,
            });
        } else if !added.is_empty() {
            steps.push(ExpansionStep::GrowBand {
                band_index: cur.band_index(),
                add_members: added,
            });
        }
    }

    // 3. Genuinely new bands may only sit above the current top extent.
    let current_top = current.bands.iter().map(|b| b.end()).max().unwrap_or(0);
    let mut new_bands: Vec<&RedundantBand> = ideal
        .bands
        .iter()
        .filter(|b| !current_indices.contains(&b.band_index()))
        .collect();
    new_bands.sort_by_key(|b| b.offset());
    for nb in new_bands {
        if nb.offset() < current_top {
            return Err(PlanError::UnsafeExpansion(format!(
                "new band {} at offset {} would overlap existing extent (top {})",
                nb.band_index(),
                nb.offset(),
                current_top
            )));
        }
        steps.push(ExpansionStep::CreateBand {
            band: (*nb).clone(),
        });
    }

    // 4. Surface stranded capacity so the dry-run/UI can show it.
    let ideal_top: u64 = ideal.bands.iter().map(|b| b.size()).sum();
    for (disk, bytes) in &ideal.unusable_per_disk {
        steps.push(ExpansionStep::MarkUnusable {
            disk: disk.clone(),
            offset: ideal_top,
            size: *bytes,
            reason: UnusableReason::InsufficientRedundancy,
        });
    }

    // Only a real geometry change advances the layout version; a plan that is
    // purely informational `MarkUnusable` (or empty) leaves the layout as-is.
    let structural = steps.iter().any(|s| {
        matches!(
            s,
            ExpansionStep::LevelUp { .. }
                | ExpansionStep::GrowBand { .. }
                | ExpansionStep::CreateBand { .. }
        )
    });
    let target_layout_version = if structural {
        current
            .layout_version
            .checked_add(1)
            .ok_or(PlanError::LayoutVersionOverflow)?
    } else {
        current.layout_version
    };

    Ok(ExpansionPlan {
        steps,
        target_layout_version,
    })
}

/// Members present in `ideal` but not in `cur`, preserving `ideal` order.
fn added_members(cur: &[DiskId], ideal: &[DiskId]) -> Vec<DiskId> {
    ideal.iter().filter(|m| !cur.contains(m)).cloned().collect()
}
