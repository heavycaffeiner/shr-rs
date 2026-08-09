use shr_state::conf::{
    find_orphaned_scrub_units, is_shr_rs_owned_unit, remove_owned_unit_file, scrub_unit_paths, write_fstab,
    write_health_check_timer_unit, write_mdadm_conf, write_scrub_timer_units, write_snapshot_timer_unit,
    write_throttle_timer_unit,
};
use shr_state::{
    ArrayState, StateBand, StateDisk, StateExpansion, StateFile, StateFilesystem, StatePartition,
};
use tempfile::tempdir;

fn group(name: &str, disk_id: &str, bands: Vec<StateBand>, fs_uuid: Option<String>) -> ArrayState {
    ArrayState {
        name: name.to_string(),
        mode: "shr".to_string(),
        created_at: "2026-07-25T12:00:00Z".to_string(),
        layout_version: 1,
        disks: vec![StateDisk {
            id: disk_id.to_string(),
            size_bytes: 4_000_000_000_000,
            serial: None,
            model: None,
            added_at: "2026-07-25T12:00:00Z".to_string(),
            partitions: vec![StatePartition {
                part_uuid: "00000000-0000-4000-8000-000000000001".to_string(),
                offset_bytes: 134217728,
                size_bytes: 3999865774080,
                band_index: 0,
            }],
        }],
        bands,
        filesystem: StateFilesystem {
            fs_uuid,
            mount_point: format!("/mnt/{name}"),
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
        },
        expansion: StateExpansion::default(),
    }
}

fn state_with_bands(bands: Vec<StateBand>, fs_uuid: Option<String>) -> StateFile {
    let mut g = group("default", "ata-TEST-DISK-1", bands, fs_uuid);
    g.filesystem.mount_point = "/mnt/data".to_string();
    StateFile::new(vec![g])
}

fn band(index: u8, md_name: &str, md_uuid: Option<&str>) -> StateBand {
    StateBand {
        index,
        level: "raid5".to_string(),
        md_name: md_name.to_string(),
        md_uuid: md_uuid.map(|s| s.to_string()),
        member_partitions: vec![],
        usable_bytes: 1,
        resize_pending: false,
        last_smart_reallocated: None,
        last_scrub: None,
        scrub_in_progress: false,
        pending_member_removal: None,
        reshape_priority: None,
    }
}

#[test]
fn mdadm_conf_contains_an_array_line_with_the_real_md_uuid_per_band() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mdadm.conf");
    let state = state_with_bands(
        vec![
            band(0, "md0", Some("a3f29ce4:8422234f:1b2c3d4e:5f6a7b8c")),
            band(1, "md1", Some("11111111:22222222:33333333:44444444")),
        ],
        None,
    );

    write_mdadm_conf(&path, &state).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert!(content.contains("ARRAY /dev/md0 UUID=a3f29ce4:8422234f:1b2c3d4e:5f6a7b8c"));
    assert!(content.contains("ARRAY /dev/md1 UUID=11111111:22222222:33333333:44444444"));
}

#[test]
fn mdadm_conf_skips_bands_without_a_real_md_uuid_yet() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mdadm.conf");
    let state = state_with_bands(vec![band(0, "md0", None)], None);

    write_mdadm_conf(&path, &state).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert!(!content.contains("md0"));
}

#[test]
fn fstab_line_uses_the_btrfs_uuid_and_never_a_dev_sdx_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fstab");
    let state = state_with_bands(vec![], Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()));

    write_fstab(&path, &state).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert!(content.contains(
        "UUID=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee /mnt/data btrfs compress=zstd:3,subvol=@,nofail,x-systemd.device-timeout=10 0 0"
    ));
    assert!(!content.contains("/dev/sd"));
    assert!(!content.contains("/dev/vd"));
}

#[test]
fn fstab_write_is_a_noop_when_the_btrfs_uuid_is_not_known_yet() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fstab");
    let state = state_with_bands(vec![], None);

    write_fstab(&path, &state).unwrap();

    assert!(
        !path.exists() || std::fs::read_to_string(&path).unwrap().trim().is_empty(),
        "no fs_uuid means nothing valid can be written yet"
    );
}

