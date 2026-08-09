//! User-facing disk references that always resolve to a stable [`DiskId`].
//!
//! CLI/TUI accept several spellings; the domain layer only ever sees `DiskId`.

use shr_core::{Disk, DiskId};

use crate::identity::{ByIdIndex, IdentityError};
use crate::lsblk::LsblkOutput;
use crate::safety::{system_mounts_on, WriteBlocker};

/// How the user named a disk on the command line or in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskRef {
    /// Kernel path or name: `/dev/sdf`, `sdf`.
    Path(String),
    /// Full by-id name: `ata-ST8000VN004-2M2101_WKD1ABCD`.
    ById(String),
    /// Serial fragment: `WKD1ABCD`.
    Serial(String),
}

impl DiskRef {
    /// Parse a free-form user string into a [`DiskRef`].
    ///
    /// Heuristics:
    /// - starts with `/dev/` or looks like a kernel name (`sdX`, `nvme*`, …)
    ///   → [`DiskRef::Path`]
    /// - contains a by-id style prefix (`ata-`, `nvme-`, `wwn-`, `scsi-`, …)
    ///   → [`DiskRef::ById`]
    /// - otherwise → [`DiskRef::Serial`]
    pub fn parse(raw: &str) -> Self {
        let s = raw.trim();
        if s.starts_with("/dev/") || looks_like_kernel_name(s) {
            return DiskRef::Path(s.to_string());
        }
        if looks_like_by_id(s) {
            return DiskRef::ById(s.to_string());
        }
        DiskRef::Serial(s.to_string())
    }

    pub fn as_raw(&self) -> &str {
        match self {
            DiskRef::Path(s) | DiskRef::ById(s) | DiskRef::Serial(s) => s,
        }
    }
}

