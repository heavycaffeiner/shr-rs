//! RAID levels and redundancy modes, with the policy that maps a member count
//! to a level.

use serde::{Deserialize, Serialize};

/// The mdadm RAID levels shr-rs uses. RAID0/10 are intentionally excluded —
/// every band must tolerate at least one disk loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RaidLevel {
    Raid1,
    Raid5,
    Raid6,
}

impl RaidLevel {
    /// Minimum member count for which this level is defined.
    pub fn min_members(self) -> usize {
        match self {
            RaidLevel::Raid1 => 2,
            RaidLevel::Raid5 => 3,
            RaidLevel::Raid6 => 4,
        }
    }

    /// Number of members whose worth of capacity actually carries user data,
    /// for a band with `n` members. The remainder is redundancy overhead.
    ///
    /// - RAID1: a single copy is usable regardless of the number of mirrors.
    /// - RAID5: `n - 1` (one member's worth is parity).
    /// - RAID6: `n - 2` (two members' worth is parity).
    pub fn data_members(self, n: usize) -> usize {
        match self {
            RaidLevel::Raid1 => 1,
            RaidLevel::Raid5 => n.saturating_sub(1),
            RaidLevel::Raid6 => n.saturating_sub(2),
        }
    }

    /// How many simultaneous member losses this level survives.
    pub fn fault_tolerance(self) -> u8 {
        match self {
            RaidLevel::Raid1 => 1,
            RaidLevel::Raid5 => 1,
            RaidLevel::Raid6 => 2,
        }
    }

    /// Redundancy rank for ordering promotions (RAID1 < RAID5 < RAID6). A
    /// higher rank must never be replaced by a lower one (no downgrades).
    pub fn rank(self) -> u8 {
        match self {
            RaidLevel::Raid1 => 1,
            RaidLevel::Raid5 => 2,
            RaidLevel::Raid6 => 3,
        }
    }
}

/// The redundancy policy fixed at array-creation time. SHR and SHR-2 never
/// convert into one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedundancyMode {
    /// 1-fault tolerant. Starts at 2 disks (RAID1), promotes to RAID5 at 3+.
    Shr,
    /// 2-fault tolerant, strict: RAID6 only, minimum 4 disks per band. No
    /// RAID1 fallback for upper bands.
    Shr2,
}

impl RedundancyMode {
    /// Minimum disks to create an array in this mode.
    pub fn min_initial_disks(self) -> usize {
        match self {
            RedundancyMode::Shr => 2,
            RedundancyMode::Shr2 => 4,
        }
    }

    /// Minimum members for any single band in this mode.
    pub fn min_members_per_band(self) -> usize {
        match self {
            RedundancyMode::Shr => 2,
            RedundancyMode::Shr2 => 4,
        }
    }

    /// The RAID level this mode assigns to a band with `n` participating
    /// members, or `None` if `n` is too small to be redundant in this mode.
    pub fn pick_level(self, n: usize) -> Option<RaidLevel> {
        match (self, n) {
            (RedundancyMode::Shr, 2) => Some(RaidLevel::Raid1),
            (RedundancyMode::Shr, n) if n >= 3 => Some(RaidLevel::Raid5),
            (RedundancyMode::Shr2, n) if n >= 4 => Some(RaidLevel::Raid6),
            _ => None,
        }
    }

    /// The guaranteed number of disk losses the whole array survives.
    pub fn fault_tolerance(self) -> u8 {
        match self {
            RedundancyMode::Shr => 1,
            RedundancyMode::Shr2 => 2,
        }
    }
}