/// Regression: destroying the LAST group must actually empty the
/// managed block in `/etc/fstab`, not leave the destroyed group's mount
/// line behind forever. Real-guest repro: after `destroy` reported success,
/// `/etc/fstab`'s managed block still contained the old `UUID=...
/// /mnt/shr_data btrfs ...` line, and `findmnt --verify` flagged it
/// unreachable on every subsequent boot. The bug was `write_fstab`
/// returning early on `lines.is_empty()` -- true both for "fresh system,
/// nothing written yet" (legitimate no-op) and "last group just destroyed"
/// (must rewrite to empty) -- without distinguishing the two. A realistic
/// unrelated pre-existing line (the root filesystem's own fstab entry) is
/// included and must survive both writes untouched.
#[test]
fn destroying_the_last_group_empties_the_fstab_managed_block_instead_of_leaving_it_stale() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fstab");
    std::fs::write(
        &path,
        "UUID=11111111-1111-1111-1111-111111111111 / ext4 defaults 0 1\n",
    )
    .unwrap();

    let with_group = state_with_bands(vec![], Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()));
    write_fstab(&path, &with_group).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("UUID=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee /mnt/data"),
        "the group's mount line must be written before destroy: {content}"
    );

    // The group is now destroyed -- `state.toml` has no groups left.
    let empty = StateFile::new(vec![]);
    write_fstab(&path, &empty).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert!(
        !content.contains("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
        "the destroyed group's fs UUID must not survive: {content}"
    );
    assert!(
        !content.contains("/mnt/data"),
        "the destroyed group's mount point must not survive: {content}"
    );
    assert!(
        content.contains("# >>> shr-rs managed >>>") && content.contains("# <<< shr-rs managed <<<"),
        "the managed block markers themselves must remain (emptied, not deleted): {content}"
    );
    assert!(
        content.contains("UUID=11111111-1111-1111-1111-111111111111 / ext4 defaults 0 1"),
        "an unrelated, non-shr-rs-owned fstab entry (e.g. the root filesystem) must survive untouched: {content}"
    );
}

/// The guard's ORIGINAL intent must still hold -- on a system that has
/// never had a group with a known filesystem UUID, `write_fstab` must stay
/// a true no-op and never create the file at all. Only "the file already
/// exists" (i.e. a managed block was written before and now needs
/// emptying) should trigger a write with empty content.
#[test]
fn fstab_still_does_not_create_the_file_when_nothing_was_ever_written() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fstab");
    let state = state_with_bands(vec![], None);

    write_fstab(&path, &state).unwrap();

    assert!(
        !path.exists(),
        "no group has ever had a known fs UUID -- the file must not be created"
    );
}

#[test]
fn rewriting_mdadm_conf_replaces_the_existing_managed_block_instead_of_duplicating() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mdadm.conf");
    let first = state_with_bands(
        vec![band(0, "md0", Some("11111111:11111111:11111111:11111111"))],
        None,
    );
    let second = state_with_bands(
        vec![band(0, "md0", Some("22222222:22222222:22222222:22222222"))],
        None,
    );

    write_mdadm_conf(&path, &first).unwrap();
    write_mdadm_conf(&path, &second).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert_eq!(
        content.matches("# >>> shr-rs managed >>>").count(),
        1,
        "must not accumulate a new managed block on every write"
    );
    assert!(!content.contains("11111111:11111111:11111111:11111111"));
    assert!(content.contains("22222222:22222222:22222222:22222222"));
}

#[test]
fn rewriting_mdadm_conf_preserves_unrelated_existing_content_outside_the_markers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mdadm.conf");
    std::fs::write(&path, "ARRAY /dev/md9 UUID=99999999:99999999:99999999:99999999\n").unwrap();
    let state = state_with_bands(
        vec![band(0, "md0", Some("11111111:11111111:11111111:11111111"))],
        None,
    );

    write_mdadm_conf(&path, &state).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert!(
        content.contains("ARRAY /dev/md9 UUID=99999999:99999999:99999999:99999999"),
        "a pre-existing, unrelated array entry must survive"
    );
    assert!(content.contains("11111111:11111111:11111111:11111111"));
}

