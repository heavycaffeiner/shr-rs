//! Stable disk identity resolution via `/dev/disk/by-id`.
//!
//! Kernel names (`sda`, `nvme0n1`) are unstable across reboots. Every durable
//! reference and write target must go through a [`DiskId`] built from a by-id
//! symlink name. This module builds an index of those links and picks a
//! preferred name when several point at the same device.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use shr_core::DiskId;
use thiserror::Error;

/// Why a by-id scan or lookup failed.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to read {path}: {source}")]
    ReadDir { path: String, source: std::io::Error },
    #[error("failed to resolve symlink {path}: {source}")]
    Resolve { path: String, source: std::io::Error },
    #[error("no stable by-id name for kernel device `{name}`")]
    NoStableId { name: String },
    #[error("ambiguous serial match `{serial}` → {matches:?}")]
    AmbiguousSerial { serial: String, matches: Vec<String> },
    #[error("disk reference `{reference}` not found")]
    NotFound { reference: String },
}

/// Bidirectional map between kernel device names and preferred [`DiskId`]s.
#[derive(Debug, Clone, Default)]
pub struct ByIdIndex {
    /// Kernel name (`sda`) → preferred stable id.
    kernel_to_id: HashMap<String, DiskId>,
    /// Stable id string → kernel name.
    id_to_kernel: HashMap<String, String>,
    /// All known by-id names for a kernel device (including non-preferred).
    aliases: HashMap<String, Vec<String>>,
}

impl ByIdIndex {
    /// Empty index — used by pure unit tests that inject mappings.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Insert a known mapping. Prefer higher-quality ids (see [`prefer_id`]).
    pub fn insert(&mut self, kernel_name: impl Into<String>, id_name: impl Into<String>) {
        let kernel = strip_dev_prefix(&kernel_name.into());
        let id_name = id_name.into();
        // Partition links (…-partN) identify partitions, not whole disks.
        if is_partition_link(&id_name) {
            return;
        }
        self.aliases
            .entry(kernel.clone())
            .or_default()
            .push(id_name.clone());
        let candidate = DiskId::new(id_name);
        match self.kernel_to_id.get(&kernel) {
            Some(existing) if !prefer_id(candidate.as_str(), existing.as_str()) => {}
            Some(existing) => {
                let old = existing.as_str().to_string();
                self.id_to_kernel.remove(&old);
                self.kernel_to_id.insert(kernel.clone(), candidate.clone());
                self.id_to_kernel.insert(candidate.as_str().to_string(), kernel);
            }
            None => {
                self.kernel_to_id.insert(kernel.clone(), candidate.clone());
                self.id_to_kernel.insert(candidate.as_str().to_string(), kernel);
            }
        }
    }

