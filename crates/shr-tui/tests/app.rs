use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use shr_command::{
    report::MemberStatus, ArrayStatus, DiskStatus, GroupBandStatus, GroupStatus, Health, SmartState,
    SmartSummary, StatusReport, SyncSummary,
};
use shr_tui::{array_needs_attention, render, wizard::Step, App, RefreshWorker, Snapshot, Tab, WizardAction};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

fn sample_report() -> StatusReport {
    StatusReport {
        schema_version: 1,
        health: Health::Degraded,
        disks: vec![DiskStatus {
            name: "vda".into(),
            id: Some("ata-EXAMPLE-SERIAL-1".into()),
            size: Some(4_000_000_000_000),
            model: Some("Example Disk".into()),
            serial: Some("SERIAL-1".into()),
            rotational: Some(true),
            smart: SmartSummary {
                state: SmartState::Warning,
                temperature_c: Some(41),
                power_on_hours: Some(1200),
                pending_sectors: Some(1),
                reallocated_sectors: Some(0),
                uncorrectable_sectors: Some(0),
                nvme_critical_warning: None,
            },
            arrays: vec!["md0".into()],
            system_disk: false,
            system_mounts: vec![],
        }],
        arrays: vec![ArrayStatus {
            name: "md0".into(),
            level: Some("raid5".into()),
            state: "active".into(),
            read_only: false,
            degraded: true,
            raid_disks: Some(3),
            active_disks: Some(2),
            members: vec!["vda1".into(), "vdb1".into()],
            member_states: vec![],
            sync: Some(SyncSummary {
                action: "recovery".into(),
                percent: Some(42.5),
                finish_min: Some(8.2),
            }),
        }],
        // No state.toml fixture in this test -- TUI rendering of the
        // inventory-derived disks/arrays views is what's under test here,
        // not the new groups section (that's covered in shr-command's own
        // tests).
        groups: vec![],
        state_path: None,
    }
}