#[test]
fn a_file_with_non_utf8_content_returns_an_error_instead_of_being_treated_as_empty() {
    // An earlier review finding: `read_to_string(path).unwrap_or_default()`
    // silently treats ANY read failure -- including invalid UTF-8, which a
    // hand-edited /etc/fstab full of legitimate non-shr-rs entries can
    // easily contain (a Latin-1 comment is enough) -- as "file is empty".
    // The next write then replaces the WHOLE file with only shr-rs's own
    // managed block, destroying every pre-existing entry. A read error
    // must propagate, never be silently downgraded to "nothing here".
    let dir = tempdir().unwrap();
    let path = dir.path().join("fstab");
    // 0xFF is never valid UTF-8 in any position.
    std::fs::write(&path, [0xFFu8, b'\n', b'U', b'U', b'I', b'D', b'=', b'x']).unwrap();
    let state = state_with_bands(vec![], Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()));

    let err = write_fstab(&path, &state)
        .expect_err("a non-UTF-8 existing file must be reported, not silently treated as empty");
    assert!(matches!(err, shr_state::StateError::Io(_)));

    // And the original bytes must survive untouched -- the whole point of
    // refusing to guess is to never destroy content shr-rs can't parse.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes, vec![0xFFu8, b'\n', b'U', b'U', b'I', b'D', b'=', b'x']);
}

#[test]
fn mismatched_managed_markers_in_an_existing_file_return_an_error_not_a_silent_overwrite() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mdadm.conf");
    // Only the begin marker is present -- a human or another tool corrupted
    // the file. Guessing what to do here (append anyway? overwrite from the
    // begin marker to EOF?) could silently destroy content someone else
    // owns; refuse instead.
    std::fs::write(&path, "# >>> shr-rs managed >>>\nARRAY /dev/md0 UUID=x\n").unwrap();
    let state = state_with_bands(
        vec![band(0, "md0", Some("11111111:11111111:11111111:11111111"))],
        None,
    );

    let err = write_mdadm_conf(&path, &state)
        .expect_err("mismatched markers must be reported, not silently resolved");
    assert!(matches!(err, shr_state::StateError::ManagedBlock(_)));
}

// --- Multi-group correctness (Phase 4 multi-group support) ---