fn looks_like_kernel_name(s: &str) -> bool {
    let s = s.trim_start_matches("/dev/");
    if s.is_empty() || s.contains('/') {
        return false;
    }
    // sdX, hdX, vdX, xvdX, nvmeXnY, mmcblkN, loopN, dm-N
    let bytes = s.as_bytes();
    if s.starts_with("nvme") || s.starts_with("mmcblk") || s.starts_with("loop") || s.starts_with("dm-") {
        return true;
    }
    if bytes.len() >= 3 {
        let prefix = &s[..2];
        if matches!(prefix, "sd" | "hd" | "vd") && s[2..].chars().all(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
        if s.starts_with("xvd") && s[3..].chars().all(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
    }
    false
}

fn looks_like_by_id(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "ata-",
        "nvme-",
        "wwn-",
        "scsi-",
        "usb-",
        "mmc-",
        "md-uuid-",
        "lvm-pv-uuid-",
    ];
    PREFIXES.iter().any(|p| s.starts_with(p))
}

/// A disk successfully resolved to stable identity + live metadata.
#[derive(Debug, Clone)]
pub struct ResolvedDisk {
    pub reference: DiskRef,
    pub kernel_name: String,
    pub id: DiskId,
    pub size_bytes: u64,
    pub serial: String,
    pub model: String,
    pub system_mounts: Vec<String>,
    pub has_content: bool,
}

impl ResolvedDisk {
    pub fn is_system_disk(&self) -> bool {
        !self.system_mounts.is_empty()
    }

    /// Convert to the pure planner [`Disk`] (stable id only).
    pub fn to_planner_disk(&self) -> Disk {
        Disk::new(self.id.clone(), self.size_bytes).with_meta(self.serial.clone(), self.model.clone())
    }

    /// Hard blockers that must prevent any write/plan-against-system use.
    pub fn write_blockers(&self) -> Vec<WriteBlocker> {
        let mut out = Vec::new();
        if self.is_system_disk() {
            out.push(WriteBlocker::SystemDisk {
                name: self.kernel_name.clone(),
                id: self.id.as_str().to_string(),
                mounts: self.system_mounts.clone(),
            });
        }
        out
    }
}

/// Resolve one user reference against live inventory + by-id index.
pub fn resolve_disk_ref(
    reference: &DiskRef,
    lsblk: &LsblkOutput,
    index: &ByIdIndex,
) -> Result<ResolvedDisk, IdentityError> {
    let kernel = match reference {
        DiskRef::Path(p) => {
            let name = p.trim().trim_start_matches("/dev/");
            if lsblk.disks().any(|d| d.name == name) {
                name.to_string()
            } else {
                return Err(IdentityError::NotFound {
                    reference: reference.as_raw().to_string(),
                });
            }
        }
        DiskRef::ById(id) => index
            .kernel_for_id_name(id)
            .ok_or_else(|| IdentityError::NotFound {
                reference: id.clone(),
            })?
            .to_string(),
        DiskRef::Serial(serial) => index.kernel_for_serial_fragment(serial)?.to_string(),
    };

    let dev = lsblk
        .disks()
        .find(|d| d.name == kernel)
        .ok_or_else(|| IdentityError::NotFound {
            reference: reference.as_raw().to_string(),
        })?;

    let id = index
        .id_for_kernel(&kernel)
        .cloned()
        .ok_or_else(|| IdentityError::NoStableId { name: kernel.clone() })?;

    let size_bytes = dev.size.ok_or_else(|| IdentityError::NotFound {
        // Reuse NotFound-ish messaging via a dedicated size error would be nicer,
        // but size-unknown is rare and treated as unusable for planning.
        reference: format!("{kernel} (size unknown)"),
    })?;

    Ok(ResolvedDisk {
        reference: reference.clone(),
        kernel_name: kernel,
        id,
        size_bytes,
        serial: dev.serial_trimmed().unwrap_or_default(),
        model: dev.model_trimmed().unwrap_or_default(),
        system_mounts: system_mounts_on(dev),
        has_content: dev.has_content(),
    })
}

/// Resolve many refs; fail on the first error.
pub fn resolve_disk_refs(
    refs: &[DiskRef],
    lsblk: &LsblkOutput,
    index: &ByIdIndex,
) -> Result<Vec<ResolvedDisk>, IdentityError> {
    refs.iter().map(|r| resolve_disk_ref(r, lsblk, index)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsblk::parse_lsblk;

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"sda","size":4000000000000,"type":"disk","model":"WD","serial":"A",
       "children":[{"name":"sda1","type":"part","mountpoint":"/"}]},
      {"name":"sdb","size":8000000000000,"type":"disk","model":"ST","serial":"WKD1ABCD"},
      {"name":"nvme0n1","size":512000000000,"type":"disk","model":"SSD","serial":"S1"}
    ]}"#;

    fn index() -> ByIdIndex {
        let mut idx = ByIdIndex::empty();
        idx.insert("sda", "ata-WD_A");
        idx.insert("sdb", "ata-ST8000VN004_WKD1ABCD");
        idx.insert("nvme0n1", "nvme-Samsung_S1");
        idx
    }

    #[test]
    fn parse_heuristics() {
        assert!(matches!(DiskRef::parse("/dev/sdb"), DiskRef::Path(_)));
        assert!(matches!(DiskRef::parse("sdb"), DiskRef::Path(_)));
        assert!(matches!(DiskRef::parse("nvme0n1"), DiskRef::Path(_)));
        assert!(matches!(
            DiskRef::parse("ata-ST8000VN004_WKD1ABCD"),
            DiskRef::ById(_)
        ));
        assert!(matches!(DiskRef::parse("WKD1ABCD"), DiskRef::Serial(_)));
    }

    #[test]
    fn resolve_path_byid_serial() {
        let lsblk = parse_lsblk(LSBLK).unwrap();
        let idx = index();
        let by_path = resolve_disk_ref(&DiskRef::parse("sdb"), &lsblk, &idx).unwrap();
        assert_eq!(by_path.kernel_name, "sdb");
        assert_eq!(by_path.id.as_str(), "ata-ST8000VN004_WKD1ABCD");

        let by_id = resolve_disk_ref(&DiskRef::parse("ata-ST8000VN004_WKD1ABCD"), &lsblk, &idx).unwrap();
        assert_eq!(by_id.kernel_name, "sdb");

        let by_serial = resolve_disk_ref(&DiskRef::parse("WKD1ABCD"), &lsblk, &idx).unwrap();
        assert_eq!(by_serial.kernel_name, "sdb");
    }
}