/// Fixture: TWO groups (proving the TUI can display more than one), one
/// of them ("shr2-hetero") with a band whose `md_name` ("md1") matches a
/// LIVE `ArrayStatus` carrying an in-progress reshape -- this is what proves
/// the Bands tab actually cross-references live sync data by `md_name`
/// rather than only ever showing "idle".
fn sample_report_with_groups() -> StatusReport {
    let mut report = sample_report();
    report.arrays.push(ArrayStatus {
        name: "md1".into(),
        level: Some("raid5".into()),
        state: "active".into(),
        read_only: false,
        degraded: false,
        raid_disks: Some(4),
        active_disks: Some(4),
        members: vec!["vdc1".into(), "vdd1".into(), "vde1".into(), "vdf1".into()],
        member_states: vec![],
        sync: Some(SyncSummary {
            action: "reshape".into(),
            percent: Some(17.3),
            finish_min: Some(240.0),
        }),
    });
    report.groups = vec![
        GroupStatus {
            name: "shr1".into(),
            mode: "shr".into(),
            layout_version: 1,
            mount_point: "/mnt/shr_data".into(),
            fs_uuid: Some("11111111-2222-3333-4444-555555555555".into()),
            vg_name: "shr_vg".into(),
            lv_name: "data".into(),
            compression: "zstd:3".into(),
            usable_bytes: 8_000_000_000_000,
            resize_pending: false,
            disks: vec!["ata-DISK1".into(), "ata-DISK2".into(), "ata-DISK3".into()],
            bands: vec![GroupBandStatus {
                index: 0,
                level: "raid5".into(),
                md_name: "md0".into(),
                md_uuid: None,
                usable_bytes: 8_000_000_000_000,
                resize_pending: false,
                // Matches `report.arrays`' "md0" entry's live members
                // -- a band whose md_name IS a live array (present in
                // `report.arrays` with sync/member data) must not also claim
                // "no live mdadm array" via an empty `members` here; that
                // combination can't occur from a real live scan (both are
                // sourced from the same mdstat read, see `GroupBandStatus::
                // members`'s doc comment).
                members: vec!["vda1".into(), "vdb1".into()],
                member_states: vec![],
                sync: None,
                last_scrub: None,
                scrub_in_progress: false,
                pending_member_removal: None,
                ..Default::default()
            }],
        },
        GroupStatus {
            name: "shr2-hetero".into(),
            mode: "shr2".into(),
            layout_version: 2,
            mount_point: "/mnt/shr2_data".into(),
            fs_uuid: None,
            vg_name: "shr_vg".into(),
            lv_name: "data".into(),
            compression: "zstd:3".into(),
            usable_bytes: 16_000_000_000_000,
            resize_pending: true,
            disks: vec![
                "ata-DISK4".into(),
                "ata-DISK5".into(),
                "ata-DISK6".into(),
                "ata-DISK7".into(),
            ],
            bands: vec![GroupBandStatus {
                index: 0,
                level: "raid5".into(),
                md_name: "md1".into(),
                md_uuid: None,
                usable_bytes: 16_000_000_000_000,
                resize_pending: true,
                // Matches `report.arrays`' "md1" entry's live members
                // (see the same rationale on shr1's band0 above) -- this is
                // the band the test cross-references live reshape percent
                // for, so it must reflect that md1 really is live.
                members: vec!["vdc1".into(), "vdd1".into(), "vde1".into(), "vdf1".into()],
                member_states: vec![],
                sync: None,
                last_scrub: None,
                scrub_in_progress: false,
                pending_member_removal: None,
                ..Default::default()
            }],
        },
    ];
    report
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn navigation_refresh_and_quit_are_explicit() {
    let mut app = App::new(sample_report());
    assert_eq!(app.tab(), Tab::Dashboard);

    // Tab forward through all 7 views (an earlier fix added four new tabs after
    // Arrays), then confirm it wraps back to Dashboard, not to Arrays --
    // Tab::ALL.len() must actually be honored, not hardcoded to the old 3.
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab(), Tab::Disks);
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.tab(), Tab::Arrays);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab(), Tab::Groups);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab(), Tab::Bands);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab(), Tab::Fs);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab(), Tab::Logs);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab(), Tab::Dashboard);
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.tab(), Tab::Logs);

    for (ch, expected) in [
        ('1', Tab::Dashboard),
        ('2', Tab::Disks),
        ('3', Tab::Arrays),
        ('4', Tab::Groups),
        ('5', Tab::Bands),
        ('6', Tab::Fs),
        ('7', Tab::Logs),
    ] {
        app.handle_key(key(KeyCode::Char(ch)));
        assert_eq!(app.tab(), expected, "key '{ch}' must select {expected:?}");
    }

    app.handle_key(key(KeyCode::Char('r')));
    assert!(app.take_refresh_requested());
    assert!(!app.take_refresh_requested());

    app.handle_key(key(KeyCode::Char('q')));
    assert!(app.should_quit());
}

#[test]
fn refresh_error_keeps_last_known_good_report() {
    let report = sample_report();
    let mut app = App::new(report.clone());

    app.set_error("lsblk failed");
    assert_eq!(app.report(), &report);
    assert_eq!(app.error(), Some("lsblk failed"));

    app.replace_report(report);
    assert_eq!(app.error(), None);
}

#[test]
fn dashboard_and_detail_tabs_render_live_report_data() {
    let backend = TestBackend::new(110, 34);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(sample_report());

    terminal.draw(|frame| render(frame, &app)).unwrap();
    let dashboard = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(dashboard.contains("SHR-RS"));
    assert!(dashboard.contains("DEGRADED"));
    assert!(dashboard.contains("md0"));

    app.handle_key(key(KeyCode::Char('2')));
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let disks = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(disks.contains("Example Disk"));
    assert!(disks.contains("SERIAL-1"));

    app.handle_key(key(KeyCode::Char('3')));
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let arrays = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(arrays.contains("RAID5"));
    assert!(arrays.contains("42.5%"));
}