#[test]
fn creating_a_second_group_leaves_the_first_groups_mdadm_conf_and_fstab_entries_intact() {
    // The correctness trap this change exists to close: these writers used
    // to take a single `ArrayState`, so regenerating the managed block for
    // group B alone would silently DELETE group A's `ARRAY` line and fstab
    // mount -- after a reboot, group A simply would not come back. Taking
    // the whole `StateFile` and flattening across every group is what
    // prevents that; this test proves it holds once a real second group is
    // added, not just that the plumbing compiles.
    //
    // Also covers the sibling case: destroying ONE of TWO groups
    // (group B) must leave group A's entries intact and only remove B's --
    // this is what the earlier fix's `!path.exists()` guard must NOT interfere
    // with, since `lines` stays non-empty here (unlike destroying the LAST
    // group, covered separately by
    // `destroying_the_last_group_empties_the_fstab_managed_block_instead_of_leaving_it_stale`).
    let dir = tempdir().unwrap();
    let mdadm_path = dir.path().join("mdadm.conf");
    let fstab_path = dir.path().join("fstab");

    let group_a = group(
        "shr1",
        "ata-DISK-A",
        vec![band(0, "md0", Some("aaaaaaaa:aaaaaaaa:aaaaaaaa:aaaaaaaa"))],
        Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string()),
    );
    let only_a = StateFile::new(vec![group_a.clone()]);
    write_mdadm_conf(&mdadm_path, &only_a).unwrap();
    write_fstab(&fstab_path, &only_a).unwrap();

    let group_b = group(
        "shr2",
        "ata-DISK-B",
        vec![band(1, "md1", Some("bbbbbbbb:bbbbbbbb:bbbbbbbb:bbbbbbbb"))],
        Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string()),
    );
    let both = StateFile::new(vec![group_a, group_b]);
    write_mdadm_conf(&mdadm_path, &both).unwrap();
    write_fstab(&fstab_path, &both).unwrap();

    let mdadm_content = std::fs::read_to_string(&mdadm_path).unwrap();
    assert!(
        mdadm_content.contains("ARRAY /dev/md0 UUID=aaaaaaaa:aaaaaaaa:aaaaaaaa:aaaaaaaa"),
        "group shr1's ARRAY line must survive creating shr2: {mdadm_content}"
    );
    assert!(
        mdadm_content.contains("ARRAY /dev/md1 UUID=bbbbbbbb:bbbbbbbb:bbbbbbbb:bbbbbbbb"),
        "group shr2's ARRAY line must be present too: {mdadm_content}"
    );

    let fstab_content = std::fs::read_to_string(&fstab_path).unwrap();
    assert!(
        fstab_content.contains("UUID=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa /mnt/shr1"),
        "group shr1's fstab mount must survive creating shr2: {fstab_content}"
    );
    assert!(
        fstab_content.contains("UUID=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb /mnt/shr2"),
        "group shr2's fstab mount must be present too: {fstab_content}"
    );

    // Now destroy shr2 -- back down to just shr1 (`only_a`, built earlier).
    // `lines` stays non-empty here (shr1 still has a UUID), so this must go
    // through the ordinary rewrite path regardless of the earlier fix.
    write_mdadm_conf(&mdadm_path, &only_a).unwrap();
    write_fstab(&fstab_path, &only_a).unwrap();

    let mdadm_content = std::fs::read_to_string(&mdadm_path).unwrap();
    assert!(
        mdadm_content.contains("ARRAY /dev/md0 UUID=aaaaaaaa:aaaaaaaa:aaaaaaaa:aaaaaaaa"),
        "group shr1's ARRAY line must survive destroying shr2: {mdadm_content}"
    );
    assert!(
        !mdadm_content.contains("bbbbbbbb:bbbbbbbb:bbbbbbbb:bbbbbbbb"),
        "destroyed group shr2's ARRAY line must be gone: {mdadm_content}"
    );

    let fstab_content = std::fs::read_to_string(&fstab_path).unwrap();
    assert!(
        fstab_content.contains("UUID=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa /mnt/shr1"),
        "group shr1's fstab mount must survive destroying shr2: {fstab_content}"
    );
    assert!(
        !fstab_content.contains("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
        "destroyed group shr2's fstab mount must be gone: {fstab_content}"
    );
}

// ---------------------------------------------------------------------
// Systemd scrub timer units
// ---------------------------------------------------------------------

/// A path deliberately unlike either real hardcoded candidate
/// (`/usr/bin/shr-rs`, `/usr/local/bin/shr-rs`) this project has actually
/// shipped and been burned by -- proves the generated unit uses whatever
/// `exe_path` the caller passed, not a baked-in guess.
fn test_exe_path() -> &'static std::path::Path {
    std::path::Path::new("/opt/shr-rs/bin/shr-rs")
}

#[test]
fn write_scrub_timer_units_writes_a_service_and_timer_named_after_the_group() {
    let dir = tempdir().unwrap();
    let state = state_with_bands(
        vec![band(0, "md0", Some("aaaaaaaa:aaaaaaaa:aaaaaaaa:aaaaaaaa"))],
        None,
    );

    let written = write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap();
    assert_eq!(written.len(), 2, "{written:?}");

    let service = std::fs::read_to_string(dir.path().join("shr-rs-scrub-default.service")).unwrap();
    assert!(
        service.contains("ExecStart=/opt/shr-rs/bin/shr-rs fs scrub start --name default"),
        "{service}"
    );
    assert!(!service.contains("/usr/bin/shr-rs"), "{service}");
    let timer = std::fs::read_to_string(dir.path().join("shr-rs-scrub-default.timer")).unwrap();
    assert!(timer.contains("OnCalendar=weekly"), "{timer}");
    assert!(timer.contains("WantedBy=timers.target"), "{timer}");
}

#[test]
fn write_scrub_timer_units_skips_a_group_with_no_bands() {
    let dir = tempdir().unwrap();
    let state = state_with_bands(vec![], None);

    let written = write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap();
    assert!(written.is_empty(), "{written:?}");
}

