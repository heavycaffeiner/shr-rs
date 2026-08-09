//! [`RedundantBand`] — a horizontal slice across disks, backed by one mdadm
//! array. Its smart constructors are the load-bearing invariant of the whole
//! system: a band cannot exist unless it is genuinely redundant.
//!
//! Two constructors, two different guarantees -- do not conflate them:
//!
//! - [`RedundantBand::from_parts`] (and the `serde` deserialization path,
//!   which routes through it) checks only the *structural*, mode-independent
//!   invariants: non-zero size, enough members for the level itself, no
//!   duplicate member. It has no `mode` parameter, so it CANNOT and does NOT
//!   check mode-consistency -- a 3-member RAID5 band deserializes cleanly
//!   even inside a strict SHR-2 array, where RAID5 is not a legal level.
//!   (from an earlier audit: a per-band JSON/TOML payload has no mode field to check
//!   against in the first place -- mode is a property of the whole array,
//!   not of one band -- so this is a structural limit, not an oversight.)
//! - [`RedundantBand::new`] additionally checks mode-consistency: member
//!   count against the mode's per-band floor, and the requested `level`
//!   against what the mode would pick for that member count.
//!
//! Mode-consistency of a *loaded* layout is instead enforced one layer up,
//! by `expansion::validate_snapshot`, which runs before any expansion
//! decision is made against a snapshot built via `from_parts`. See that
//! function's doc comment and `tests/band.rs` for what each layer actually
//! covers.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::disk::DiskId;
use crate::raid::{RaidLevel, RedundancyMode};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BandError {
    #[error("band has {got} members but mode requires at least {min}")]
    InsufficientForMode { got: usize, min: usize },
    #[error("{level:?} needs at least {min} members, got {got}")]
    InsufficientForLevel {
        got: usize,
        min: usize,
        level: RaidLevel,
    },
    #[error("no redundant RAID level is available for {0} members in this mode")]
    NoLevelAvailable(usize),
    #[error("level mismatch: expected {expected:?} for this member count, got {got:?}")]
    LevelMismatch { expected: RaidLevel, got: RaidLevel },
    #[error("a disk appears twice in one band (single point of failure)")]
    DuplicateMember,
    #[error("band size must be greater than zero")]
    ZeroSize,
}

/// A redundant band. Fields are private and immutable; every construction path
/// funnels through validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RedundantBandRaw")]
pub struct RedundantBand {
    band_index: u8,
    offset: u64,
    size: u64,
    members: Vec<DiskId>,
    level: RaidLevel,
}

/// Deserialization shadow: raw fields are converted through
/// [`RedundantBand::from_parts`] so JSON/TOML cannot forge an invalid band.
#[derive(Deserialize)]
struct RedundantBandRaw {
    band_index: u8,
    offset: u64,
    size: u64,
    members: Vec<DiskId>,
    level: RaidLevel,
}

impl TryFrom<RedundantBandRaw> for RedundantBand {
    type Error = BandError;
    fn try_from(r: RedundantBandRaw) -> Result<Self, Self::Error> {
        RedundantBand::from_parts(r.band_index, r.offset, r.size, r.members, r.level)
    }
}

impl RedundantBand {
    /// Structural (mode-independent) validation shared by every construction
    /// path: non-zero size, enough members for the level, no duplicate member.
    pub fn from_parts(
        band_index: u8,
        offset: u64,
        size: u64,
        members: Vec<DiskId>,
        level: RaidLevel,
    ) -> Result<Self, BandError> {
        if size == 0 {
            return Err(BandError::ZeroSize);
        }
        if members.len() < level.min_members() {
            return Err(BandError::InsufficientForLevel {
                got: members.len(),
                min: level.min_members(),
                level,
            });
        }
        let unique: HashSet<&DiskId> = members.iter().collect();
        if unique.len() != members.len() {
            return Err(BandError::DuplicateMember);
        }
        Ok(Self {
            band_index,
            offset,
            size,
            members,
            level,
        })
    }

    /// Build a band under a specific redundancy mode, verifying:
    /// 1. member count ≥ the mode's per-band minimum,
    /// 2. a redundant level exists for that member count,
    /// 3. the requested `level` matches what the mode would pick.
    ///
    /// It then applies the structural invariants of
    /// [`from_parts`](Self::from_parts).
    pub fn new(
        band_index: u8,
        offset: u64,
        size: u64,
        members: Vec<DiskId>,
        level: RaidLevel,
        mode: RedundancyMode,
    ) -> Result<Self, BandError> {
        let min = mode.min_members_per_band();
        if members.len() < min {
            return Err(BandError::InsufficientForMode {
                got: members.len(),
                min,
            });
        }
        let expected = mode
            .pick_level(members.len())
            .ok_or(BandError::NoLevelAvailable(members.len()))?;
        if expected != level {
            return Err(BandError::LevelMismatch { expected, got: level });
        }
        Self::from_parts(band_index, offset, size, members, level)
    }

    pub fn band_index(&self) -> u8 {
        self.band_index
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Per-member slice length in bytes (all members contribute equally).
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Exclusive end of this band's extent on each member disk.
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.size)
    }

    pub fn members(&self) -> &[DiskId] {
        &self.members
    }

    pub fn level(&self) -> RaidLevel {
        self.level
    }

    /// Usable (data-bearing) capacity of this band.
    pub fn usable_bytes(&self) -> u64 {
        self.size * self.level.data_members(self.members.len()) as u64
    }

    /// Total raw capacity this band consumes across all members.
    pub fn raw_bytes(&self) -> u64 {
        self.size * self.members.len() as u64
    }

    pub fn contains(&self, id: &DiskId) -> bool {
        self.members.iter().any(|m| m == id)
    }

    /// Would this band still be recoverable after losing `victims`?
    pub fn is_recoverable_without(&self, victims: &[DiskId]) -> bool {
        let lost = self.members.iter().filter(|m| victims.contains(m)).count();
        lost as u8 <= self.level.fault_tolerance()
    }
}