#[test]
fn groups_bands_fs_and_logs_tabs_render_live_data() {
    let backend = TestBackend::new(120, 34);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(sample_report_with_groups());
    app.replace_snapshot(Snapshot {
        report: sample_report_with_groups(),
        logs: vec!["2026-07-26 md1: reshape started".to_string()],
        // No live `btrfs`/`df` data in this fixture, so every usage
        // field is `None` and the FS tab renders `?` -- which is what this
        // test wants anyway: it asserts on tabs/logs, not on capacity.
        fs_df: shr_command::build_fs_df(&sample_report_with_groups().groups, &Default::default()),
    });

    fn render_current(terminal: &mut Terminal<TestBackend>, app: &shr_tui::App) -> String {
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    // Both groups are visible on the Groups tab, not just the first one.
    app.handle_key(key(KeyCode::Char('4')));
    let groups = render_current(&mut terminal, &app);
    assert!(groups.contains("shr1"), "first group must be visible: {groups}");
    assert!(
        groups.contains("shr2-hetero"),
        "second group must be visible -- this is the whole point of a multi-group fixture: {groups}"
    );
    assert!(groups.contains("expansion unfinished"));

    // The Bands tab shows the live reshape percent for md1's band,
    // cross-referenced from report.arrays by md_name -- not just "idle".
    app.handle_key(key(KeyCode::Char('5')));
    let bands = render_current(&mut terminal, &app);
    assert!(bands.contains("md1"));
    assert!(
        bands.contains("17.3%"),
        "band md1's live reshape percent must be cross-referenced from arrays: {bands}"
    );
    assert!(bands.contains("md0"));

    // The FS tab shows per-group filesystem info, including a group
    // with no fs_uuid yet (must render "-", not panic/blank).
    app.handle_key(key(KeyCode::Char('6')));
    let fs = render_current(&mut terminal, &app);
    assert!(fs.contains("/mnt/shr_data"));
    assert!(fs.contains("/mnt/shr2_data"));
    assert!(fs.contains("11111111"));

    // The Logs tab shows the fetched kernel log lines.
    app.handle_key(key(KeyCode::Char('7')));
    let logs = render_current(&mut terminal, &app);
    assert!(
        logs.contains("reshape started"),
        "log tab must show fetched log lines: {logs}"
    );
}

/// The TUI's Arrays tab used to render `array.members.join(", ")` --
/// plain names, ignoring `array.member_states` entirely -- so an operator
/// watching a `disk replace` in the TUI had strictly less information than
/// the CLI's `status --detail`, which already marks `(F)`/faulty and
/// `(R)`/replacement. This must use the same `(F)`/`(S)`/`(W)`/`(R)`
/// vocabulary as `shr_command::render::annotated_members`.
#[test]
fn arrays_tab_annotates_faulty_and_replacement_members_like_the_cli() {
    let backend = TestBackend::new(120, 34);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut report = sample_report();
    report.arrays[0].member_states = vec![
        MemberStatus {
            name: "vda1".into(),
            role: Some(0),
            faulty: true,
            spare: false,
            write_mostly: false,
            replacement: false,
        },
        MemberStatus {
            name: "vdb1".into(),
            role: Some(1),
            faulty: false,
            spare: false,
            write_mostly: false,
            replacement: true,
        },
    ];
    let mut app = App::new(report);
    app.handle_key(key(KeyCode::Char('3'))); // Arrays tab

    terminal.draw(|frame| render(frame, &app)).unwrap();
    let arrays = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        arrays.contains("vda1(F)"),
        "faulty member must be annotated on the Arrays tab, like the CLI: {arrays}"
    );
    assert!(
        arrays.contains("vdb1(R)"),
        "Replacement member must be annotated too: {arrays}"
    );
}

#[test]
fn logs_tab_shows_a_placeholder_instead_of_a_blank_pane_when_there_are_no_lines() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(sample_report());
    app.handle_key(key(KeyCode::Char('7')));

    terminal.draw(|frame| render(frame, &app)).unwrap();
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(content.contains("no recent kernel log"));
}

#[test]
fn inactive_array_needs_attention_even_without_degraded_flag() {
    let mut array = sample_report().arrays.remove(0);
    array.state = "inactive".into();
    array.degraded = false;
    array.read_only = false;
    array.active_disks = array.raid_disks;

    assert!(array_needs_attention(&array));
}

#[test]
fn array_warning_rule_covers_all_unsafe_states() {
    let mut array = sample_report().arrays.remove(0);
    array.degraded = false;
    array.read_only = false;
    array.active_disks = array.raid_disks;
    assert!(!array_needs_attention(&array));

    array.state = "clean".into();
    assert!(!array_needs_attention(&array));
    array.state = "inactive".into();
    assert!(array_needs_attention(&array));
    array.state = "active".into();
    array.read_only = true;
    assert!(array_needs_attention(&array));
    array.read_only = false;
    array.degraded = true;
    assert!(array_needs_attention(&array));
    array.degraded = false;
    array.level = Some("raid6".into());
    array.raid_disks = Some(3);
    assert!(array_needs_attention(&array));
}

