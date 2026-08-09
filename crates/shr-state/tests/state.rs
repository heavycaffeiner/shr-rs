use shr_state::{
    ArrayState, StateBand, StateDisk, StateError, StateExpansion, StateFile, StateFilesystem, StatePartition,
    StateStore,
};
use tempfile::tempdir;

/// Every single-group test in this file wraps its `ArrayState` as the sole
/// entry in a `StateFile` -- `StateStore::save`/`load` now operate on the
/// whole multi-group container, not a bare `ArrayState`.
fn wrap(state: ArrayState) -> StateFile {
    StateFile::new(vec![state])
}

fn base_state(md_uuid: Option<String>, fs_uuid: Option<String>) -> ArrayState {
    ArrayState {
        name: "default".to_string(),
        mode: "shr".to_string(),
        created_at: "2026-07-25T12:00:00Z".to_string(),
        layout_version: 1,
        disks: vec![StateDisk {
            id: "ata-TEST-DISK-1".to_string(),
            size_bytes: 4_000_000_000_000,
            serial: Some("SERIAL-1".to_string()),
            model: Some("Model 1".to_string()),
            added_at: "2026-07-25T12:00:00Z".to_string(),
            partitions: vec![StatePartition {
                part_uuid: "00000000-0000-4000-8000-000000000001".to_string(),
                offset_bytes: 134217728,
                size_bytes: 3999865774080,
                band_index: 0,
            }],
        }],
        bands: vec![StateBand {
            index: 0,
            level: "raid1".to_string(),
            md_name: "md0".to_string(),
            md_uuid,
            member_partitions: vec!["00000000-0000-4000-8000-000000000001".to_string()],
            usable_bytes: 3999865774080,
            resize_pending: false,
            last_smart_reallocated: None,
            last_scrub: None,
            scrub_in_progress: false,
            pending_member_removal: None,
            reshape_priority: None,
        }],
        filesystem: StateFilesystem {
            fs_uuid,
            mount_point: "/mnt/data".to_string(),
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
        },
        expansion: StateExpansion::default(),
    }
}

#[test]
fn save_and_load_state_roundtrip() {
    let dir = tempdir().unwrap();
    let state_file = dir.path().join("state.toml");
    let store = StateStore::new(&state_file);

    assert!(!store.exists());
    assert_eq!(store.load().unwrap(), None);

    let sample_state = ArrayState {
        name: "default".to_string(),
        mode: "shr".to_string(),
        created_at: "2026-07-25T12:00:00Z".to_string(),
        layout_version: 1,
        disks: vec![StateDisk {
            id: "ata-TEST-DISK-1".to_string(),
            size_bytes: 4_000_000_000_000,
            serial: Some("SERIAL-1".to_string()),
            model: Some("Model 1".to_string()),
            added_at: "2026-07-25T12:00:00Z".to_string(),
            partitions: vec![StatePartition {
                part_uuid: "00000000-0000-4000-8000-000000000001".to_string(),
                offset_bytes: 134217728,
                size_bytes: 3999865774080,
                band_index: 0,
            }],
        }],
        bands: vec![StateBand {
            index: 0,
            level: "raid1".to_string(),
            md_name: "md0".to_string(),
            md_uuid: Some("12345678:abcdef01:23456789:0abcdef1".to_string()),
            member_partitions: vec!["00000000-0000-4000-8000-000000000001".to_string()],
            usable_bytes: 3999865774080,
            resize_pending: false,
            last_smart_reallocated: None,
            last_scrub: None,
            scrub_in_progress: false,
            pending_member_removal: None,
            reshape_priority: None,
        }],
        filesystem: StateFilesystem {
            fs_uuid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()),
            mount_point: "/mnt/data".to_string(),
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
        },
        expansion: StateExpansion::default(),
    };
    let sample = wrap(sample_state);

    store.save(&sample).unwrap();
    assert!(store.exists());

    let loaded = store.load().unwrap().expect("state should be loaded");
    assert_eq!(loaded, sample);
}