#[test]
fn write_scrub_timer_units_sanitizes_unsafe_characters_in_the_group_name() {
    let dir = tempdir().unwrap();
    let mut g = group("my group/2", "ata-TEST-DISK-1", vec![band(0, "md0", None)], None);
    g.filesystem.mount_point = "/mnt/data".to_string();
    let state = StateFile::new(vec![g]);

    let written = write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap();
    assert!(
        written
            .iter()
            .all(|p| !p.to_string_lossy().contains('/') || p.starts_with(dir.path())),
        "{written:?}"
    );
    assert!(
        dir.path().join("shr-rs-scrub-my_group_2.service").exists(),
        "{written:?}"
    );
}

/// The exact multi-group correctness trap `write_mdadm_conf`/`write_fstab`
/// had to be fixed for in Phase 4, applied to the scheduler: creating a
/// SECOND group's scrub schedule must never disturb the FIRST group's
/// already-written unit files -- because each group gets its own dedicated
/// files, not a shared one regenerated from whichever group was touched
/// last. Verified with 4 groups (the same scale the design's own
/// smoke-test principles call for, `SM-SCHED-2`), not just 2.
#[test]
fn write_scrub_timer_units_never_overwrites_another_groups_units_across_four_groups() {
    let dir = tempdir().unwrap();

    let groups: Vec<ArrayState> = (0..4)
        .map(|i| {
            group(
                &format!("shr{i}"),
                &format!("ata-DISK-{i}"),
                vec![band(
                    0,
                    &format!("md{i}"),
                    Some(&format!("{i}{i}{i}{i}{i}{i}{i}{i}:11111111:22222222:33333333")),
                )],
                None,
            )
        })
        .collect();

    // Register each group's units ONE AT A TIME, exactly like `create()`
    // would trigger scheduling for a newly created group -- if this
    // regenerated a shared file from only the group just added, every
    // earlier group's files would already be gone by the time this loop
    // finishes.
    for i in 0..groups.len() {
        let partial_state = StateFile::new(groups[..=i].to_vec());
        write_scrub_timer_units(dir.path(), &partial_state, test_exe_path()).unwrap();
    }

    for i in 0..4 {
        let service_path = dir.path().join(format!("shr-rs-scrub-shr{i}.service"));
        let timer_path = dir.path().join(format!("shr-rs-scrub-shr{i}.timer"));
        assert!(
            service_path.exists(),
            "group shr{i}'s service must survive every later group's registration"
        );
        assert!(
            timer_path.exists(),
            "group shr{i}'s timer must survive every later group's registration"
        );
        let service = std::fs::read_to_string(&service_path).unwrap();
        assert!(service.contains(&format!("--name shr{i}")), "{service}");
    }
}

#[test]
fn write_scrub_timer_units_is_idempotent_on_rewrite() {
    let dir = tempdir().unwrap();
    let state = state_with_bands(vec![band(0, "md0", None)], None);

    write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap();
    write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap();

    let service = std::fs::read_to_string(dir.path().join("shr-rs-scrub-default.service")).unwrap();
    assert_eq!(
        service.matches("ExecStart=").count(),
        1,
        "rewriting must replace, not duplicate: {service}"
    );
}

#[test]
fn write_throttle_timer_unit_writes_one_global_service_and_timer() {
    let dir = tempdir().unwrap();
    let written = write_throttle_timer_unit(dir.path(), test_exe_path()).unwrap();
    assert_eq!(written.len(), 2, "{written:?}");

    let service = std::fs::read_to_string(dir.path().join("shr-rs-throttle-tick.service")).unwrap();
    assert!(
        service.contains("ExecStart=/opt/shr-rs/bin/shr-rs internal reshape-throttle-tick"),
        "{service}"
    );
    assert!(!service.contains("/usr/bin/shr-rs"), "{service}");
    let timer = std::fs::read_to_string(dir.path().join("shr-rs-throttle-tick.timer")).unwrap();
    assert!(timer.contains("OnCalendar=*:0/2"), "{timer}");
}