/// `App`'s side of the Add Disk wizard is pure UI-state wiring -- no
/// IO, so this covers it directly without needing `main.rs`'s controller/
/// thread plumbing (that's `wizard::tests` in `src/wizard.rs`).
mod add_disk_wizard {
    use super::*;

    /// One group, one disk report -- the simplification `open_wizard` makes
    /// explicit in its own doc comment (auto-selects the group only when
    /// exactly one exists).
    fn single_group_report() -> StatusReport {
        let mut report = sample_report();
        report.disks.push(DiskStatus {
            name: "vdb".into(),
            id: Some("ata-EXAMPLE-SERIAL-2".into()),
            size: Some(4_000_000_000_000),
            model: Some("New Disk".into()),
            serial: Some("SERIAL-2".into()),
            rotational: Some(true),
            smart: SmartSummary {
                state: SmartState::Ok,
                temperature_c: None,
                power_on_hours: None,
                pending_sectors: None,
                reallocated_sectors: None,
                uncorrectable_sectors: None,
                nvme_critical_warning: None,
            },
            arrays: vec![],
            system_disk: false,
            system_mounts: vec![],
        });
        report.groups = vec![GroupStatus {
            name: "shr1".into(),
            mode: "shr".into(),
            layout_version: 1,
            mount_point: "/mnt/shr_data".into(),
            fs_uuid: None,
            vg_name: "shr_vg".into(),
            lv_name: "data".into(),
            compression: "zstd:3".into(),
            usable_bytes: 4_000_000_000_000,
            resize_pending: false,
            disks: vec!["ata-EXAMPLE-SERIAL-1".into()],
            bands: vec![GroupBandStatus {
                index: 0,
                level: "raid1".into(),
                md_name: "md0".into(),
                md_uuid: None,
                usable_bytes: 4_000_000_000_000,
                resize_pending: false,
                members: vec![],
                member_states: vec![],
                sync: None,
                last_scrub: None,
                scrub_in_progress: false,
                pending_member_removal: None,
                ..Default::default()
            }],
        }];
        report
    }

    #[test]
    fn a_opens_the_wizard_and_auto_selects_the_only_group() {
        let mut app = App::new(single_group_report());
        assert!(app.wizard().is_none());

        app.handle_key(key(KeyCode::Char('a')));

        let wizard = app.wizard().expect("wizard should be open");
        assert_eq!(wizard.group_name, "shr1");
        assert_eq!(wizard.step(), Step::SelectDisks);
        assert!(app.error().is_none());
    }

    #[test]
    fn ambiguous_group_count_refuses_to_open_and_reports_why_not_silently() {
        let mut app = App::new(sample_report()); // zero groups
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.wizard().is_none());
        assert!(app.error().is_some(), "must explain why, not silently do nothing");

