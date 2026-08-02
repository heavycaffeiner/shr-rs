//! Parse `lsblk -J -b -o NAME,SIZE,TYPE,MODEL,SERIAL,ROTA,TRAN,PARTUUID,FSTYPE,MOUNTPOINT`.
//!
//! `lsblk` JSON is inconsistent about number encoding across versions (sizes
//! may be a JSON number or a string), so sizes are parsed leniently.

use serde::{Deserialize, Deserializer};

/// Top-level `lsblk -J` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LsblkOutput {
    #[serde(default)]
    pub blockdevices: Vec<BlockDevice>,
}

/// One node in the `lsblk` tree (a disk, partition, LVM device, …).
#[derive(Debug, Clone, Deserialize)]
pub struct BlockDevice {
    pub name: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub size: Option<u64>,
    #[serde(rename = "type", default)]
    pub dtype: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub rota: Option<bool>,
    #[serde(default)]
    pub tran: Option<String>,
    #[serde(default)]
    pub partuuid: Option<String>,
    #[serde(default)]
    pub fstype: Option<String>,
    #[serde(default)]
    pub mountpoint: Option<String>,
    /// Partition-table type (`gpt`, `dos`, …) if the disk is partitioned.
    #[serde(default)]
    pub pttype: Option<String>,
    #[serde(default)]
    pub children: Vec<BlockDevice>,
}

impl BlockDevice {
    pub fn is_disk(&self) -> bool {
        self.dtype == "disk" || self.dtype == "loop"
    }

    /// Trimmed model string, if present and non-empty.
    pub fn model_trimmed(&self) -> Option<String> {
        self.model
            .as_ref()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
    }

    /// Trimmed serial string, if present and non-empty.
    pub fn serial_trimmed(&self) -> Option<String> {
        self.serial
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Does this device (or any descendant) hold data or structure that should
    /// warn before reuse — a filesystem/RAID signature, OR merely an existing
    /// partition layout (a child of type `part` / with a PARTUUID, even if
    /// unformatted)? Used to warn before wiping a disk.
    pub fn has_content(&self) -> bool {
        let own_fs = self
            .fstype
            .as_deref()
            .map(str::trim)
            .is_some_and(|f| !f.is_empty());
        let has_ptable = self
            .pttype
            .as_deref()
            .map(str::trim)
            .is_some_and(|p| !p.is_empty());
        own_fs
            || has_ptable
            || self
                .children
                .iter()
                .any(|c| c.dtype == "part" || c.partuuid.is_some() || c.has_content())
    }
}

impl LsblkOutput {
    /// All top-level `type == "disk"` devices.
    pub fn disks(&self) -> impl Iterator<Item = &BlockDevice> {
        self.blockdevices.iter().filter(|d| d.is_disk())
    }
}

/// Parse `lsblk -J` JSON.
pub fn parse_lsblk(json: &str) -> Result<LsblkOutput, serde_json::Error> {
    serde_json::from_str(json)
}

/// Accept a JSON number, a numeric string, or null → `Option<u64>`.
fn de_opt_u64<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde_json::Value;
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_u64()),
        Some(Value::String(s)) => Ok(s.trim().parse::<u64>().ok()),
        Some(_) => Ok(None),
    }
}
