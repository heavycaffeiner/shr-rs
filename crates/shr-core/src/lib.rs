//! `shr-core` — the pure, OS-independent heart of shr-rs.
//!
//! This crate contains the domain model (disks, RAID levels, redundant bands)
//! and the planner (initial layout + expansion diff). It performs **no** I/O,
//! shells out to **nothing**, and knows nothing about `mdadm`/`lvm`/`btrfs`.
//! That keeps every invariant unit-testable on any platform, including the
//! Windows host used during development.
//!
//! Resolving a live `/dev/sdX` into a stable [`DiskId`], reading SMART, etc.
//! all live in `shr-inspect`; executing plans lives in `shr-exec`.

pub mod band;
pub mod disk;
pub mod expansion;
pub mod metrics;
pub mod planner;
pub mod raid;

pub use band::{BandError, RedundantBand};
pub use disk::{Disk, DiskId};
pub use expansion::{plan_expansion, ExpansionPlan, ExpansionStep, LayoutSnapshot, UnusableReason};
pub use metrics::{compute_metrics, DistributionMetrics};
pub use planner::{
    plan_initial, PlanError, PlannerInput, PlannerOutput, PlannerWarning, DEFAULT_BAND_ALIGNMENT,
    DEFAULT_RESERVED_HEAD, DEFAULT_RESERVED_TAIL,
};
pub use raid::{RaidLevel, RedundancyMode};
