//! The initial-layout planner: given a set of disks and a mode, carve them into
//! redundant bands -- the "sliced" in Sliced Hybrid RAID. One slice boundary
//! per distinct usable size.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::band::{BandError, RedundantBand};
use crate::disk::{Disk, DiskId};
use crate::metrics::{compute_metrics, DistributionMetrics};
use crate::raid::RedundancyMode;

/// Default band alignment (SHR convention): 4 GiB.
pub const DEFAULT_BAND_ALIGNMENT: u64 = 4 * 1024 * 1024 * 1024;
/// Default reserved head (partition table + slack): 128 MiB.
pub const DEFAULT_RESERVED_HEAD: u64 = 128 * 1024 * 1024;
/// Default reserved tail (mdadm 1.2 superblock slack): 8 MiB.
pub const DEFAULT_RESERVED_TAIL: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PlannerInput {
    pub disks: Vec<Disk>,
    pub mode: RedundancyMode,
    pub band_alignment: u64,
    pub reserved_head: u64,
    pub reserved_tail: u64,
}

impl PlannerInput {
    /// Production defaults (4 GiB alignment, reserved head/tail).
    pub fn new(disks: Vec<Disk>, mode: RedundancyMode) -> Self {
        Self {
            disks,
            mode,
            band_alignment: DEFAULT_BAND_ALIGNMENT,
            reserved_head: DEFAULT_RESERVED_HEAD,
            reserved_tail: DEFAULT_RESERVED_TAIL,
        }
    }

    /// No reserves, byte-exact alignment — for reasoning and tests where clean
    /// arithmetic matters.
    pub fn exact(disks: Vec<Disk>, mode: RedundancyMode) -> Self {
        Self {
            disks,
            mode,
            band_alignment: 1,
            reserved_head: 0,
            reserved_tail: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlannerWarning {
    /// A slice had members, but too few to be redundant in this mode.
    TooFewForBand {
        offset: u64,
        size: u64,
        members: usize,
        needed: usize,
    },
    /// A disk has non-redundant capacity stranded at its tail.
    UnusableTail { disk: DiskId, bytes: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerOutput {
    pub bands: Vec<RedundantBand>,
    pub unusable_per_disk: BTreeMap<DiskId, u64>,
    pub metrics: DistributionMetrics,
    pub warnings: Vec<PlannerWarning>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("too few disks: got {got}, mode requires at least {min}")]
    TooFewDisks { got: usize, min: usize },
    #[error("no redundant band could be formed from the given disks")]
    NoRedundantCapacity,
    #[error("invalid current layout snapshot: {0}")]
    InvalidSnapshot(String),
    #[error("expansion would be unsafe: {0}")]
    UnsafeExpansion(String),
    #[error("layout version would overflow u64")]
    LayoutVersionOverflow,
    #[error("layout would need band_index {index}, which does not fit in u8 (max 255)")]
    TooManyBands { index: usize },
    #[error(transparent)]
    Band(#[from] BandError),
}

fn align_down(value: u64, align: u64) -> u64 {
    if align <= 1 {
        value
    } else {
        value - (value % align)
    }
}

/// Plan the initial layout for a fresh array.
pub fn plan_initial(input: &PlannerInput) -> Result<PlannerOutput, PlanError> {
    let min = input.mode.min_initial_disks();
    if input.disks.len() < min {
        return Err(PlanError::TooFewDisks {
            got: input.disks.len(),
            min,
        });
    }

    // 1. Usable length per disk after reserves + alignment.
    let mut usable: Vec<(DiskId, u64)> = input
        .disks
        .iter()
        .map(|d| {
            let raw = d
                .size_bytes
                .saturating_sub(input.reserved_head)
                .saturating_sub(input.reserved_tail);
            (d.id.clone(), align_down(raw, input.band_alignment))
        })
        .collect();
    // Deterministic order: by size, then by id.
    usable.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    // 2. Boundaries: 0 plus each distinct usable size.
    let mut boundary_set: BTreeSet<u64> = usable.iter().map(|(_, s)| *s).collect();
    boundary_set.insert(0);
    let boundaries: Vec<u64> = boundary_set.into_iter().collect();

    // 3. One candidate band per [start, end) slice.
    let mut bands = Vec::new();
    let mut unusable: BTreeMap<DiskId, u64> = BTreeMap::new();
    let mut warnings = Vec::new();

    for w in boundaries.windows(2) {
        let (start, end) = (w[0], w[1]);
        if end <= start {
            continue;
        }
        let size = end - start;

        // Disks whose usable length reaches at least `end` participate here.
        let members: Vec<DiskId> = usable
            .iter()
            .filter(|(_, len)| *len >= end)
            .map(|(id, _)| id.clone())
            .collect();

        match input.mode.pick_level(members.len()) {
            Some(level) => {
                // Band_index is a u8. `bands.len() as u8` wraps
                // silently past 255, colliding a 257th band with band 0
                // inside the returned PlannerOutput -- fail loudly instead
                // (rule #4: never report a result that isn't what happened).
                let band_index =
                    u8::try_from(bands.len()).map_err(|_| PlanError::TooManyBands { index: bands.len() })?;
                let band = RedundantBand::new(band_index, start, size, members, level, input.mode)?;
                bands.push(band);
            }
            None => {
                if members.len() >= 2 {
                    warnings.push(PlannerWarning::TooFewForBand {
                        offset: start,
                        size,
                        members: members.len(),
                        needed: input.mode.min_members_per_band(),
                    });
                }
                for id in &members {
                    *unusable.entry(id.clone()).or_insert(0) += size;
                }
            }
        }
    }

    for (id, bytes) in &unusable {
        warnings.push(PlannerWarning::UnusableTail {
            disk: id.clone(),
            bytes: *bytes,
        });
    }

    // A layout with no redundant band is not a layout — reject rather than
    // returning an empty "success" (e.g. all-zero or sub-alignment disks).
    if bands.is_empty() {
        return Err(PlanError::NoRedundantCapacity);
    }

    let metrics = compute_metrics(&input.disks, &bands);

    Ok(PlannerOutput {
        bands,
        unusable_per_disk: unusable,
        metrics,
        warnings,
    })
}
