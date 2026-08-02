//! Distribution / capacity metrics for a planned layout. Always included in
//! `shr-rs plan` output.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::band::RedundantBand;
use crate::disk::{Disk, DiskId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistributionMetrics {
    /// Per-disk fill fraction (0.0..=1.0): how much of each disk the bands use.
    pub utilization: BTreeMap<DiskId, f64>,
    /// Fraction of total raw capacity that carries neither user data nor
    /// parity — i.e. reserved head/tail, alignment loss, and non-redundant
    /// stranded regions. Equals `(total_raw - total_usable - overhead) / raw`.
    pub waste_ratio: f64,
    /// Total user-visible capacity across all bands.
    pub total_usable: u64,
    /// Sum of every disk's raw size.
    pub total_raw: u64,
    /// Raw capacity spent on parity/mirroring across all bands.
    pub redundancy_overhead: u64,
    /// Bytes carrying neither data nor parity (reserved + alignment + stranded).
    pub stranded_bytes: u64,
}

/// Compute metrics for a set of `bands` over `disks`.
///
/// Accounting is conserved by construction:
/// `total_usable + redundancy_overhead + stranded_bytes == total_raw`.
pub fn compute_metrics(disks: &[Disk], bands: &[RedundantBand]) -> DistributionMetrics {
    let total_raw: u64 = disks.iter().map(|d| d.size_bytes).sum();
    let total_usable: u64 = bands.iter().map(|b| b.usable_bytes()).sum();
    let redundancy_overhead: u64 = bands.iter().map(|b| b.raw_bytes() - b.usable_bytes()).sum();

    // Everything not carried as data or parity is stranded: reserved head/tail,
    // alignment loss, and capacity on disks too shallow to join an upper band.
    let stranded_bytes = total_raw
        .saturating_sub(total_usable)
        .saturating_sub(redundancy_overhead);

    // Bytes each disk contributes to bands it is a member of.
    let mut consumed: BTreeMap<DiskId, u64> = BTreeMap::new();
    for b in bands {
        for m in b.members() {
            *consumed.entry(m.clone()).or_insert(0) += b.size();
        }
    }

    let mut utilization = BTreeMap::new();
    for d in disks {
        let used = consumed.get(&d.id).copied().unwrap_or(0);
        let frac = if d.size_bytes == 0 {
            0.0
        } else {
            used as f64 / d.size_bytes as f64
        };
        utilization.insert(d.id.clone(), frac);
    }

    let waste_ratio = if total_raw == 0 {
        0.0
    } else {
        stranded_bytes as f64 / total_raw as f64
    };

    DistributionMetrics {
        utilization,
        waste_ratio,
        total_usable,
        total_raw,
        redundancy_overhead,
        stranded_bytes,
    }
}