        let mut report = single_group_report();
        report.groups.push(report.groups[0].clone());
        report.groups[1].name = "shr2".into();
        let mut app = App::new(report);
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.wizard().is_none());
        assert!(app.error().is_some());
    }

    #[test]
    fn enter_without_any_disk_selected_does_not_request_preflight() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Enter));

        assert!(
            app.take_wizard_action().is_none(),
            "constraint 1: nothing to preview yet"
        );
        assert_eq!(app.wizard().unwrap().step(), Step::SelectDisks);
    }

    #[test]
    fn selecting_a_disk_and_pressing_enter_requests_preflight_exactly_once() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Down)); // move onto "vdb", the new disk
        app.handle_key(key(KeyCode::Char(' '))); // select it
        assert_eq!(app.wizard().unwrap().selected, vec!["vdb".to_string()]);

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.take_wizard_action(), Some(WizardAction::RunPreflight));
        assert!(
            app.take_wizard_action().is_none(),
            "the action must be consumed, not repeated"
        );
    }

    /// The TUI Add Disk wizard must not let the operator select the
    /// system disk -- Cockpit's create wizard was fixed for this but
    /// the TUI's own Add Disk wizard was never touched. `single_group_report`
    /// puts the system disk ("vda") at cursor 0, so this exercises the
    /// refusal at the very first row without moving the cursor at all.
    #[test]
    fn a_system_disk_cannot_be_selected_but_the_cursor_still_moves_over_it() {
        let mut report = single_group_report();
        report.disks[0].system_disk = true; // "vda"
        let mut app = App::new(report);
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.wizard().unwrap().candidate_disks[0].system_disk);

        // Cursor starts on the system disk row (index 0). Space must refuse.
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(
            app.wizard().unwrap().selected.is_empty(),
            "The system disk must never end up selected"
        );
        assert!(
            app.wizard().unwrap().selection_blocked_reason.is_some(),
            "must say why the selection was refused, not silently do nothing"
        );

        // The cursor must still be free to move onto/over the row -- the
        // brief explicitly rules out skipping it, which would be confusing.
        assert_eq!(app.wizard().unwrap().cursor, 0);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.wizard().unwrap().cursor,
            1,
            "cursor must be able to move past the system disk row"
        );

        // The non-system disk at cursor 1 ("vdb") must remain selectable.
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.wizard().unwrap().selected, vec!["vdb".to_string()]);
    }

    #[test]
    fn confirmation_text_must_match_the_group_name_exactly_before_execute_is_requested() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Enter));
        app.take_wizard_action(); // consume RunPreflight

        // Simulate main.rs reporting back that preflight passed and the
        // dry-run preview succeeded (the only way `App` ever reaches
        // `Step::Confirm` for real -- it never calls `preflight_create`/
        // `preview_expand` itself).
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Confirm),
            preview_commands: vec!["mdadm --add /dev/md0 /dev/vdb1".to_string()],
            ..Default::default()
        });

        for ch in "shr".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(
            app.take_wizard_action().is_none(),
            "constraint 5: partial match must not confirm"
        );

        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.wizard().unwrap().confirmation_input, "shr1");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.take_wizard_action(), Some(WizardAction::Execute));
    }

    /// `Step::Preflight`'s `o` key must set `force_content` and re-fire
    /// `RunPreflight` -- this alone is NOT proof the override actually
    /// reaches `AddDiskController` (a test asserting only `force_content ==
    /// true` would still pass even if `runtime.rs` kept forwarding a
    /// hardcoded `false`, which is precisely the earlier trap); the
    /// controller-reaching proof lives in `runtime.rs`'s own test module,
    /// alongside its `wizard_execute_forwards_confirmation`-pattern
    /// tests. This test only covers `App`'s pure UI-state half of the wire.
    #[test]
    fn o_sets_force_content_and_requests_preflight_again_from_the_preflight_step() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Enter));
        app.take_wizard_action(); // consume the first RunPreflight

        // Simulate runtime.rs reporting a blocked preflight (the only real
        // way `App` reaches `Step::Preflight` -- it never calls
        // `preflight_create` itself).
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Preflight),
            ..Default::default()
        });
        assert!(!app.wizard().unwrap().force_content, "must be off by default");

        app.handle_key(key(KeyCode::Char('o')));
        assert!(
            app.wizard().unwrap().force_content,
            "'o' must set the override flag"
        );
        assert_eq!(
            app.take_wizard_action(),
            Some(WizardAction::RunPreflight),
            "'o' must re-request preflight so the override actually gets evaluated"
        );
    }

    /// Constraint: `force_content` must never be inferred or enabled by
    /// any key other than the deliberate `o` -- plain Enter (used everywhere
    /// else in the wizard to advance) must do nothing at `Step::Preflight`.
    #[test]
    fn plain_enter_does_not_set_force_content_at_the_preflight_step() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Preflight),
            ..Default::default()
        });

        app.handle_key(key(KeyCode::Enter));
        assert!(!app.wizard().unwrap().force_content);
        assert!(app.take_wizard_action().is_none());
    }

    #[test]
    fn backspace_edits_the_confirmation_text() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Confirm),
            ..Default::default()
        });

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('y')));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.wizard().unwrap().confirmation_input, "x");
    }

    #[test]
    fn esc_closes_the_wizard_from_any_step_without_requesting_execute() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Confirm),
            ..Default::default()
        });
        app.handle_key(key(KeyCode::Esc));

        assert!(app.wizard().is_none());
        assert!(app.take_wizard_action().is_none());
        assert!(
            !app.should_quit(),
            "Esc while the wizard is open must close the wizard, not quit the TUI"
        );
    }

    /// `Step::Executing` means a background thread is running the
    /// real, potentially hours-long `AddDiskController::execute()` against
    /// real mdadm/LVM/Btrfs (see `runtime.rs`'s `WizardAction::Execute`).
    /// Pre-fix, `handle_wizard_key`'s Esc arm unconditionally closed the
    /// modal here too -- the disk work kept running regardless, but the
    /// eventual success/failure report was thrown away with nothing to show
    /// for it. Esc during this one step must refuse and say why, not close.
    #[test]
    fn esc_is_refused_while_executing_and_says_why() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Executing),
            ..Default::default()
        });

        app.handle_key(key(KeyCode::Esc));

        assert!(
            app.wizard().is_some(),
            "Esc must not abandon an in-flight destructive operation"
        );
        assert_eq!(app.wizard().unwrap().step(), Step::Executing);
        assert!(
            app.error().is_some(),
            "must say why Esc did nothing, not silently ignore it"
        );
    }

    /// The second half: even with Esc refused during `Executing`, a result
    /// can still arrive after the wizard is gone by some other means (the
    /// modal was already closed from a later step). That result must not
    /// vanish silently -- it must surface through `App`'s existing
    /// error/message banner, the same one `refresh` failures already use.
    #[test]
    fn a_late_arriving_result_after_the_wizard_is_gone_is_not_silently_dropped() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Confirm),
            ..Default::default()
        });
        app.handle_key(key(KeyCode::Esc)); // Confirm still allows Esc to close.
        assert!(app.wizard().is_none());

        // Simulate runtime.rs's background execute() thread reporting its
        // real outcome after the operator already closed the modal.
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Error),
            error_message: Some("mdadm --add failed: device busy".to_string()),
            ..Default::default()
        });

        let msg = app.error().expect("A late result must not vanish silently");
        assert!(msg.contains("device busy"), "{msg}");
    }

    /// The success-path sibling of the test above: the only other test
    /// exercising `wizard_result_message` used `Step::Error`, so the
    /// `Step::Done` arm (the common case -- the operation actually
    /// succeeded) had no coverage at all. Confirms both that a late success
    /// surfaces and that it names the group/layout version, not a generic
    /// "something happened" banner.
    #[test]
    fn a_late_arriving_success_after_the_wizard_is_gone_still_names_the_group() {
        let mut app = App::new(single_group_report());
        app.handle_key(key(KeyCode::Char('a')));
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Confirm),
            ..Default::default()
        });
        app.handle_key(key(KeyCode::Esc));
        assert!(app.wizard().is_none());

        let result = shr_state::ArrayState {
            name: "shr1".to_string(),
            mode: "shr".to_string(),
            created_at: "2026-07-30T00:00:00Z".to_string(),
            layout_version: 2,
            disks: vec![],
            bands: vec![],
            filesystem: shr_state::StateFilesystem {
                fs_uuid: None,
                mount_point: "/mnt/shr_data".to_string(),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
            },
            expansion: shr_state::StateExpansion::default(),
        };
        app.set_wizard_state(shr_tui::wizard::WizardState {
            step: Some(Step::Done),
            result: Some(result),
            ..Default::default()
        });

        let msg = app
            .error()
            .expect("A late success must not vanish silently either");
        assert!(msg.contains("shr1"), "{msg}");
        // Not the layout version: that is an internal on-disk revision
        // number and no longer reaches any user-facing surface.
        assert!(msg.contains("band(s)"), "{msg}");
    }

    #[test]
    fn while_the_wizard_is_open_number_keys_do_not_leak_through_to_tab_navigation() {
        let mut app = App::new(single_group_report());
        assert_eq!(app.tab(), Tab::Dashboard);
        app.handle_key(key(KeyCode::Char('a')));

        app.handle_key(key(KeyCode::Char('4'))); // would normally jump to the Groups tab
        assert_eq!(
            app.tab(),
            Tab::Dashboard,
            "wizard key handling must consume the key, not fall through"
        );
    }
}