#[test]
fn save_and_load_multiple_groups_roundtrip() {
    // The core multi-group scenario: two independent named groups coexist
    // in the same state.toml and both survive a save/load cycle intact.
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let mut group_a = base_state(
        Some("aaaaaaaa:aaaaaaaa:aaaaaaaa:aaaaaaaa".to_string()),
        Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string()),
    );
    group_a.name = "shr1".to_string();

    let mut group_b = base_state(
        Some("bbbbbbbb:bbbbbbbb:bbbbbbbb:bbbbbbbb".to_string()),
        Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string()),
    );
    group_b.name = "shr2".to_string();
    group_b.disks[0].id = "ata-TEST-DISK-2".to_string();
    group_b.bands[0].md_name = "md1".to_string();

    let state = StateFile::new(vec![group_a, group_b]);
    store.save(&state).unwrap();

    let loaded = store.load().unwrap().expect("state should be loaded");
    assert_eq!(loaded.groups.len(), 2);
    assert_eq!(loaded, state);
    assert!(loaded.find("shr1").is_some());
    assert!(loaded.find("shr2").is_some());
}

#[test]
fn save_rejects_duplicate_group_names() {
    // Two groups sharing a name would make `expand --name`/status lookups
    // ambiguous -- reject up front, the same way a placeholder identifier is
    // rejected, rather than silently persisting an ambiguous file.
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let mut group_a = base_state(None, None);
    group_a.name = "shr1".to_string();
    let mut group_b = base_state(None, None);
    group_b.name = "shr1".to_string();
    group_b.disks[0].id = "ata-TEST-DISK-2".to_string();

    let state = StateFile::new(vec![group_a, group_b]);
    let err = store
        .save(&state)
        .expect_err("duplicate group names must be rejected");
    assert!(matches!(err, StateError::DuplicateGroupName(_)));
    assert_no_files_written(&dir, &store);
}

#[test]
fn load_migrates_a_pre_multigroup_state_toml_to_a_single_default_group() {
    // The literal on-disk shape shr-rs wrote before multi-group support
    // existed: a bare array's fields directly at the file's top level -- no
    // `name`, no `groups`/`schema_version` wrapper. An operator's existing
    // state.toml from before this change looks exactly like this and must
    // keep loading correctly: silently dropping it (or refusing to load it)
    // would be a real-world data-loss bug on upgrade, not just a test
    // failure.
    let dir = tempdir().unwrap();
    let state_file = dir.path().join("state.toml");
    let legacy_toml = r#"
mode = "shr"
created_at = "2026-07-25T12:00:00Z"
layout_version = 1

[[disks]]
id = "ata-TEST-DISK-1"
size_bytes = 4000000000000
serial = "SERIAL-1"
model = "Model 1"
added_at = "2026-07-25T12:00:00Z"

[[disks.partitions]]
part_uuid = "00000000-0000-4000-8000-000000000001"
offset_bytes = 134217728
size_bytes = 3999865774080
band_index = 0

[[bands]]
index = 0
level = "raid1"
md_name = "md0"
md_uuid = "12345678:abcdef01:23456789:0abcdef1"
member_partitions = ["00000000-0000-4000-8000-000000000001"]
usable_bytes = 3999865774080

[filesystem]
fs_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
mount_point = "/mnt/data"
vg_name = "shr_vg"
lv_name = "data"
compression = "zstd:3"
"#;
    std::fs::write(&state_file, legacy_toml).unwrap();
    let store = StateStore::new(&state_file);

    let loaded = store.load().unwrap().expect("legacy state.toml must still load");

    assert_eq!(
        loaded.groups.len(),
        1,
        "the old single array must migrate to exactly one group"
    );
    let group = &loaded.groups[0];
    assert_eq!(
        group.name, "default",
        "a legacy array with no name must migrate to the default name"
    );
    assert_eq!(group.mode, "shr");
    assert_eq!(group.layout_version, 1);
    assert_eq!(group.disks.len(), 1);
    assert_eq!(group.disks[0].id, "ata-TEST-DISK-1");
    assert_eq!(group.disks[0].serial.as_deref(), Some("SERIAL-1"));
    assert_eq!(group.bands.len(), 1);
    assert_eq!(group.bands[0].md_name, "md0");
    assert_eq!(
        group.bands[0].md_uuid.as_deref(),
        Some("12345678:abcdef01:23456789:0abcdef1")
    );
    assert_eq!(
        group.filesystem.fs_uuid.as_deref(),
        Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
    );
    assert!(
        !group.expansion.in_progress,
        "an omitted [expansion] table must default, not fail to parse"
    );
}

