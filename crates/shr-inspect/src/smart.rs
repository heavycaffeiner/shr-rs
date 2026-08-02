//! Parse `smartctl -j` (JSON) into the health signals shr-rs cares about.
//! smartctl's schema differs between ATA and NVMe devices, so this navigates
//! the JSON leniently rather than deriving a rigid struct.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SmartInfo {
    /// `smart_status.passed` — overall SMART health assessment.
    pub passed: Option<bool>,
    /// `smartctl.exit_status` — its high bits (>=8) flag real device problems.
    pub exit_status: Option<u64>,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    /// ATA attribute 5 (Reallocated_Sector_Ct) raw value.
    pub reallocated_sectors: Option<u64>,
    /// ATA attribute 197 (Current_Pending_Sector) raw value.
    pub pending_sectors: Option<u64>,
    /// ATA attribute 198 (Offline_Uncorrectable) raw value.
    pub uncorrectable_sectors: Option<u64>,
    /// NVMe `critical_warning` bitmask.
    pub nvme_critical_warning: Option<u64>,
    /// NVMe `media_errors` count.
    pub nvme_media_errors: Option<u64>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

/// smartctl exit-status bits >= 3 (value >= 8) indicate SMART/device problems
/// (failing, prefail, error-log entries, self-test failures) rather than mere
/// CLI/usage errors.
const SMARTCTL_PROBLEM_BITS: u64 = 0xF8;

impl SmartInfo {
    /// A definite "needs attention" signal: failed health assessment, problem
    /// exit bits, or any pending/reallocated/uncorrectable/media error.
    pub fn has_warning(&self) -> bool {
        self.passed == Some(false)
            || self.exit_status.unwrap_or(0) & SMARTCTL_PROBLEM_BITS != 0
            || self.pending_sectors.unwrap_or(0) > 0
            || self.reallocated_sectors.unwrap_or(0) > 0
            || self.uncorrectable_sectors.unwrap_or(0) > 0
            || self.nvme_critical_warning.unwrap_or(0) > 0
            || self.nvme_media_errors.unwrap_or(0) > 0
    }

    /// smartctl couldn't actually read SMART: exit bits 0-2 mean a command-line
    /// error, a failed device open, or a failed SMART command. The health it
    /// reports (if any) is not trustworthy.
    pub fn inspection_failed(&self) -> bool {
        self.exit_status.map(|s| s & 0x07 != 0).unwrap_or(false)
    }

    /// True when SMART was not (successfully) inspected — the read failed, or
    /// there is no health verdict and no signals at all. Distinct from
    /// "inspected and healthy".
    pub fn is_unknown(&self) -> bool {
        // A definite problem is known-bad, never "unknown".
        if self.has_warning() {
            return false;
        }
        if self.inspection_failed() {
            return true;
        }
        self.passed.is_none()
            && self.temperature_c.is_none()
            && self.power_on_hours.is_none()
            && self.pending_sectors.is_none()
            && self.reallocated_sectors.is_none()
            && self.uncorrectable_sectors.is_none()
            && self.nvme_critical_warning.is_none()
            && self.nvme_media_errors.is_none()
    }
}

/// Parse `smartctl -j` output.
pub fn parse_smartctl(json: &str) -> Result<SmartInfo, serde_json::Error> {
    let v: Value = serde_json::from_str(json)?;

    let mut info = SmartInfo {
        passed: v.pointer("/smart_status/passed").and_then(Value::as_bool),
        exit_status: v.pointer("/smartctl/exit_status").and_then(Value::as_u64),
        temperature_c: v.pointer("/temperature/current").and_then(Value::as_i64),
        power_on_hours: v.pointer("/power_on_time/hours").and_then(Value::as_u64),
        model: v
            .get("model_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        serial: v
            .get("serial_number")
            .and_then(Value::as_str)
            .map(str::to_string),
        nvme_critical_warning: v
            .pointer("/nvme_smart_health_information_log/critical_warning")
            .and_then(Value::as_u64),
        nvme_media_errors: v
            .pointer("/nvme_smart_health_information_log/media_errors")
            .and_then(Value::as_u64),
        reallocated_sectors: None,
        pending_sectors: None,
        uncorrectable_sectors: None,
    };

    if let Some(table) = v
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
    {
        for attr in table {
            let id = attr.get("id").and_then(Value::as_u64);
            let raw = attr.pointer("/raw/value").and_then(Value::as_u64);
            match id {
                Some(5) => info.reallocated_sectors = raw,
                Some(197) => info.pending_sectors = raw,
                Some(198) => info.uncorrectable_sectors = raw,
                _ => {}
            }
        }
    }

    // NVMe fallback for temperature if the ATA path was absent.
    if info.temperature_c.is_none() {
        info.temperature_c = v
            .pointer("/nvme_smart_health_information_log/temperature")
            .and_then(Value::as_i64);
    }

    Ok(info)
}