#[test]
fn refresh_worker_never_blocks_or_stacks_probes() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let report = sample_report();
    let mut worker = RefreshWorker::spawn(move || {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(Snapshot {
            fs_df: shr_command::build_fs_df(&report.groups, &Default::default()),
            report: report.clone(),
            logs: Vec::new(),
        })
    });

    let before = Instant::now();
    assert!(worker.request());
    assert!(before.elapsed() < Duration::from_millis(50));
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!worker.request(), "an in-flight inspection must be coalesced");

    release_tx.send(()).unwrap();
    let result = loop {
        if let Some(result) = worker.try_result() {
            break result;
        }
        assert!(before.elapsed() < Duration::from_secs(2));
        std::thread::yield_now();
    };
    assert!(result.is_ok());
    assert!(!worker.is_in_flight());
}

/// SMART detail, fault-tolerance-remaining, and the system-disk marker
/// were each computed/received but never reached the TUI, while Cockpit
/// showed all three (`crates/shr-command/src/report.rs`'s own doc comments,
/// `cockpit/src/panels.tsx:137-144,510-532`, `cockpit/src/model.ts:837-882`).
/// These exercise the full `render()` + tab-navigation loop, not just the
/// isolated `render_disks`/`render_groups` unit tests in `shr-tui/src/ui.rs`.
mod disk_and_group_detail {
    use super::*;
    use ratatui::buffer::Buffer;