#[test]
fn load_a_v2_state_toml_written_before_e11_with_no_new_disks_field_still_loads() {
    // The literal on-disk shape a older build wrote for an in-progress
    // expansion: `[groups.expansion]` has `in_progress`/`checkpoint` but no
    // `new_disks` key at all (it didn't exist yet). Must still load --
    // treated as "nothing recorded to resume", not a parse failure.
    let dir = tempdir().unwrap();
    let state_file = dir.path().join("state.toml");
    let pre_e11_toml = r#"
schema_version = 2

[[groups]]
name = "default"
mode = "shr"
created_at = "2026-07-25T12:00:00Z"
layout_version = 1

[[groups.disks]]
id = "ata-TEST-DISK-1"
size_bytes = 4000000000000
serial = "SERIAL-1"
model = "Model 1"
added_at = "2026-07-25T12:00:00Z"

[[groups.disks.partitions]]
part_uuid = "00000000-0000-4000-8000-000000000001"
offset_bytes = 134217728
size_bytes = 3999865774080
band_index = 0

[[groups.bands]]
index = 0
level = "raid1"
md_name = "md0"
md_uuid = "12345678:abcdef01:23456789:0abcdef1"
member_partitions = ["00000000-0000-4000-8000-000000000001"]
usable_bytes = 3999865774080

[groups.filesystem]
fs_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
mount_point = "/mnt/data"
vg_name = "shr_vg"
lv_name = "data"
compression = "zstd:3"

[groups.expansion]
in_progress = true

[groups.expansion.checkpoint]
step_index = 1
resumable = false
description = "expansion starting: 2 step(s) planned"
"#;
    std::fs::write(&state_file, pre_e11_toml).unwrap();
    let store = StateStore::new(&state_file);

    let loaded = store
        .load()
        .unwrap()
        .expect("older v2 state.toml must still load");

    let group = &loaded.groups[0];
    assert!(group.expansion.in_progress);
    assert_eq!(
        group.expansion.new_disks,
        Vec::new(),
        "an omitted new_disks key must default to empty, not fail to parse"
    );
    assert!(
        !group.expansion.checkpoint.as_ref().unwrap().resumable,
        "a older checkpoint has no plan to resume from and must stay non-resumable"
    );
    assert_eq!(
        group.bands[0].last_smart_reallocated, None,
        "an omitted last_smart_reallocated key must default to None, not fail to parse"
    );
    assert_eq!(
        group.bands[0].last_scrub, None,
        "an omitted last_scrub key must default to None, not fail to parse"
    );
    assert!(
        !group.bands[0].scrub_in_progress,
        "an omitted scrub_in_progress key must default to false, not fail to parse"
    );
}

fn assert_no_files_written(dir: &tempfile::TempDir, store: &StateStore) {
    assert!(!store.exists(), "real state file should not be written");
    let tmp_path = store.path().with_extension("tmp");
    assert!(!tmp_path.exists(), "no .tmp sibling should be left behind");
    // Also ensure the directory itself has no stray entries at all.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(
        entries.is_empty(),
        "expected no files in state dir after rejected save, found: {entries:?}"
    );
}

#[test]
fn save_rejects_legacy_placeholder_md_uuid() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = base_state(Some("00000000:00000000:00000000:00000001".to_string()), None);

    let err = store
        .save(&wrap(state))
        .expect_err("placeholder md_uuid should be rejected");
    assert!(matches!(err, StateError::PlaceholderIdentifier(_)));
    assert_no_files_written(&dir, &store);
}

#[test]
fn save_rejects_placeholder_md_uuid_variant_band_index() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = base_state(Some("00000000:00000000:00000000:00000007".to_string()), None);

    let err = store
        .save(&wrap(state))
        .expect_err("placeholder md_uuid variant should be rejected");
    assert!(matches!(err, StateError::PlaceholderIdentifier(_)));
    assert_no_files_written(&dir, &store);
}