    /// Scan a by-id directory (normally `/dev/disk/by-id`).
    ///
    /// Each symlink is resolved to its target basename (e.g. `../../sda` →
    /// `sda`) and recorded. Safe to call on non-Linux hosts: missing directory
    /// yields an empty index, not a hard failure, so Windows unit tests remain
    /// portable. Explicit I/O errors on an existing directory still propagate.
    pub fn scan_dir(path: impl AsRef<Path>) -> Result<Self, IdentityError> {
        let path = path.as_ref();
        let mut index = Self::empty();
        let entries = match fs::read_dir(path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(index),
            Err(e) => {
                return Err(IdentityError::ReadDir {
                    path: path.display().to_string(),
                    source: e,
                })
            }
        };
        for entry in entries {
            let entry = entry.map_err(|e| IdentityError::ReadDir {
                path: path.display().to_string(),
                source: e,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let link = entry.path();
            let target = fs::read_link(&link).map_err(|e| IdentityError::Resolve {
                path: link.display().to_string(),
                source: e,
            })?;
            let kernel = target
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.display().to_string());
            index.insert(kernel, name.into_owned());
        }
        Ok(index)
    }

    /// Convenience: scan the host's `/dev/disk/by-id`.
    pub fn scan_system() -> Result<Self, IdentityError> {
        Self::scan_dir("/dev/disk/by-id")
    }

    pub fn id_for_kernel(&self, kernel_name: &str) -> Option<&DiskId> {
        self.kernel_to_id.get(&strip_dev_prefix(kernel_name))
    }

    /// Require a stable id for `kernel_name`, or error.
    pub fn require_id(&self, kernel_name: &str) -> Result<&DiskId, IdentityError> {
        self.id_for_kernel(kernel_name)
            .ok_or_else(|| IdentityError::NoStableId {
                name: strip_dev_prefix(kernel_name),
            })
    }

    /// Look up by exact by-id name (any alias, not only the preferred one).
    pub fn kernel_for_id_name(&self, id_name: &str) -> Option<&str> {
        if let Some(k) = self.id_to_kernel.get(id_name) {
            return Some(k.as_str());
        }
        for (kernel, aliases) in &self.aliases {
            if aliases.iter().any(|a| a == id_name) {
                return Some(kernel.as_str());
            }
        }
        None
    }

    /// Match a serial fragment against known disks' preferred ids and aliases.
    ///
    /// Returns the unique kernel name, or an ambiguity / not-found error.
    pub fn kernel_for_serial_fragment(&self, fragment: &str) -> Result<&str, IdentityError> {
        let frag = fragment.trim();
        if frag.is_empty() {
            return Err(IdentityError::NotFound {
                reference: fragment.to_string(),
            });
        }
        let mut hits: Vec<&str> = Vec::new();
        for (kernel, aliases) in &self.aliases {
            let preferred = self.kernel_to_id.get(kernel).map(|d| d.as_str());
            let match_alias = aliases.iter().any(|a| serial_matches(a, frag));
            let match_pref = preferred.is_some_and(|p| serial_matches(p, frag));
            if (match_alias || match_pref) && !hits.contains(&kernel.as_str()) {
                hits.push(kernel.as_str());
            }
        }
        match hits.as_slice() {
            [one] => Ok(*one),
            [] => Err(IdentityError::NotFound {
                reference: fragment.to_string(),
            }),
            many => Err(IdentityError::AmbiguousSerial {
                serial: fragment.to_string(),
                matches: many.iter().map(|s| (*s).to_string()).collect(),
            }),
        }
    }
}

/// Prefer non-`wwn-*` names; among equals, shorter / more specific wins by
/// simple lexical stability so results are deterministic.
fn prefer_id(candidate: &str, existing: &str) -> bool {
    let cand_wwn = candidate.starts_with("wwn-");
    let exist_wwn = existing.starts_with("wwn-");
    match (cand_wwn, exist_wwn) {
        (false, true) => true,
        (true, false) => false,
        _ => candidate < existing,
    }
}

fn is_partition_link(name: &str) -> bool {
    // by-id partition links look like `ata-MODEL_SERIAL-part1`.
    name.contains("-part")
}

fn strip_dev_prefix(name: &str) -> String {
    name.trim()
        .trim_start_matches("/dev/")
        .trim_start_matches('/')
        .to_string()
}

fn serial_matches(id_name: &str, fragment: &str) -> bool {
    if id_name.eq_ignore_ascii_case(fragment) {
        return true;
    }
    // Common pattern: `ata-MODEL_SERIAL` — serial is after the last `_`.
    if let Some(tail) = id_name.rsplit('_').next() {
        if tail.eq_ignore_ascii_case(fragment) {
            return true;
        }
    }
    id_name
        .to_ascii_lowercase()
        .contains(&fragment.to_ascii_lowercase())
}

/// Build a synthetic fallback [`DiskId`] when by-id is unavailable.
///
/// Prefer never using this for durable state or writes — only for read-only
/// displays when the host has no by-id links (containers, odd VMs).
pub fn fallback_disk_id(kernel_name: &str) -> DiskId {
    DiskId::new(format!("kernel-{}", strip_dev_prefix(kernel_name)))
}

/// Current path for a [`DiskId`] via by-id (Linux only).
pub fn resolve_disk_path(id: &DiskId) -> PathBuf {
    Path::new("/dev/disk/by-id").join(id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_ata_over_wwn() {
        let mut idx = ByIdIndex::empty();
        idx.insert("sda", "wwn-0x5000c500");
        idx.insert("sda", "ata-WDC_WD40_SERIAL");
        assert_eq!(idx.id_for_kernel("sda").unwrap().as_str(), "ata-WDC_WD40_SERIAL");
    }

    #[test]
    fn ignores_partition_links() {
        let mut idx = ByIdIndex::empty();
        idx.insert("sda", "ata-WDC_WD40_SERIAL-part1");
        assert!(idx.id_for_kernel("sda").is_none());
        idx.insert("sda", "ata-WDC_WD40_SERIAL");
        assert_eq!(idx.id_for_kernel("sda").unwrap().as_str(), "ata-WDC_WD40_SERIAL");
    }

    #[test]
    fn serial_fragment_unique_and_ambiguous() {
        let mut idx = ByIdIndex::empty();
        idx.insert("sda", "ata-WDC_WD40_SERIALA");
        idx.insert("sdb", "ata-WDC_WD40_SERIALB");
        assert_eq!(idx.kernel_for_serial_fragment("SERIALA").unwrap(), "sda");
        assert!(matches!(
            idx.kernel_for_serial_fragment("SERIAL"),
            Err(IdentityError::AmbiguousSerial { .. })
        ));
    }

    #[test]
    fn strip_dev_and_fallback() {
        assert_eq!(strip_dev_prefix("/dev/sda"), "sda");
        assert_eq!(fallback_disk_id("sda").as_str(), "kernel-sda");
    }
}