#[test]
fn write_health_check_timer_unit_writes_one_global_service_and_timer() {
    let dir = tempdir().unwrap();
    let written = write_health_check_timer_unit(dir.path(), test_exe_path()).unwrap();
    assert_eq!(written.len(), 2, "{written:?}");

    let service = std::fs::read_to_string(dir.path().join("shr-rs-health-check.service")).unwrap();
    assert!(
        service.contains("ExecStart=/opt/shr-rs/bin/shr-rs internal health-check-tick"),
        "{service}"
    );
    assert!(!service.contains("/usr/bin/shr-rs"), "{service}");
    let timer = std::fs::read_to_string(dir.path().join("shr-rs-health-check.timer")).unwrap();
    assert!(timer.contains("OnCalendar=*:0/15"), "{timer}");
}

// -- `[snapshot]` policy-driven automation timer.

#[test]
fn write_snapshot_timer_unit_writes_one_global_service_and_timer_using_the_configured_schedule() {
    let dir = tempdir().unwrap();
    let written = write_snapshot_timer_unit(dir.path(), test_exe_path(), "daily").unwrap();
    assert_eq!(written.len(), 2, "{written:?}");

    let service = std::fs::read_to_string(dir.path().join("shr-rs-snapshot-auto.service")).unwrap();
    assert!(
        service.contains("ExecStart=/opt/shr-rs/bin/shr-rs internal snapshot-auto-tick"),
        "{service}"
    );
    assert!(!service.contains("/usr/bin/shr-rs"), "{service}");
    let timer = std::fs::read_to_string(dir.path().join("shr-rs-snapshot-auto.timer")).unwrap();
    // The policy's `schedule` value flows straight into `OnCalendar=` --
    // proven with a non-default value ("daily") so this can't pass by
    // coincidence against some other hardcoded default.
    assert!(timer.contains("OnCalendar=daily"), "{timer}");
}

#[test]
fn write_snapshot_timer_unit_honors_a_different_schedule_value() {
    let dir = tempdir().unwrap();
    write_snapshot_timer_unit(dir.path(), test_exe_path(), "weekly").unwrap();
    let timer = std::fs::read_to_string(dir.path().join("shr-rs-snapshot-auto.timer")).unwrap();
    assert!(timer.contains("OnCalendar=weekly"), "{timer}");
}

// -- Unit ownership marking + orphan detection/cleanup (real-guest
// repro: a destroyed group's scrub timer kept firing `fs scrub start
// --name <gone>` forever, because nothing could tell a shr-rs-generated
// unit apart from an operator's own hand-written one of the same name).

#[test]
fn every_generated_unit_kind_is_recognized_as_shr_rs_owned() {
    let dir = tempdir().unwrap();
    let state = state_with_bands(vec![band(0, "md0", None)], None);

    for path in write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap() {
        assert!(is_shr_rs_owned_unit(&path), "{path:?} must be recognized as ours");
    }
    for path in write_throttle_timer_unit(dir.path(), test_exe_path()).unwrap() {
        assert!(is_shr_rs_owned_unit(&path), "{path:?} must be recognized as ours");
    }
    for path in write_health_check_timer_unit(dir.path(), test_exe_path()).unwrap() {
        assert!(is_shr_rs_owned_unit(&path), "{path:?} must be recognized as ours");
    }
    for path in write_snapshot_timer_unit(dir.path(), test_exe_path(), "daily").unwrap() {
        assert!(is_shr_rs_owned_unit(&path), "{path:?} must be recognized as ours");
    }
}

#[test]
fn is_shr_rs_owned_unit_fails_closed_for_a_hand_written_file_and_a_missing_one() {
    let dir = tempdir().unwrap();
    let hand_written = dir.path().join("shr-rs-scrub-somegroup.service");
    std::fs::write(
        &hand_written,
        "[Unit]\nDescription=an operator wrote this by hand\n",
    )
    .unwrap();
    assert!(
        !is_shr_rs_owned_unit(&hand_written),
        "no marker present -- must not claim ownership"
    );

    let missing = dir.path().join("does-not-exist.service");
    assert!(
        !is_shr_rs_owned_unit(&missing),
        "a missing file must not be reported as owned"
    );
}