#[test]
fn save_rejects_legacy_placeholder_fs_uuid() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = base_state(None, Some("00000000-0000-4000-8000-000000000001".to_string()));

    let err = store
        .save(&wrap(state))
        .expect_err("placeholder fs_uuid should be rejected");
    assert!(matches!(err, StateError::PlaceholderIdentifier(_)));
    assert_no_files_written(&dir, &store);
}

#[test]
fn save_rejects_all_zero_fs_uuid() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = base_state(None, Some("00000000-0000-0000-0000-000000000000".to_string()));

    let err = store
        .save(&wrap(state))
        .expect_err("all-zero fs_uuid should be rejected");
    assert!(matches!(err, StateError::PlaceholderIdentifier(_)));
    assert_no_files_written(&dir, &store);
}

#[test]
fn save_rejects_md_uuid_with_zero_groups_in_a_different_arrangement() {
    // An earlier review finding: a position-specific detector (only groups 0-2
    // hardcoded zero, matching exactly the historical bug) would miss a
    // future variant that zeroes a different combination of groups. This
    // must be caught by an order-independent "3+ of 4 groups are all-zero"
    // rule instead.
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = base_state(Some("00000000:00000000:00000001:00000000".to_string()), None);

    let err = store
        .save(&wrap(state))
        .expect_err("md_uuid with 3 zero groups in a different arrangement should be rejected");
    assert!(matches!(err, StateError::PlaceholderIdentifier(_)));
    assert_no_files_written(&dir, &store);
}

#[test]
fn save_rejects_malformed_md_uuid() {
    // Not merely "looks like the old placeholder" -- an empty string or
    // garbage is never a valid MD_UUID and must never be persisted either.
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    for bad in ["", "unknown", "12345678:abcdef01:23456789"] {
        let state = base_state(Some(bad.to_string()), None);
        let result = store.save(&wrap(state));
        assert!(result.is_err(), "malformed md_uuid `{bad}` should be rejected");
        assert!(matches!(
            result.unwrap_err(),
            StateError::PlaceholderIdentifier(_)
        ));
        assert_no_files_written(&dir, &store);
    }
}

#[test]
fn save_rejects_malformed_fs_uuid() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    for bad in ["", "not-a-uuid", "aaaa-bbbb-cccc-dddd"] {
        let state = base_state(None, Some(bad.to_string()));
        let result = store.save(&wrap(state));
        assert!(result.is_err(), "malformed fs_uuid `{bad}` should be rejected");
        assert!(matches!(
            result.unwrap_err(),
            StateError::PlaceholderIdentifier(_)
        ));
        assert_no_files_written(&dir, &store);
    }
}

#[test]
fn save_accepts_real_looking_uuids() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = wrap(base_state(
        Some("a3f29ce4:8422234f:1b2c3d4e:5f6a7b8c".to_string()),
        Some("a3f29ce4-8422-4f1b-8c3d-4e5f6a7b8c9d".to_string()),
    ));

    store.save(&state).expect("real-looking UUIDs should be accepted");
    assert!(store.exists());

    let loaded = store.load().unwrap().expect("state should be loaded");
    assert_eq!(loaded, state);
}

#[cfg(unix)]
#[test]
fn save_sets_file_permissions_to_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));
    let state = base_state(None, None);

    store.save(&wrap(state)).unwrap();

    let mode = std::fs::metadata(store.path()).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "state.toml may contain by-id/UUID identity data; must not be group/world readable"
    );
}

#[test]
fn load_returns_a_clear_error_not_a_panic_on_a_corrupted_file() {
    let dir = tempdir().unwrap();
    let state_file = dir.path().join("state.toml");
    // Not written via `save` -- simulates disk corruption / a truncated write
    // that a crash interrupted before this crate's own atomic-replace logic
    // could apply.
    std::fs::write(&state_file, b"this is not valid toml {{{").unwrap();
    let store = StateStore::new(&state_file);

    let err = store
        .load()
        .expect_err("corrupted state.toml must not parse successfully");
    assert!(matches!(err, StateError::Deserialize(_)));
}

#[test]
fn save_accepts_none_identifiers() {
    let dir = tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state.toml"));

    let state = wrap(base_state(None, None));

    store
        .save(&state)
        .expect("None identifiers should always be accepted");
    assert!(store.exists());

    let loaded = store.load().unwrap().expect("state should be loaded");
    assert_eq!(loaded, state);
}
