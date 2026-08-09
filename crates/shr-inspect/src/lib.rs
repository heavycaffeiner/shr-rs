//! `shr-inspect` — read-only system inspection.
//!
//! Pure parsers that turn the textual/JSON output of `lsblk`, `/proc/mdstat`
//! and `smartctl` into structured data. The parsers do no I/O themselves, so
//! they are fully unit-testable from fixtures on any platform; a
//! [`SystemInspector`] runs the commands and feeds their output here.
//!
//! The inspector layer also owns responsibilities the pure parsers cannot:
//! - normalizing device names (strip `/dev/`, map `by-id`/partition names to
//!   the kernel disk names `diskref`/`safety` compare against);
//! - requesting `PTTYPE` from `lsblk` so an empty-but-partitioned disk is
//!   distinguishable from a truly blank one;
//! - resolving stable [`shr_core::DiskId`] values from `/dev/disk/by-id`;
//! - propagating the smartctl process exit code into [`smart::SmartInfo`].

pub mod diskref;
pub mod identity;
pub mod inspector;
pub mod lsblk;
pub mod mdstat;
pub mod safety;
pub mod smart;

pub use diskref::{resolve_disk_ref, resolve_disk_refs, DiskRef, ResolvedDisk};
pub use identity::{fallback_disk_id, resolve_disk_path, ByIdIndex, IdentityError};
pub use inspector::{InspectError, Inspector, StaticInspector, SystemInspector};
pub use lsblk::{parse_lsblk, BlockDevice, LsblkOutput};
pub use mdstat::{parse_mdstat, MdArray, MdMember, MdStat, SyncStatus};
pub use safety::{
    is_system_disk, is_system_mountpoint, preflight_write_targets, system_mounts_on, PreflightTarget,
    WriteBlocker, WritePreflight, SYSTEM_MOUNTPOINTS,
};
pub use smart::{parse_smartctl, SmartInfo};
