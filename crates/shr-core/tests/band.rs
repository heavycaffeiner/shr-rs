//! `RedundantBand` invariant tests. Both the `new` path and the (sealed)
//! deserialization path (which routes through `from_parts`) reject
//! structurally non-redundant bands (too few members for the level itself,
//! zero size, duplicate member). Only `new` additionally checks
//! mode-consistency (member count against the mode's floor, level against
//! what the mode would pick) -- deserialization cannot, since a per-band
//! payload carries no mode. See band.rs's module doc comment for why, and
//! `deserialize_accepts_a_mode_inconsistent_band` below for what that gap
//! actually means in practice.

use shr_core::{BandError, DiskId, RaidLevel, RedundancyMode, RedundantBand};

fn ids(n: usize) -> Vec<DiskId> {
    (0..n).map(|i| DiskId::new(format!("d{i}"))).collect()
}

#[test]
fn new_rejects_duplicate_member() {
    let dup = vec![DiskId::new("d0"), DiskId::new("d0")];
    let err =
        RedundantBand::new(0, 0, 1000, dup, RaidLevel::Raid1, RedundancyMode::Shr).unwrap_err();
    assert_eq!(err, BandError::DuplicateMember);
}

#[test]
fn new_rejects_too_few_members_for_mode() {
    let err =
        RedundantBand::new(0, 0, 1000, ids(2), RaidLevel::Raid6, RedundancyMode::Shr2).unwrap_err();
    assert!(matches!(
        err,
        BandError::InsufficientForMode { got: 2, min: 4 }
    ));
}

#[test]
fn new_rejects_level_mismatch() {
    // 3 disks in SHR would be RAID5, not RAID1.
    let err =
        RedundantBand::new(0, 0, 1000, ids(3), RaidLevel::Raid1, RedundancyMode::Shr).unwrap_err();
    assert!(matches!(
        err,
        BandError::LevelMismatch {
            expected: RaidLevel::Raid5,
            got: RaidLevel::Raid1
        }
    ));
}

#[test]
fn new_rejects_zero_size() {
    let err =
        RedundantBand::new(0, 0, 0, ids(2), RaidLevel::Raid1, RedundancyMode::Shr).unwrap_err();
    assert_eq!(err, BandError::ZeroSize);
}

#[test]
fn deserialize_rejects_band_below_the_levels_own_floor() {
    // The previous name here ("...underpopulated_band") implied
    // broader coverage than this actually proves. This checks ONLY
    // RaidLevel::Raid5's own structural floor (min_members()==3), via
    // `from_parts` -- deserialization has no `mode` field to check
    // mode-consistency against (see band.rs's module doc comment and the
    // `deserialize_accepts_a_mode_inconsistent_band` test below for what
    // this does NOT cover).
    let json = r#"{"band_index":0,"offset":0,"size":1000,"members":["d0"],"level":"raid5"}"#;
    let parsed: Result<RedundantBand, _> = serde_json::from_str(json);
    assert!(
        parsed.is_err(),
        "forged 1-member RAID5 (below RAID5's own 3-member floor) should fail to deserialize"
    );
}

#[test]
fn deserialize_accepts_a_mode_inconsistent_band() {
    // Documents the known, deliberate gap left by the constructor
    // split above -- this is NOT a defect test (no fix makes this reject).
    //
    // A 3-member RAID5 band is structurally valid on its own (RAID5's own
    // floor is 3, satisfied) and so deserializes cleanly, even though a
    // 3-member RAID5 band is illegal inside a strict SHR-2 array (SHR-2
    // mandates RAID6, min 4 members -- see RedundancyMode::pick_level).
    // Mode is a property of the whole array, not of one band, so a per-band
    // JSON/TOML payload has no mode field to check this against; that check
    // lives one layer up, in `expansion::validate_snapshot`, which runs
    // before any expansion decision is made against a loaded snapshot.
    let json = r#"{"band_index":0,"offset":0,"size":1000,"members":["d0","d1","d2"],"level":"raid5"}"#;
    let parsed: Result<RedundantBand, _> = serde_json::from_str(json);
    assert!(
        parsed.is_ok(),
        "structurally-valid RAID5 band deserializes regardless of any array mode: {parsed:?}"
    );
}

#[test]
fn deserialize_rejects_zero_size_band() {
    let json = r#"{"band_index":0,"offset":0,"size":0,"members":["d0","d1"],"level":"raid1"}"#;
    let parsed: Result<RedundantBand, _> = serde_json::from_str(json);
    assert!(parsed.is_err(), "zero-size band should fail to deserialize");
}

#[test]
fn valid_band_round_trips_through_serde() {
    let band =
        RedundantBand::new(0, 0, 1000, ids(3), RaidLevel::Raid5, RedundancyMode::Shr).unwrap();
    let json = serde_json::to_string(&band).unwrap();
    let back: RedundantBand = serde_json::from_str(&json).unwrap();
    assert_eq!(band, back);
}
