//! Disk identity and the disk value type used by the planner.

use serde::{Deserialize, Serialize};

/// A stable disk identifier, derived from a `/dev/disk/by-id` symlink name
/// (e.g. `ata-WDC_WD40EFRX-68N32N0_WD-WCC7K1ABCDEF`).
///
/// In this pure-domain crate it is an opaque, ordered newtype. Turning a live
/// `/dev/sdX` path into a `DiskId` (canonicalizing `by-id`, preferring
/// `ata-*`/`nvme-*` over `wwn-*`) is the job of `shr-inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DiskId(String);

impl DiskId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Short, human-friendly tail — usually the serial fragment after the last
    /// underscore, e.g. `WD-WCC7K1ABCDEF`.
    pub fn short(&self) -> &str {
        self.0.rsplit('_').next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for DiskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for DiskId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for DiskId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// A physical disk as the planner sees it. The current `/dev/sdX` path is
/// deliberately absent: it is unstable and never persisted or planned against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub id: DiskId,
    pub size_bytes: u64,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub model: String,
}

impl Disk {
    pub fn new(id: impl Into<DiskId>, size_bytes: u64) -> Self {
        Self {
            id: id.into(),
            size_bytes,
            serial: String::new(),
            model: String::new(),
        }
    }

    pub fn with_meta(mut self, serial: impl Into<String>, model: impl Into<String>) -> Self {
        self.serial = serial.into();
        self.model = model.into();
        self
    }
}
