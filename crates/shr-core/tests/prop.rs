//! Property-based invariants for the planner.

use proptest::prelude::*;
use shr_core::{plan_initial, Disk, PlannerInput, RedundancyMode};

const TB: u64 = 1_000_000_000_000;

proptest! {
    /// Every band the planner emits is well-formed: enough members for the
    /// mode, the level the mode would pick, and never more usable than raw.
    #[test]
    fn bands_are_wellformed(
        sizes in prop::collection::vec(1u64..=20u64, 1..8usize),
        shr2 in any::<bool>(),
    ) {
        let mode = if shr2 { RedundancyMode::Shr2 } else { RedundancyMode::Shr };
        prop_assume!(sizes.len() >= mode.min_initial_disks());

        let ds: Vec<Disk> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| Disk::new(format!("d{i}"), s * TB))
            .collect();
        let out = plan_initial(&PlannerInput::exact(ds, mode)).unwrap();

        for b in &out.bands {
            prop_assert!(b.members().len() >= mode.min_members_per_band());
            prop_assert_eq!(mode.pick_level(b.members().len()), Some(b.level()));
            prop_assert!(b.usable_bytes() <= b.raw_bytes());
        }

        // Capacity accounting is conserved: usable + overhead == raw-in-bands.
        let raw_in_bands: u64 = out.bands.iter().map(|b| b.raw_bytes()).sum();
        prop_assert_eq!(
            out.metrics.total_usable + out.metrics.redundancy_overhead,
            raw_in_bands
        );

        // And over the whole array: usable + overhead + stranded == total raw.
        prop_assert_eq!(
            out.metrics.total_usable
                + out.metrics.redundancy_overhead
                + out.metrics.stranded_bytes,
            out.metrics.total_raw
        );
        prop_assert!(out.metrics.waste_ratio >= 0.0 && out.metrics.waste_ratio <= 1.0);
    }

    /// Losing `fault_tolerance` arbitrary members of any band keeps it
    /// recoverable — the central redundancy guarantee.
    #[test]
    fn bands_survive_configured_disk_loss(
        sizes in prop::collection::vec(1u64..=20u64, 2..8usize),
        shr2 in any::<bool>(),
    ) {
        let mode = if shr2 { RedundancyMode::Shr2 } else { RedundancyMode::Shr };
        prop_assume!(sizes.len() >= mode.min_initial_disks());

        let ds: Vec<Disk> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| Disk::new(format!("d{i}"), s * TB))
            .collect();
        let out = plan_initial(&PlannerInput::exact(ds, mode)).unwrap();

        let ft = mode.fault_tolerance() as usize;
        for b in &out.bands {
            // Any `ft` members may die and the band still recovers.
            let victims: Vec<_> = b.members().iter().take(ft).cloned().collect();
            prop_assert!(b.is_recoverable_without(&victims));
        }
    }
}
