//! Write-safety gates: never touch OS/root disks, never write without a stable
//! id, and surface preflight blockers before any executor exists.
//!
//! Policy (Phase 4 Stage A, aligns with the M1 recommendation):
//! **data disks only** — any disk that backs `/`, `/boot`, `/boot/efi` (or
//! common variants) is permanently excluded from create/expand targets.

use serde::Serialize;

use crate::identity::ByIdIndex;
use crate::lsblk::{BlockDevice, LsblkOutput};

/// Mountpoints that mark a disk as holding the operating system.
pub const SYSTEM_MOUNTPOINTS: &[&str] = &["/", "/boot", "/boot/efi", "/boot/EFI", "/efi", "/boot/grub"];

/// Hard reasons a disk must not be written by shr-rs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WriteBlocker {
    SystemDisk {
        name: String,
        id: String,
        mounts: Vec<String>,
    },
    NoStableId {
        name: String,
    },
    NotFound {
        reference: String,
    },
    SizeUnknown {
        name: String,
    },
    /// An earlier review finding: a disk already carrying partitions or a
    /// filesystem signature was previously only ever a `warnings` entry,
    /// never anything that actually stopped `create`/`expand` from wiping
    /// it. Blocks by default; `force_content` opts out (a used disk being
    /// intentionally repurposed is a legitimate case).
    HasContent {
        name: String,
    },
}

impl std::fmt::Display for WriteBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteBlocker::SystemDisk { name, id, mounts } => write!(
                f,
                "disk `{name}` ({id}) holds system mounts {}; refused (data-disk-only policy)",
                mounts.join(", ")
            ),
            WriteBlocker::NoStableId { name } => {
                write!(f, "disk `{name}` has no stable /dev/disk/by-id name; refused")
            }
            WriteBlocker::NotFound { reference } => {
                write!(f, "disk reference `{reference}` not found")
            }
            WriteBlocker::SizeUnknown { name } => {
                write!(f, "disk `{name}` has unknown size; refused")
            }
            WriteBlocker::HasContent { name } => write!(
                f,
                "disk `{name}` already has partitions or a filesystem signature; refused \
                 unless explicitly overridden -- each frontend has its own way to do that"
            ),
        }
    }
}

/// Collect system mountpoints present on this device tree.
pub fn system_mounts_on(dev: &BlockDevice) -> Vec<String> {
    let mut out = Vec::new();
    collect_system_mounts(dev, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect_system_mounts(dev: &BlockDevice, out: &mut Vec<String>) {
    if let Some(mp) = dev.mountpoint.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        if is_system_mountpoint(mp) {
            out.push(mp.to_string());
        }
    }
    for child in &dev.children {
        collect_system_mounts(child, out);
    }
}

pub fn is_system_mountpoint(mp: &str) -> bool {
    SYSTEM_MOUNTPOINTS.contains(&mp)
}

pub fn is_system_disk(dev: &BlockDevice) -> bool {
    !system_mounts_on(dev).is_empty()
}

/// Preflight report for a proposed write set (create/expand targets).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WritePreflight {
    pub ok: bool,
    pub blockers: Vec<WriteBlocker>,
    pub warnings: Vec<String>,
    pub targets: Vec<PreflightTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreflightTarget {
    pub kernel_name: String,
    pub id: Option<String>,
    pub size: Option<u64>,
    pub system_disk: bool,
    pub system_mounts: Vec<String>,
    pub has_content: bool,
}

/// Evaluate create/expand targets by kernel name against inventory + by-id.
/// `force_content`: if false (the default posture), a disk with existing
/// partitions/filesystem signature is a hard blocker, not just a warning
/// (an earlier review) -- pass true only when the operator has explicitly
/// opted in to reusing a disk that already has content.
pub fn preflight_write_targets(
    kernel_names: &[String],
    lsblk: &LsblkOutput,
    index: &ByIdIndex,
    force_content: bool,
) -> WritePreflight {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let mut targets = Vec::new();

    for name in kernel_names {
        let name = name.trim().trim_start_matches("/dev/");
        let Some(dev) = lsblk.disks().find(|d| d.name == name) else {
            blockers.push(WriteBlocker::NotFound {
                reference: name.to_string(),
            });
            continue;
        };
        let mounts = system_mounts_on(dev);
        let system = !mounts.is_empty();
        let id = index.id_for_kernel(name).map(|d| d.as_str().to_string());
        if system {
            blockers.push(WriteBlocker::SystemDisk {
                name: name.to_string(),
                id: id.clone().unwrap_or_else(|| format!("kernel-{name}")),
                mounts: mounts.clone(),
            });
        }
        if id.is_none() {
            blockers.push(WriteBlocker::NoStableId {
                name: name.to_string(),
            });
        }
        if dev.size.is_none() {
            blockers.push(WriteBlocker::SizeUnknown {
                name: name.to_string(),
            });
        }
        if dev.has_content() {
            warnings.push(format!(
                "disk `{name}` already has partitions or a filesystem signature"
            ));
            if !force_content {
                blockers.push(WriteBlocker::HasContent {
                    name: name.to_string(),
                });
            }
        }
        targets.push(PreflightTarget {
            kernel_name: name.to_string(),
            id,
            size: dev.size,
            system_disk: system,
            system_mounts: mounts,
            has_content: dev.has_content(),
        });
    }

    WritePreflight {
        ok: blockers.is_empty(),
        blockers,
        warnings,
        targets,
    }
}