    /// Mirrors `ui.rs`'s own `buffer_text` fix: double-width Hangul leaves a
    /// continuation cell right after every such grapheme (`Buffer::set_stringn`
    /// resets it), and a reset cell's `symbol()` reports a literal `" "` --
    /// indistinguishable from a real space once concatenated naively. Skip
    /// the continuation cell so the reconstructed text matches what a human
    /// actually reads on screen.
    fn is_wide_char(ch: char) -> bool {
        matches!(ch as u32,
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6)
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            let mut x = area.left();
            while x < area.right() {
                let symbol = buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                let wide = symbol.chars().next().is_some_and(is_wide_char);
                out.push_str(symbol);
                x += if wide { 2 } else { 1 };
            }
        }
        out
    }

    fn render_tab(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// Known-value case, through the real tab-navigation path
    /// (`2` key), not a direct `render_disks` call: SMART detail and the
    /// system-disk marker must both reach the screen for the same disk row.
    #[test]
    fn disks_tab_shows_smart_detail_and_the_system_disk_marker_via_real_navigation() {
        let mut report = sample_report();
        report.disks[0].system_disk = true;
        report.disks[0].system_mounts = vec!["/".to_string()];
        let mut app = App::new(report);
        app.handle_key(key(KeyCode::Char('2')));

        let text = render_tab(&app, 160, 20);
        assert!(
            text.contains("1200h"),
            "known power-on-hours must render on the Disks tab: {text}"
        );
        assert!(
            text.contains("1 pending sector"),
            "known pending-sector count must render: {text}"
        );
        assert!(
            text.contains("SYSTEM DISK"),
            "the system-disk marker must render without opening a wizard: {text}"
        );
    }

    /// Known-value case via real navigation (`4` key): a group whose
    /// band carries live, fully-healthy `member_states` must show the
    /// nominal SHR tolerance, not the "no live data" fallback.
    #[test]
    fn groups_tab_shows_fault_tolerance_for_a_band_with_live_healthy_members_via_real_navigation() {
        let mut report = sample_report_with_groups();
        report.groups[0].bands[0].member_states = vec![
            MemberStatus {
                name: "vda1".into(),
                role: Some(0),
                faulty: false,
                spare: false,
                write_mostly: false,
                replacement: false,
            },
            MemberStatus {
                name: "vdb1".into(),
                role: Some(1),
                faulty: false,
                spare: false,
                write_mostly: false,
                replacement: false,
            },
        ];
        let mut app = App::new(report);
        app.handle_key(key(KeyCode::Char('4')));

        let text = render_tab(&app, 160, 20);
        assert!(
            text.contains("tolerates 1-disk loss"),
            "a fully-healthy SHR band must show the nominal tolerance: {text}"
        );
    }

    /// Honesty requirement via real navigation: `sample_report_with_
    /// groups`'s bands carry empty `member_states` (no live data yet) --
    /// this must never render as if the group were confirmed fully healthy.
    #[test]
    fn groups_tab_shows_unknown_tolerance_not_a_fabricated_one_via_real_navigation() {
        let mut app = App::new(sample_report_with_groups());
        app.handle_key(key(KeyCode::Char('4')));

        let text = render_tab(&app, 160, 20);
        assert!(
            text.contains("no live member data"),
            "missing live member data must say so: {text}"
        );
    }
}