/// TDD (the test that would have caught the real-guest orphan): three
/// groups' scrub units exist on disk, but `state` (the CURRENT truth,
/// post-`destroy()`) only lists two of them -- `find_orphaned_scrub_units`
/// must report exactly the third group's own pair as `owned`, and must
/// never mention the two still-live groups' units at all.
#[test]
fn find_orphaned_scrub_units_reports_only_the_group_missing_from_state() {
    let dir = tempdir().unwrap();
    let all_three = StateFile::new(vec![
        group("grp-a", "ata-A", vec![band(0, "md0", None)], None),
        group("grp-b", "ata-B", vec![band(0, "md1", None)], None),
        group("grp-w1", "ata-C", vec![band(0, "md2", None)], None),
    ]);
    write_scrub_timer_units(dir.path(), &all_three, test_exe_path()).unwrap();

    // `grp-b` was destroyed -- state.toml now only knows about the other two.
    let after_destroy = StateFile::new(vec![
        group("grp-a", "ata-A", vec![band(0, "md0", None)], None),
        group("grp-w1", "ata-C", vec![band(0, "md2", None)], None),
    ]);

    let orphans = find_orphaned_scrub_units(dir.path(), &after_destroy).unwrap();
    let (b_service, b_timer) = scrub_unit_paths(dir.path(), "grp-b");
    let mut owned = orphans.owned.clone();
    owned.sort();
    let mut expected = vec![b_service, b_timer];
    expected.sort();
    assert_eq!(owned, expected, "{orphans:?}");
    assert!(orphans.unowned_lookalikes.is_empty(), "{orphans:?}");
}

/// Same real-guest scenario, but the "orphaned-looking" file is actually an
/// operator's own hand-written unit that happens to share the naming
/// convention -- it must be reported as `unowned_lookalikes` (warn-only)
/// and never `owned` (never a deletion candidate).
#[test]
fn find_orphaned_scrub_units_never_treats_a_hand_written_lookalike_as_owned() {
    let dir = tempdir().unwrap();
    let (service_path, _timer_path) = scrub_unit_paths(dir.path(), "manual-group");
    std::fs::write(&service_path, "[Unit]\nDescription=hand-written, not shr-rs\n").unwrap();

    let empty_state = StateFile::new(vec![]);
    let orphans = find_orphaned_scrub_units(dir.path(), &empty_state).unwrap();

    assert!(orphans.owned.is_empty(), "{orphans:?}");
    assert_eq!(orphans.unowned_lookalikes, vec![service_path], "{orphans:?}");
}

#[test]
fn find_orphaned_scrub_units_is_empty_when_the_unit_directory_does_not_exist_yet() {
    let dir = tempdir().unwrap();
    let never_installed_dir = dir.path().join("never-created");
    let state = state_with_bands(vec![band(0, "md0", None)], None);

    let orphans = find_orphaned_scrub_units(&never_installed_dir, &state).unwrap();
    assert_eq!(
        orphans,
        Default::default(),
        "a host that never ran `schedule install` has nothing to prune"
    );
}

#[test]
fn remove_owned_unit_file_deletes_ours_but_refuses_a_hand_written_lookalike() {
    let dir = tempdir().unwrap();
    let state = state_with_bands(vec![band(0, "md0", None)], None);
    let (ours_service, _) = scrub_unit_paths(dir.path(), "default");
    write_scrub_timer_units(dir.path(), &state, test_exe_path()).unwrap();
    assert!(ours_service.exists());

    assert!(
        remove_owned_unit_file(&ours_service).unwrap(),
        "must report a real deletion"
    );
    assert!(!ours_service.exists(), "our own unit must actually be gone");

    let hand_written = dir.path().join("shr-rs-scrub-someone-elses.service");
    std::fs::write(&hand_written, "[Unit]\nDescription=not ours\n").unwrap();
    assert!(
        !remove_owned_unit_file(&hand_written).unwrap(),
        "must refuse to report deleting a lookalike"
    );
    assert!(
        hand_written.exists(),
        "a hand-written lookalike must never actually be deleted"
    );

    let missing = dir.path().join("nothing-here.service");
    assert!(
        !remove_owned_unit_file(&missing).unwrap(),
        "a missing file is a no-op, not an error"
    );
}
