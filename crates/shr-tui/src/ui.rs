use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap},
    Frame,
};
use shr_command::{ArrayStatus, FsDfReport, GroupBandStatus, GroupStatus, Health, SmartState};
use shr_inspect::WriteBlocker;

use crate::app::{ReconcileStep, ReconcileView, ReplaceView, ScrubView};
use crate::scrub::Step as ScrubStep;
use crate::wizard::{ReplaceStep, Step};
use crate::{App, Tab, WizardView};

const ACCENT: Color = Color::Rgb(115, 188, 247);
const GOOD: Color = Color::Rgb(110, 198, 100);
const WARNING: Color = Color::Rgb(244, 193, 69);
const DANGER: Color = Color::Rgb(224, 108, 96);
const MUTED: Color = Color::DarkGray;

pub fn render(frame: &mut Frame, app: &App) {
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, app, header);
    render_tabs(frame, app, tabs);
    render_body(frame, app, body);
    render_footer(frame, footer);

    // Mutually exclusive, mirroring the guarantee `app.rs::handle_key`
    // enforces on the input side (only one of `wizard`/`replace`/`scrub` is
    // ever `Some` at a time) -- an `if`/`else if` chain rather than three
    // independent `if let`s so that guarantee is visible here too, not just
    // assumed.
    if let Some(wizard) = app.wizard() {
        render_wizard_overlay(frame, wizard, frame.area());
    } else if let Some(replace) = app.replace() {
        render_replace_overlay(frame, replace, frame.area());
    } else if let Some(scrub) = app.scrub() {
        render_scrub_overlay(frame, scrub, frame.area());
    } else if let Some(reconcile) = app.reconcile() {
        render_reconcile_overlay(frame, reconcile, frame.area());
    }
}

pub fn array_needs_attention(array: &ArrayStatus) -> bool {
    let state = array.state.trim();
    let invalid_raid6 = array.level.as_deref().is_some_and(|level| {
        level.eq_ignore_ascii_case("raid6") && array.raid_disks.unwrap_or(array.members.len()) < 4
    });
    array.degraded
        || array.read_only
        || !(state.eq_ignore_ascii_case("active") || state.eq_ignore_ascii_case("clean"))
        || invalid_raid6
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let (health, color) = health_label(&app.report().health);
    let title = Line::from(vec![
        Span::styled(" SHR-RS ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  storage dashboard"),
        Span::raw("  ·  "),
        Span::styled(health, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(
        Paragraph::new(title)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let labels = [
        "1 Dashboard",
        "2 Disks",
        "3 Arrays",
        "4 Groups",
        "5 Bands",
        "6 FS",
        "7 Logs",
    ]
    .into_iter()
    .map(Line::from)
    .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(labels)
            .select(app.tab().index())
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .divider(" │ ")
            .block(Block::default().borders(Borders::ALL).title("Views")),
        area,
    );
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let content = if app.error().is_some() {
        let [error, content] = Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).areas(area);
        let message = app.error().unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" refresh failed: ", Style::default().fg(WARNING).bold()),
                Span::raw(message),
                Span::styled("  (showing last known-good state)", Style::default().fg(MUTED)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(WARNING)),
            ),
            error,
        );
        content
    } else {
        area
    };

    match app.tab() {
        Tab::Dashboard => render_dashboard(frame, app, content),
        Tab::Disks => render_disks(frame, app, content),
        Tab::Arrays => render_arrays(frame, app, content),
        Tab::Groups => render_groups(frame, app, content),
        Tab::Bands => render_bands(frame, app, content),
        Tab::Fs => render_fs(frame, app, content),
        Tab::Logs => render_logs(frame, app, content),
    }
}

fn render_dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let [summary, arrays] = Layout::vertical([Constraint::Length(7), Constraint::Min(5)]).areas(area);
    let report = app.report();
    let raw = report
        .disks
        .iter()
        .try_fold(0u64, |sum, disk| disk.size.map(|size| sum.saturating_add(size)));
    let smart_warnings = report
        .disks
        .iter()
        .filter(|disk| disk.smart.state == SmartState::Warning)
        .count();
    let array_warnings = report
        .arrays
        .iter()
        .filter(|array| array_needs_attention(array))
        .count();
    let raw = raw
        .map(shr_command::render::human_bytes)
        .unwrap_or_else(|| "unknown".into());

    let summary_text = Text::from(vec![
        Line::from(vec![
            Span::styled("Observed raw capacity  ", Style::default().fg(MUTED)),
            Span::styled(raw, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(format!(
            "Disks {}  ·  SMART warnings {}  ·  Arrays {}  ·  Warnings {}",
            report.disks.len(),
            smart_warnings,
            report.arrays.len(),
            array_warnings
        )),
        Line::from(Span::styled(
            "Live figures read from this host · usable and parity capacity are not estimated here",
            Style::default().fg(MUTED),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(summary_text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Overview")),
        summary,
    );
    render_array_table(frame, report.arrays.iter(), arrays, "RAID bands");
}

fn render_disks(frame: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["Node", "Model / serial", "Size", "SMART", "Arrays"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows = app.report().disks.iter().map(|disk| {
        let (smart, smart_color) = smart_label(&disk.smart.state);
        let mut smart = match disk.smart.temperature_c {
            Some(temp) => format!("{smart} · {temp}°C"),
            None => smart.to_string(),
        };
        // Pending/reallocated/uncorrectable/nvme-warning detail --
        // `report.rs::SmartSummary`'s own doc comment says this exists "so
        // the UI can show e.g. '1 Pending Sector'"; only Cockpit ever did
        // (`panels.tsx:137-144`). Appended as a second line, the same
        // precedent as temperature already being appended to this cell
        // rather than adding a new column (constraint 1).
        if let Some(detail) = smart_detail(&disk.smart) {
            smart.push('\n');
            smart.push_str(&detail);
        }
        // The system-disk marker previously appeared only inside
        // the Add/Replace-Disk wizard overlays -- finding the OS disk
        // required opening a destructive-action wizard. Appended as a
        // second line like Model/serial already is. The wording used
        // to match Cockpit's Korean disk-row marker (`panels.tsx`); Cockpit
        // stays Korean and the TUI is now English-only, so the two no
        // longer share literal text, only intent.
        //
        // The mounts go on their OWN third line, not appended to the marker.
        // Measured on the real guest: the (then-Korean) marker (32 display
        // cells) + " · /, /boot, /boot/efi" (22) = 54, which overran the
        // 48-cell column and truncated the mount list to "/, /boot, /bo".
        // Widening the column to fit one observed mount set just moves the
        // cliff; a host with more OS mounts would truncate again. A
        // separate line makes the marker's own width the only thing the
        // column must accommodate.
        let mut node = format!("/dev/{}", disk.name);
        if disk.system_disk {
            node.push('\n');
            node.push_str("SYSTEM DISK -- OS runs here, do not touch");
            if !disk.system_mounts.is_empty() {
                node.push('\n');
                node.push_str(&disk.system_mounts.join(", "));
            }
        }
        Row::new([
            Cell::from(node),
            Cell::from(format!(
                "{}\n{}",
                disk.model.as_deref().unwrap_or("-"),
                disk.serial.as_deref().unwrap_or("-")
            )),
            Cell::from(
                disk.size
                    .map(shr_command::render::human_bytes)
                    .unwrap_or_else(|| "?".into()),
            ),
            Cell::from(smart).style(Style::default().fg(smart_color)),
            Cell::from(if disk.arrays.is_empty() {
                "not linked".into()
            } else {
                disk.arrays.join(", ")
            }),
        ])
        // 3, not 2: the system disk needs a third line for its mount list
        // (see the `node` construction above). Every row is the same height
        // because ratatui's `Table` aligns cells per row, so a taller
        // system-disk row would otherwise misalign the columns beside it.
        .height(3)
    });
    let table = Table::new(
        rows,
        [
            // Sized for the marker alone -- "SYSTEM DISK -- OS
            // runs here, do not touch" is 41 ASCII cells (all single-width
            // now that the TUI is English-only), and the mount list has its
            // own line rather than sharing this one, so this width no longer
            // depends on how many OS mounts a given host happens to have.
            // 45 leaves headroom for the mount line too: `/, /boot,
            // /boot/efi` measured 22 on the guest.
            Constraint::Length(45),
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(40),
            Constraint::Min(12),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Disks ({})", app.report().disks.len())),
    );
    frame.render_widget(table, area);
}

/// Additional SMART detail beyond state+temperature, matching what
/// Cockpit's `DiskRow` already renders (`panels.tsx:137-144`), joined the
/// same way (" · "). `power_on_hours` uses a strict `Some`/`None` check --
/// a genuine 0 hours is real data, same as `temperature_c`'s existing
/// handling above -- but the four warning-signal fields use Cockpit's own
/// truthy filter (`disk.smart.pending_sectors ? ... : null`): a genuine 0
/// there reads as "nothing to report" too, so this mirrors it rather than
/// diverging from the reference frontend. Either way `None` never becomes a
/// shown "0" -- it becomes nothing, matching this codebase's `?`/omit
/// precedent for genuinely-unknown fields.
fn smart_detail(smart: &shr_command::SmartSummary) -> Option<String> {
    let parts: Vec<String> = [
        smart.power_on_hours.map(|h| format!("{h}h")),
        smart.pending_sectors.filter(|&v| v != 0).map(|v| {
            let noun = if v == 1 { "sector" } else { "sectors" };
            format!("{v} pending {noun}")
        }),
        smart
            .reallocated_sectors
            .filter(|&v| v != 0)
            .map(|v| format!("{v} reallocated")),
        smart
            .uncorrectable_sectors
            .filter(|&v| v != 0)
            .map(|v| format!("{v} uncorrectable")),
        smart
            .nvme_critical_warning
            .filter(|&v| v != 0)
            .map(|v| format!("NVMe warning 0x{v:x}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn render_arrays(frame: &mut Frame, app: &App, area: Rect) {
    render_array_table(frame, app.report().arrays.iter(), area, "RAID arrays");
}

/// Every SHR group `state.toml` records -- previously the TUI never
/// read `report.groups` at all, so a host managing more than one group had
/// no view that showed that.
fn render_groups(frame: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["Name", "Mode", "Mount", "Disks", "Bands", "Status"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let groups = &app.report().groups;
    let rows = groups.iter().map(|group| {
        let status_color = if group.resize_pending { WARNING } else { GOOD };
        // Fault-tolerance-remaining -- Cockpit already computes and
        // shows this (`model.ts:837-882`, `panels.tsx:510-532`); the TUI
        // reads the same `member_states` input but never derived
        // it. Appended as a second line to the Mode cell, following this
        // table's existing precedent of appending rather than adding a
        // column (constraint 1) -- mode is what determines the nominal
        // figure the tolerance is measured against, so the two belong
        // together the same way Cockpit pairs them in one badge+text.
        let tolerance = group_tolerance_status(&group.mode, &group.bands);
        let tolerance_col = tolerance_color(&tolerance);
        Row::new([
            Cell::from(group.name.clone()),
            Cell::from(format!(
                "{}\n{}",
                group.mode.to_ascii_uppercase(),
                tolerance_label(&tolerance)
            ))
            .style(Style::default().fg(tolerance_col)),
            Cell::from(group.mount_point.clone()),
            Cell::from(group.disks.len().to_string()),
            Cell::from(group.bands.len().to_string()),
            Cell::from(if group.resize_pending {
                "expansion unfinished"
            } else {
                "ok"
            })
            .style(Style::default().fg(status_color)),
        ])
        .height(2)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            // The tolerance label appended below the mode name
            // is now ASCII (all single-width); worst case ("designed for
            // 2-disk loss (no live member data)") is 46 cells wide. 50
            // leaves a little headroom, same margin the previous
            // Korean-sized width (52, for a 48-cell string) kept.
            Constraint::Length(50),
            Constraint::Min(18),
            Constraint::Length(7),
            Constraint::Length(7),
            // 20 cells: "expansion unfinished", the widest Status label.
            Constraint::Length(20),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("SHR groups ({})", groups.len())),
    );
    frame.render_widget(table, area);
}

/// Nominal disk-loss tolerance a redundancy mode promises when
/// every band is healthy. Ported verbatim from Cockpit's
/// `groupFaultTolerance` (`model.ts:837-846`) -- `None` for any mode this
/// UI doesn't recognize, never a guessed number.
fn group_fault_tolerance(mode: &str) -> Option<i64> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "shr" => Some(1),
        "shr2" => Some(2),
        _ => None,
    }
}

/// What the mode promises (`nominal`) vs the LIVE remaining margin right
/// now (`remaining`), mirroring Cockpit's `GroupToleranceStatus`
/// (`model.ts:848-860`).
struct GroupToleranceStatus {
    nominal: Option<i64>,
    remaining: Option<i64>,
}

/// Ported from Cockpit's `groupToleranceStatus` (`model.ts:862-882`)
/// rather than reinvented, per this task's instruction to port the formula
/// faithfully. The worst-affected band governs the group's remaining
/// margin -- bands can differ in width/level within one SHR-2 group (e.g. a
/// 2-disk raid1 band next to a 4-disk raid6 band) -- and `remaining` is
/// `None`, not a guess, whenever the mode itself is unrecognized, there are
/// no bands, or ANY band's `member_states` is empty. An empty
/// `member_states` means "no live mdstat match for this band right now"
/// (see `GroupBandStatus::member_states`'s doc comment) -- the one band
/// with no data could be the actual worst one, so the whole figure must not
/// pretend to know.
fn group_tolerance_status(mode: &str, bands: &[GroupBandStatus]) -> GroupToleranceStatus {
    let nominal = group_fault_tolerance(mode);
    let Some(nominal_value) = nominal else {
        return GroupToleranceStatus {
            nominal,
            remaining: None,
        };
    };
    if bands.is_empty() || bands.iter().any(|band| band.member_states.is_empty()) {
        return GroupToleranceStatus {
            nominal,
            remaining: None,
        };
    }
    let worst_faulty = bands
        .iter()
        .map(|band| band.member_states.iter().filter(|m| m.faulty).count() as i64)
        .max()
        .unwrap_or(0);
    GroupToleranceStatus {
        nominal,
        remaining: Some(nominal_value - worst_faulty),
    }
}

/// Wording ported verbatim from Cockpit's `toleranceLabel`
/// (`panels.tsx:308-320`) so an operator moving between the TUI and
/// Cockpit reads the same terms rather than relearning them.
fn tolerance_label(status: &GroupToleranceStatus) -> String {
    let Some(nominal) = status.nominal else {
        return "unrecognized mode -- tolerance unknown".to_string();
    };
    let Some(remaining) = status.remaining else {
        return format!("designed for {nominal}-disk loss (no live member data)");
    };
    if remaining == nominal {
        format!("tolerates {nominal}-disk loss")
    } else if remaining >= 0 {
        format!("{remaining} remaining / designed for {nominal}-disk loss")
    } else {
        // Never clamp to 0 -- a band already past its tolerance must read
        // as past it, matching Cockpit's own refusal to clamp.
        format!("over tolerance (designed for {nominal}-disk loss)")
    }
}

/// Ported from Cockpit's `toleranceTone` (`panels.tsx:322-329`).
fn tolerance_color(status: &GroupToleranceStatus) -> Color {
    let Some(nominal) = status.nominal else {
        return MUTED;
    };
    match status.remaining {
        None => WARNING,
        Some(remaining) if remaining < nominal => WARNING,
        _ => GOOD,
    }
}

/// Per-band view across every group, cross-referenced against
/// `report.arrays[].sync` by `md_name` for live reshape/resync progress --
/// `GroupBandStatus` alone carries no live sync percentage, only the
/// persisted `resize_pending` flag.
fn render_bands(frame: &mut Frame, app: &App, area: Rect) {
    let report = app.report();
    let sync_for =
        |md_name: &str| -> Option<&ArrayStatus> { report.arrays.iter().find(|a| a.name == md_name) };

    let header = Row::new(["Group", "Band", "Level", "Device", "Usable", "Sync", "Resize"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut row_count = 0;
    let rows: Vec<Row> = report
        .groups
        .iter()
        .flat_map(|group| group.bands.iter().map(move |band| (group, band)))
        .map(|(group, band)| {
            row_count += 1;
            // `band.members.is_empty()` == "no live mdadm array with
            // this name right now" (see `GroupBandStatus::members`'s doc
            // comment), same as the CLI's `render_band_detail_row` /
            // `watch_band_row` guards and this table's own sibling
            // `render_array_table` (`array.members.is_empty()` below) -- this
            // table alone printed `band.level`/`md_name`/`usable_bytes`
            // unconditionally, so a band whose array had been stopped (e.g.
            // a reboot where loopback devices/mdadm arrays don't come back
            // but `state.toml` survives) rendered identically to a live,
            // healthy one.
            let sync = if band.members.is_empty() {
                "no live RAID array".to_string()
            } else {
                sync_for(&band.md_name).and_then(|a| a.sync.as_ref()).map_or_else(
                    || "idle".to_string(),
                    |sync| match sync.percent {
                        Some(percent) => format!("{} {percent:.1}%", sync.action),
                        None => sync.action.clone(),
                    },
                )
            };
            let resize_color = if band.resize_pending { WARNING } else { GOOD };
            Row::new([
                Cell::from(group.name.clone()),
                Cell::from(format!("band{}", band.index)),
                Cell::from(band.level.to_ascii_uppercase()),
                Cell::from(format!("/dev/{}", band.md_name)),
                Cell::from(shr_command::render::human_bytes(band.usable_bytes)),
                Cell::from(sync),
                Cell::from(if band.resize_pending { "pending" } else { "ok" })
                    .style(Style::default().fg(resize_color)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(18),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Bands ({row_count})")),
    );
    frame.render_widget(table, area);
}

/// Per-group filesystem view.
fn render_fs(frame: &mut Frame, app: &App, area: Rect) {
    // Compression sits next to FS UUID -- both are per-filesystem facts
    // `fs_uuid` already had a column for; compression is the one `fs
    // recompress` (this table's operational trigger) changes.
    //
    // Used/Free added alongside Usable -- previously this tab showed no
    // indication of how full a filesystem actually is, unlike `shr-rs fs df`
    // (CLI) and Cockpit. FS UUID shrinks (38 -> 20) to make room; a full
    // UUID is still fully visible, just tighter, and the value itself is
    // unchanged.
    let header = Row::new([
        "Group",
        "Mount point",
        "FS UUID",
        "Compression",
        "Usable",
        "Used",
        "Free",
        "Resize",
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let groups = &app.report().groups;
    let fs_df = app.fs_df();
    let rows = groups.iter().map(|group| render_fs_row(group, fs_df));
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Min(20),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Filesystems ({})", groups.len())),
    );
    frame.render_widget(table, area);
}

/// Used = `data_used_bytes + metadata_used_bytes` (mirrors Cockpit's
/// `model.ts::summarizeCapacityUsage` combination rule -- both must be known
/// or the whole figure is `?`, never a partial sum). Free is
/// `unallocated_bytes` shown verbatim -- a DELIBERATE divergence from
/// Cockpit's own `freeBytes` (which computes `usable - used`): unallocated
/// space is the figure this crate's own `render_fs_df` header note already
/// calls more trustworthy than a derived one, since Btrfs allocates data and
/// metadata into chunks separately from what a plain `df` sees.
fn render_fs_row(group: &GroupStatus, fs_df: &FsDfReport) -> Row<'static> {
    let resize_color = if group.resize_pending { WARNING } else { GOOD };
    // `compression` is a required String (never fabricated -- see
    // GroupStatus's doc comment), so empty is shown as "-" like `fs_uuid`'s
    // `None` case, not guessed.
    let compression = if group.compression.is_empty() {
        "-".to_string()
    } else {
        group.compression.clone()
    };

    let df_row = fs_df.groups.iter().find(|g| g.name == group.name);
    let used = df_row.and_then(|g| match (g.data_used_bytes, g.metadata_used_bytes) {
        (Some(data), Some(meta)) => Some(shr_command::render::human_bytes(data.saturating_add(meta))),
        _ => None,
    });
    let free = df_row
        .and_then(|g| g.unallocated_bytes)
        .map(shr_command::render::human_bytes);

    Row::new([
        Cell::from(group.name.clone()),
        Cell::from(group.mount_point.clone()),
        Cell::from(group.fs_uuid.clone().unwrap_or_else(|| "-".to_string())),
        Cell::from(compression),
        Cell::from(shr_command::render::human_bytes(group.usable_bytes)),
        Cell::from(used.unwrap_or_else(|| "?".to_string())),
        Cell::from(free.unwrap_or_else(|| "?".to_string())),
        Cell::from(if group.resize_pending { "pending" } else { "ok" })
            .style(Style::default().fg(resize_color)),
    ])
}

/// Recent kernel log lines (`journalctl -k`) -- the honest substitute
/// for a dedicated shr-rs log store the current schema doesn't have (see
/// `Inspector::recent_log_lines`'s doc comment).
fn render_logs(frame: &mut Frame, app: &App, area: Rect) {
    let logs = app.logs();
    let text = if logs.is_empty() {
        Text::from("(no recent kernel log lines)")
    } else {
        Text::from(
            logs.iter()
                .map(|line| Line::from(line.as_str()))
                .collect::<Vec<_>>(),
        )
    };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Kernel log (last {})", logs.len())),
        ),
        area,
    );
}

fn render_array_table<'a>(
    frame: &mut Frame,
    arrays: impl Iterator<Item = &'a ArrayStatus>,
    area: Rect,
    title: &str,
) {
    let header = Row::new(["Device", "Level", "State", "Members", "Sync"])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows = arrays.map(|array| {
        let active = array.active_disks.unwrap_or(array.members.len());
        let expected = array.raid_disks.unwrap_or(array.members.len());
        let state_color = if array_needs_attention(array) {
            WARNING
        } else {
            GOOD
        };
        let sync = array.sync.as_ref().map_or_else(
            || "idle".into(),
            |sync| match sync.percent {
                Some(percent) => format!("{} {percent:.1}%", sync.action),
                None => sync.action.clone(),
            },
        );
        Row::new([
            Cell::from(format!("/dev/{}", array.name)),
            Cell::from(array.level.as_deref().unwrap_or("-").to_ascii_uppercase()),
            Cell::from(array.state.clone()).style(Style::default().fg(state_color)),
            Cell::from(format!(
                "{active}/{expected}\n{}",
                if array.members.is_empty() {
                    "-".into()
                } else {
                    // Was a plain `array.members.join(", ")` -- ignored
                    // `member_states` entirely, so the TUI showed strictly
                    // less than the CLI's `status --detail` (which already
                    // marks `(F)`/`(S)`, and after that fix `(W)`/`(R)` too).
                    // Shares the CLI's own helper rather than re-implementing
                    // the same mapping a second time.
                    shr_command::render::annotated_members(&array.members, &array.member_states).join(", ")
                }
            )),
            Cell::from(sync),
        ])
        .height(2)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(15),
            Constraint::Length(10),
            Constraint::Length(13),
            Constraint::Min(25),
            Constraint::Length(20),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(table, area);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(" [1-7/Tab] view   [r] refresh   [a] add disk   [x] replace disk   [s] scrub   [f] finish expansion   [q/Esc] quit ")
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center),
        area,
    );
}

fn health_label(health: &Health) -> (&'static str, Color) {
    match health {
        Health::Healthy => ("HEALTHY", GOOD),
        Health::Degraded => ("DEGRADED", WARNING),
        Health::Unknown => ("UNKNOWN", MUTED),
    }
}

fn smart_label(state: &SmartState) -> (&'static str, Color) {
    match state {
        SmartState::Ok => ("ok", GOOD),
        SmartState::Warning => ("WARNING", WARNING),
        SmartState::Unknown => ("unknown", MUTED),
    }
}

/// The Add Disk wizard, drawn as a centered modal on top of whatever tab is
/// showing underneath (`Clear` wipes the popup area first so table rows
/// beneath it don't bleed through). Constraint 5's "more than one click"
/// shows up here as the plan text + typed-confirmation requirement on the
/// `Confirm` step, not just a single OK button.
fn render_wizard_overlay(frame: &mut Frame, wizard: &WizardView, area: Rect) {
    let popup = centered_rect(80, 80, area);
    frame.render_widget(Clear, popup);

    let body = match wizard.step() {
        Step::SelectDisks => wizard_select_disks_text(wizard),
        Step::Preflight => wizard_preflight_text(wizard),
        Step::Preview => Text::from(vec![Line::from(
            "Safety checks passed. Press Enter to preview the execution plan (nothing has been changed yet).",
        )]),
        Step::ScrubCheckWarning => wizard_scrub_check_warning_text(wizard),
        Step::Confirm => wizard_confirm_text(wizard),
        Step::Executing => Text::from(vec![
            Line::from(Span::styled(
                "Running -- do not force-quit the TUI.",
                Style::default().fg(WARNING).bold(),
            )),
            Line::from("The expansion is running in the background. Rebuilding a real array can take hours."),
        ]),
        Step::Done => wizard_done_text(wizard),
        Step::Error => Text::from(vec![
            Line::from(Span::styled("Failed", Style::default().fg(DANGER).bold())),
            Line::from(wizard.controller_state.error_message.clone().unwrap_or_default()),
            Line::from(""),
            Line::from(Span::styled("[Esc] close", Style::default().fg(MUTED))),
        ]),
    };

    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" Add Disk -- group `{}` ", wizard.group_name)),
        ),
        popup,
    );
}

fn wizard_select_disks_text(wizard: &WizardView) -> Text<'static> {
    let mut lines = vec![
        Line::from("Select disk(s) to add ([Space] toggle, [Up/Down] move, [Enter] run safety checks, [Esc] cancel):"),
        Line::from(""),
    ];
    if wizard.candidate_disks.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no disks detected)",
            Style::default().fg(MUTED),
        )));
    }
    for (i, disk) in wizard.candidate_disks.iter().enumerate() {
        let cursor = if i == wizard.cursor { ">" } else { " " };
        // A system disk is never selectable, so it never shows "[x]"
        // even if somehow present in `selected` -- the mark reflects reality
        // (`app.rs::handle_wizard_key` already refuses to put it there),
        // this just keeps the row itself from implying otherwise.
        let mark = if !disk.system_disk && wizard.selected.contains(&disk.name) {
            "[x]"
        } else {
            "[ ]"
        };
        let style = if i == wizard.cursor {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans = vec![Span::styled(format!("{cursor} {mark} /dev/{}", disk.name), style)];
        if disk.system_disk {
            // Same intent as Cockpit's `createGroupWizard.tsx`
            // "chip chip--system" so the two frontends warn about the
            // same thing, even though an earlier fix put them in different languages
            // (Cockpit Korean, TUI English) rather than sharing literal text.
            spans.push(Span::styled(
                " (system disk -- not selectable)",
                Style::default().fg(DANGER),
            ));
        }
        lines.push(Line::from(spans));
    }
    if let Some(reason) = &wizard.selection_blocked_reason {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            reason.clone(),
            Style::default().fg(DANGER),
        )));
    }
    Text::from(lines)
}

/// This used to tell the operator to leave the TUI ("use shr-rs
/// --force-content from the CLI to override") -- exactly the recurring
/// TUI-lacks-CLI-parity defect (...). The override now
/// exists in the TUI itself (`o` at this step, see `app.rs::
/// handle_wizard_key`'s `Step::Preflight` arm), so the hint is only shown
/// when it would actually help: purely a display decision over the
/// already-computed `report.blockers` (no safety logic lives here -- the
/// backend's retried `preflight_write_targets` call is still the only thing
/// that decides whether the override clears the block).
fn wizard_preflight_text(wizard: &WizardView) -> Text<'static> {
    let mut lines = vec![Line::from(Span::styled(
        "Safety checks failed -- cannot continue:",
        Style::default().fg(DANGER).bold(),
    ))];
    let mut has_content_blocker = false;
    if let Some(report) = &wizard.controller_state.preflight {
        for blocker in &report.blockers {
            if matches!(blocker, WriteBlocker::HasContent { .. }) {
                has_content_blocker = true;
            }
            lines.push(Line::from(format!("  BLOCK: {blocker}")));
        }
        for warning in &report.warnings {
            lines.push(Line::from(format!("  WARN:  {warning}")));
        }
    }
    lines.push(Line::from(""));
    let hint = if has_content_blocker {
        "[o] force -- reuse this disk anyway (existing content will be lost)   [Esc] close"
    } else {
        "[Esc] close"
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(MUTED))));
    Text::from(lines)
}

/// `expand()`'s pre-reshape scrub-freshness block, shown as a
/// distinct safety gate rather than a plain error -- the operator must
/// press `y` (never plain Enter, see `app.rs::handle_wizard_key`) to
/// explicitly accept the risk and retry, exactly mirroring `shr-cli`'s
/// `--skip-scrub-check` flag not being on by default.
fn wizard_scrub_check_warning_text(wizard: &WizardView) -> Text<'static> {
    let mut lines = vec![
        Line::from(Span::styled(
            "Scrub check blocked this expansion",
            Style::default().fg(WARNING).bold(),
        )),
        Line::from(""),
    ];
    if let Some(warning) = &wizard.controller_state.scrub_check_warning {
        lines.push(Line::from(warning.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[y] proceed anyway (skip the scrub check)   [Esc] cancel",
        Style::default().fg(MUTED),
    )));
    Text::from(lines)
}

fn wizard_confirm_text(wizard: &WizardView) -> Text<'static> {
    let mut lines = vec![
        Line::from(Span::styled(
            "This cannot be undone. The selected disk(s) will be partitioned and added to the array.",
            Style::default().fg(DANGER).bold(),
        )),
        Line::from(""),
    ];
    if let Some(commands) = Some(&wizard.controller_state.preview_commands).filter(|c| !c.is_empty()) {
        lines.push(Line::from(format!("Planned commands ({}):", commands.len())));
        for cmd in commands {
            lines.push(Line::from(Span::styled(
                format!("  {cmd}"),
                Style::default().fg(MUTED),
            )));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(format!(
        "Type the group name `{}` to confirm, then press Enter:",
        wizard.group_name
    )));
    lines.push(Line::from(Span::styled(
        format!("> {}", wizard.confirmation_input),
        Style::default().fg(ACCENT),
    )));
    Text::from(lines)
}

fn wizard_done_text(wizard: &WizardView) -> Text<'static> {
    let mut lines = vec![Line::from(Span::styled(
        "Done.",
        Style::default().fg(GOOD).bold(),
    ))];
    if let Some(state) = &wizard.controller_state.result {
        lines.push(Line::from(format!(
            "Group `{}` now spans {} band(s).",
            state.name,
            state.bands.len()
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[Esc] close",
        Style::default().fg(MUTED),
    )));
    Text::from(lines)
}

/// The Replace Disk wizard, same centered-modal treatment as
/// `render_wizard_overlay`. `ReplaceStep::Select` covers two picker screens
/// (old member, then new free disk) distinguished by `ReplaceView::
/// picking_new` -- there is no separate controller-level step for that half,
/// since both picks happen before `ReplaceDiskController::select` is ever
/// called (see `ReplaceOldCandidate`'s doc comment in `app.rs`).
fn render_replace_overlay(frame: &mut Frame, replace: &ReplaceView, area: Rect) {
    let popup = centered_rect(80, 80, area);
    frame.render_widget(Clear, popup);

    let body = match replace.step() {
        ReplaceStep::Select if !replace.picking_new => replace_pick_old_text(replace),
        ReplaceStep::Select => replace_pick_new_text(replace),
        ReplaceStep::Confirm => replace_confirm_text(replace),
        ReplaceStep::Executing => Text::from(vec![
            Line::from(Span::styled(
                "Running -- do not force-quit the TUI.",
                Style::default().fg(WARNING).bold(),
            )),
            Line::from(
                "The replace operation is running in the background. This can take a long time for a real copy.",
            ),
        ]),
        ReplaceStep::Done => replace_done_text(replace),
        ReplaceStep::Error => Text::from(vec![
            Line::from(Span::styled("Failed", Style::default().fg(DANGER).bold())),
            Line::from(replace.controller_state.error_message.clone().unwrap_or_default()),
            Line::from(""),
            Line::from(Span::styled("[Esc] close", Style::default().fg(MUTED))),
        ]),
    };

    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" Replace Disk -- group `{}` ", replace.group_name)),
        ),
        popup,
    );
}

fn replace_pick_old_text(replace: &ReplaceView) -> Text<'static> {
    let mut lines = vec![
        Line::from("Select the disk to retire ([Up/Down] move, [Enter] pick, [Esc] cancel):"),
        Line::from(""),
    ];
    if replace.old_candidates.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no member disks in this group)",
            Style::default().fg(MUTED),
        )));
    }
    for (i, candidate) in replace.old_candidates.iter().enumerate() {
        let cursor = if i == replace.old_cursor { ">" } else { " " };
        let style = if i == replace.old_cursor {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{cursor} {}", candidate.display),
            style,
        )));
    }
    Text::from(lines)
}

/// Mirrors `wizard_select_disks_text`'s system-disk marking exactly --
/// same visible reason a system disk would otherwise look like any other
/// free disk in this list (`DiskStatus.arrays.is_empty()` doesn't exclude
/// it).
fn replace_pick_new_text(replace: &ReplaceView) -> Text<'static> {
    let mut lines = vec![
        Line::from(
            "Select the replacement disk ([Up/Down] move, [Enter] pick, [Backspace] back, [Esc] cancel):",
        ),
        Line::from(""),
    ];
    if replace.new_candidates.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no free disks detected)",
            Style::default().fg(MUTED),
        )));
    }
    for (i, candidate) in replace.new_candidates.iter().enumerate() {
        let cursor = if i == replace.new_cursor { ">" } else { " " };
        let style = if i == replace.new_cursor {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans = vec![Span::styled(format!("{cursor} /dev/{}", candidate.name), style)];
        if candidate.system_disk {
            spans.push(Span::styled(
                " (system disk -- not selectable)",
                Style::default().fg(DANGER),
            ));
        }
        lines.push(Line::from(spans));
    }
    if let Some(reason) = &replace.selection_blocked_reason {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            reason.clone(),
            Style::default().fg(DANGER),
        )));
    }
    Text::from(lines)
}

fn replace_confirm_text(replace: &ReplaceView) -> Text<'static> {
    let mut lines = vec![
        Line::from(Span::styled(
            "This cannot be undone. The old disk will be released and the new disk added in its place.",
            Style::default().fg(DANGER).bold(),
        )),
        Line::from(""),
    ];
    if let Some(commands) = Some(&replace.controller_state.preview_commands).filter(|c| !c.is_empty()) {
        lines.push(Line::from(format!("Planned commands ({}):", commands.len())));
        for cmd in commands {
            lines.push(Line::from(Span::styled(
                format!("  {cmd}"),
                Style::default().fg(MUTED),
            )));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(format!(
        "Type the group name `{}` to confirm, then press Enter:",
        replace.group_name
    )));
    lines.push(Line::from(Span::styled(
        format!("> {}", replace.confirmation_input),
        Style::default().fg(ACCENT),
    )));
    Text::from(lines)
}

fn replace_done_text(replace: &ReplaceView) -> Text<'static> {
    let mut lines = vec![Line::from(Span::styled(
        "Done.",
        Style::default().fg(GOOD).bold(),
    ))];
    if let Some(state) = &replace.controller_state.result {
        lines.push(Line::from(format!(
            "Group `{}` now spans {} band(s).",
            state.name,
            state.bands.len()
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[Esc] close",
        Style::default().fg(MUTED),
    )));
    Text::from(lines)
}

/// The scrub start/cancel controls. A single function covering every step
/// (unlike the Add Disk/Replace Disk wizards' per-step builders) -- this
/// modal's lifetime is much shorter (no multi-step picker, no preflight),
/// so one match arm per `scrub::Step` reads more directly than a family of
/// helper functions for what's a handful of short lines each.
fn render_scrub_overlay(frame: &mut Frame, scrub: &ScrubView, area: Rect) {
    let popup = centered_rect(60, 50, area);
    frame.render_widget(Clear, popup);

    let body = match scrub.step() {
        ScrubStep::ConfirmStart => Text::from(vec![
            Line::from(format!("Start a scrub on group `{}`?", scrub.group_name)),
            Line::from(""),
            Line::from(Span::styled(
                "[Enter] start   [Esc] cancel",
                Style::default().fg(MUTED),
            )),
        ]),
        // Requires the typed group name, same that distinct-confirmation
        // discipline as `AddDiskController::can_execute`/
        // `ReplaceDiskController::can_execute` -- cancelling a running scrub
        // is not a bare-keypress action.
        ScrubStep::ConfirmCancel => Text::from(vec![
            Line::from(Span::styled(
                "A scrub is running on this group. Cancelling stops it in place.",
                Style::default().fg(WARNING).bold(),
            )),
            Line::from(""),
            Line::from(format!(
                "Type the group name `{}` to confirm, then press Enter:",
                scrub.group_name
            )),
            Line::from(Span::styled(
                format!("> {}", scrub.confirmation_input),
                Style::default().fg(ACCENT),
            )),
            Line::from(""),
            Line::from(Span::styled("[Esc] cancel", Style::default().fg(MUTED))),
        ]),
        ScrubStep::Done => Text::from(vec![
            Line::from(Span::styled("Done.", Style::default().fg(GOOD).bold())),
            Line::from(""),
            Line::from(Span::styled("[Esc] close", Style::default().fg(MUTED))),
        ]),
        ScrubStep::Error => Text::from(vec![
            Line::from(Span::styled("Failed", Style::default().fg(DANGER).bold())),
            Line::from(scrub.controller_state.error_message.clone().unwrap_or_default()),
            Line::from(""),
            Line::from(Span::styled("[Esc] close", Style::default().fg(MUTED))),
        ]),
        // Not expected to actually render: `open_scrub` (`app.rs`) sets
        // `pending_action` to `RequestStart`/`RequestCancel` the same frame
        // the modal opens, so `runtime.rs` moves the controller state past
        // `Idle` before the next draw. Kept as a harmless placeholder rather
        // than an unreachable!() -- a `render` call is never allowed to
        // panic on a state it merely finds surprising.
        ScrubStep::Idle => Text::from(vec![Line::from("")]),
    };

    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" Scrub -- group `{}` ", scrub.group_name)),
        ),
        popup,
    );
}

/// `shr-rs reconcile` -- finishes any LVM/Btrfs resize a previous
/// `expand` had to defer while its mdadm reshape was still running (the
/// `resize_pending` warning the Groups/Bands/Fs tables already render).
/// Not scoped to one group (`shr-cli`'s `Command::Reconcile` takes no
/// `--name`), unlike every other modal in this file -- so, unlike
/// `render_wizard_overlay`/`render_replace_overlay`/`render_scrub_overlay`,
/// there is no group name to put in the title or body.
fn render_reconcile_overlay(frame: &mut Frame, reconcile: &ReconcileView, area: Rect) {
    let popup = centered_rect(60, 50, area);
    frame.render_widget(Clear, popup);

    let body = match reconcile.step() {
        // A single explicit confirm, not a bare keypress and not the
        // stronger typed-exact-name gate `AddDiskController`/
        // `ReplaceDiskController`/`ScrubController::confirm_cancel` use:
        // `reconcile` is idempotent and non-destructive (`engine.rs`'s own
        // doc comment on this call: "never starts a NEW destructive
        // action"), so this level of friction matches the actual risk.
        ReconcileStep::Confirm => Text::from(vec![
            Line::from("Finish the space-expansion step a previous expand left unfinished,"),
            Line::from("on every group. Safe to run even when nothing is pending."),
            Line::from(""),
            Line::from(Span::styled(
                "[Enter] finish expansion   [Esc] cancel",
                Style::default().fg(MUTED),
            )),
        ]),
        ReconcileStep::Executing => Text::from(vec![
            Line::from(Span::styled(
                "Running -- do not force-quit the TUI.",
                Style::default().fg(WARNING).bold(),
            )),
            Line::from("Growing the filesystem onto the new space can take a while."),
        ]),
        ReconcileStep::Done => {
            let mut lines = vec![
                Line::from(Span::styled("Done.", Style::default().fg(GOOD).bold())),
                Line::from(""),
            ];
            if reconcile.controller_state.performed.is_empty() {
                lines.push(Line::from("Nothing was pending."));
            } else {
                for action in &reconcile.controller_state.performed {
                    lines.push(Line::from(action.clone()));
                }
            }
            if let Some(pending) = &reconcile.controller_state.still_pending {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    pending.clone(),
                    Style::default().fg(WARNING),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[Esc] close",
                Style::default().fg(MUTED),
            )));
            Text::from(lines)
        }
        ReconcileStep::Error => Text::from(vec![
            Line::from(Span::styled("Failed", Style::default().fg(DANGER).bold())),
            Line::from(
                reconcile
                    .controller_state
                    .error_message
                    .clone()
                    .unwrap_or_default(),
            ),
            Line::from(""),
            Line::from(Span::styled("[Esc] close", Style::default().fg(MUTED))),
        ]),
    };

    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" Finish expansion "),
        ),
        popup,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [area] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DiskCandidate, Snapshot};
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
    use shr_command::{FsDfReport, GroupDfStatus, StatusReport};

    fn flatten(text: &Text) -> String {
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// A single-group report with a known compression setting -- enough to
    /// drive the FS tab, nothing else populated (the fixture only needs
    /// `render_fs`'s inputs, not a whole live host).
    fn fs_tab_fixture() -> StatusReport {
        StatusReport {
            schema_version: 2,
            health: Health::Healthy,
            disks: vec![],
            arrays: vec![],
            groups: vec![GroupStatus {
                name: "default".to_string(),
                mode: "shr".to_string(),
                layout_version: 1,
                mount_point: "/mnt/shr_data".to_string(),
                fs_uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
                usable_bytes: 8_000_000_000_000,
                resize_pending: false,
                disks: vec![],
                bands: vec![],
            }],
            state_path: None,
        }
    }

    /// `GroupStatus.compression` reached `status --json` but the TUI's
    /// FS tab -- which already renders `fs_uuid` -- never showed it. An
    /// operator deciding whether to run `fs recompress`, or checking whether
    /// it took effect, had no on-screen answer without dropping to the CLI's
    /// `--json` output.
    #[test]
    fn fs_tab_shows_compression_alongside_fs_uuid() {
        let app = App::new(fs_tab_fixture());
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_fs(frame, &app, area);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("zstd:3"), "{text}");
    }

    /// One group's worth of known live Btrfs usage -- `data_used_bytes`
    /// (2.0 TB) + `metadata_used_bytes` (1.0 TB) = 3.0 TB Used;
    /// `unallocated_bytes` (5.0 TB) shown as Free literally, not derived from
    /// `usable_bytes - used` (see `render_fs_row`'s doc comment for why).
    fn fs_df_with_known_usage() -> FsDfReport {
        FsDfReport {
            schema_version: 2,
            groups: vec![GroupDfStatus {
                name: "default".to_string(),
                mount_point: "/mnt/shr_data".to_string(),
                usable_bytes: 8_000_000_000_000,
                data_used_bytes: Some(2_000_000_000_000),
                data_total_bytes: None,
                metadata_used_bytes: Some(1_000_000_000_000),
                metadata_total_bytes: None,
                unallocated_bytes: Some(5_000_000_000_000),
                statvfs_avail_bytes: None,
            }],
        }
    }

    /// The FS tab showed `Usable` capacity but nothing about how full a
    /// filesystem actually is, unlike `shr-rs fs df` (CLI) and Cockpit. Used
    /// combines `data_used_bytes` + `metadata_used_bytes` (mirrors Cockpit's
    /// `summarizeCapacityUsage` combination rule); Free is `unallocated_bytes`
    /// verbatim.
    #[test]
    fn fs_tab_shows_used_and_free_capacity_for_a_group_with_known_usage() {
        let mut app = App::new(fs_tab_fixture());
        app.replace_snapshot(Snapshot {
            report: fs_tab_fixture(),
            logs: vec![],
            fs_df: fs_df_with_known_usage(),
        });
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_fs(frame, &app, area);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("3.0 TB"), "Used column missing: {text}");
        assert!(text.contains("5.0 TB"), "Free column missing: {text}");
    }

    /// Honesty requirement: before the first live Btrfs fetch completes
    /// (or if the underlying `btrfs`/`df` call fails), Used/Free must show
    /// `?`, never a fabricated number and never a stale value silently
    /// presented as current.
    #[test]
    fn fs_tab_shows_unknown_marker_not_a_fabricated_number_when_usage_is_not_yet_known() {
        let app = App::new(fs_tab_fixture());
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_fs(frame, &app, area);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains('?'), "{text}");
    }

    fn line_containing<'a>(text: &'a Text, needle: &str) -> &'a Line<'a> {
        text.lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
            .unwrap_or_else(|| panic!("no line contains {needle:?}"))
    }

    /// The TUI's Add Disk wizard must mark the system disk the same way
    /// Cockpit's create wizard does -- silently offering it identically
    /// to a data disk is exactly the defect being fixed here.
    #[test]
    fn system_disk_row_is_marked_and_a_normal_disk_row_is_not() {
        let wizard = WizardView {
            candidate_disks: vec![
                DiskCandidate {
                    name: "vda".into(),
                    system_disk: true,
                },
                DiskCandidate {
                    name: "vdb".into(),
                    system_disk: false,
                },
            ],
            ..Default::default()
        };
        let text = wizard_select_disks_text(&wizard);
        let flat = flatten(&text);
        assert!(flat.contains("vda"), "{flat}");
        assert!(
            flat.contains("system disk"),
            "system disk row must carry a visible marker: {flat}"
        );

        let vdb_line = line_containing(&text, "vdb");
        let vdb_flat: String = vdb_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !vdb_flat.contains("system disk"),
            "a non-system disk must not carry the system marker: {vdb_flat}"
        );
    }

    /// When the operator tries to select the system disk, the wizard
    /// must say why near the list -- same place `wizard_preflight_text`
    /// shows its BLOCK/WARN lines, not a separate dialog.
    #[test]
    fn selection_blocked_reason_is_rendered_near_the_list() {
        let wizard = WizardView {
            candidate_disks: vec![DiskCandidate {
                name: "vda".into(),
                system_disk: true,
            }],
            selection_blocked_reason: Some(
                "The system disk cannot be selected: /dev/vda -- the OS is running on this disk.".into(),
            ),
            ..Default::default()
        };
        let text = wizard_select_disks_text(&wizard);
        let flat = flatten(&text);
        assert!(flat.contains("cannot be selected"), "{flat}");
    }

    /// The old text told the operator to leave the TUI ("use shr-rs
    /// --force-content from the CLI to override") -- that became false once
    /// the TUI grew its own override, and leaving it would itself repeat
    /// this project's recurring TUI-lacks-CLI-parity defect. Only shown
    /// when it would actually help (a real `HasContent` blocker present).
    #[test]
    fn preflight_text_offers_the_o_override_only_when_a_has_content_blocker_is_present() {
        use crate::wizard::WizardState;
        use shr_inspect::WritePreflight;

        let wizard_blocked_by_content = WizardView {
            controller_state: WizardState {
                preflight: Some(WritePreflight {
                    ok: false,
                    blockers: vec![WriteBlocker::HasContent { name: "vdb".into() }],
                    warnings: vec!["disk `vdb` already has partitions or a filesystem signature".into()],
                    targets: vec![],
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let flat = flatten(&wizard_preflight_text(&wizard_blocked_by_content));
        assert!(
            flat.contains("[o]"),
            "a HasContent blocker must offer the override: {flat}"
        );
        assert!(
            !flat.contains("from the CLI"),
            "the stale CLI-redirect hint must be gone now that the TUI has its own override: {flat}"
        );

        let wizard_blocked_for_another_reason = WizardView {
            controller_state: WizardState {
                preflight: Some(WritePreflight {
                    ok: false,
                    blockers: vec![WriteBlocker::NoStableId { name: "vdb".into() }],
                    warnings: vec![],
                    targets: vec![],
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let flat = flatten(&wizard_preflight_text(&wizard_blocked_for_another_reason));
        assert!(
            !flat.contains("[o]"),
            "the override hint must not be offered when it wouldn't help (no HasContent blocker): {flat}"
        );
    }

    // --- SMART detail, fault-tolerance-remaining, system-disk marker
    // -- all three already computed/received but never shown on the TUI's
    // Disks/Groups tabs, while Cockpit already shows all three. ------------

    use shr_command::{report::MemberStatus, DiskStatus, GroupBandStatus, SmartSummary};

    fn disk_fixture(name: &str, smart: SmartSummary) -> DiskStatus {
        DiskStatus {
            name: name.to_string(),
            id: None,
            size: Some(4_000_000_000_000),
            model: Some("Model X".into()),
            serial: Some("SN1".into()),
            rotational: Some(true),
            smart,
            arrays: vec![],
            system_disk: false,
            system_mounts: vec![],
        }
    }

    fn disks_report(disks: Vec<DiskStatus>) -> StatusReport {
        StatusReport {
            schema_version: 2,
            health: Health::Healthy,
            disks,
            arrays: vec![],
            groups: vec![],
            state_path: None,
        }
    }

    /// Hangul (and CJK generally) renders two cells wide; `Buffer::set_stringn`
    /// resets the cell right after every such grapheme, and `Cell::symbol()`
    /// reports a reset cell as a literal `" "` -- indistinguishable from a
    /// real space once cells are concatenated naively. Skipping the
    /// continuation cell (rather than collapsing repeated whitespace, which
    /// does not apply here: there is exactly one artifact space per wide
    /// grapheme) reconstructs what a human actually reads on screen.
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

    fn render_disks_text(report: StatusReport, width: u16) -> String {
        let app = App::new(report);
        let backend = TestBackend::new(width, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_disks(frame, &app, frame.area()))
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// Known-value case: `report.rs::SmartSummary`'s own doc comment
    /// says the detail fields exist "so the UI can show e.g. '1 Pending
    /// Sector'" -- only Cockpit ever did (`panels.tsx:137-144`). A genuine
    /// zero (`reallocated_sectors: Some(0)`) must not print a count either
    /// -- Cockpit's own `disk.smart.reallocated_sectors ? ... : null` treats
    /// a real zero as "nothing to report", and this mirrors that rather than
    /// diverging from the reference frontend.
    #[test]
    fn disks_tab_shows_known_smart_detail_and_hides_a_genuine_zero_count() {
        let text = render_disks_text(
            disks_report(vec![disk_fixture(
                "sda",
                SmartSummary {
                    state: SmartState::Warning,
                    temperature_c: Some(41),
                    power_on_hours: Some(1200),
                    pending_sectors: Some(3),
                    reallocated_sectors: Some(0),
                    uncorrectable_sectors: None,
                    nvme_critical_warning: None,
                },
            )]),
            150,
        );
        assert!(
            text.contains("3 pending sectors"),
            "known pending-sector count must render: {text}"
        );
        assert!(text.contains("1200h"), "known power-on-hours must render: {text}");
        assert!(
            !text.contains("reallocated"),
            "a genuine zero reallocated-sector count must not print: {text}"
        );
        assert!(
            !text.contains("uncorrectable"),
            "uncorrectable is None here, must not appear: {text}"
        );
    }

    /// Honesty requirement: with every detail field `None`, nothing
    /// is shown -- never a fabricated "0 pending sectors" etc. Mirrors the
    /// `?`-marker precedent's spirit: unknown must never look like a
    /// measured zero.
    #[test]
    fn disks_tab_shows_no_fabricated_smart_detail_when_every_field_is_unknown() {
        let text = render_disks_text(
            disks_report(vec![disk_fixture(
                "sda",
                SmartSummary {
                    state: SmartState::Unknown,
                    temperature_c: None,
                    power_on_hours: None,
                    pending_sectors: None,
                    reallocated_sectors: None,
                    uncorrectable_sectors: None,
                    nvme_critical_warning: None,
                },
            )]),
            150,
        );
        assert!(!text.contains("pending sectors"), "{text}");
        assert!(!text.contains("reallocated"), "{text}");
        assert!(!text.contains("uncorrectable"), "{text}");
        assert!(!text.contains("NVMe"), "{text}");
        // Power-on hours renders as e.g. "1200h" -- a digit immediately
        // followed by 'h' -- when known. Checking for the bare letter 'h'
        // would be too broad (other words in this render legitimately
        // contain it), so scan for the actual pattern the formatter
        // produces instead.
        assert!(
            !contains_digit_then_h(&text),
            "power-on hours must not appear when unknown: {text}"
        );
        assert!(
            text.contains("unknown"),
            "the SMART state label itself must still render: {text}"
        );
    }

    fn contains_digit_then_h(s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        chars.windows(2).any(|w| w[0].is_ascii_digit() && w[1] == 'h')
    }

    /// Known-value case: the system-disk marker previously appeared
    /// only inside the Add/Replace-Disk wizard overlays -- finding the OS
    /// disk required opening a destructive-action wizard. Wording matches the
    /// intent behind Cockpit's disk-row marker (`panels.tsx`), though the
    /// two frontends are in different languages, so the literal text no
    /// longer matches. A non-system disk in the same report must never carry
    /// it.
    #[test]
    fn disks_tab_marks_the_system_disk_and_its_mounts_without_a_wizard() {
        let ok_smart = SmartSummary {
            state: SmartState::Ok,
            temperature_c: None,
            power_on_hours: None,
            pending_sectors: None,
            reallocated_sectors: None,
            uncorrectable_sectors: None,
            nvme_critical_warning: None,
        };
        let mut system_disk = disk_fixture("vda", ok_smart.clone());
        system_disk.system_disk = true;
        system_disk.system_mounts = vec!["/".to_string(), "/boot".to_string()];
        let data_disk = disk_fixture("vdb", ok_smart);

        let text = render_disks_text(disks_report(vec![system_disk, data_disk]), 150);
        assert_eq!(
            text.matches("SYSTEM DISK").count(),
            1,
            "exactly one row (the system disk) may carry the marker: {text}"
        );
        assert!(
            text.contains("/boot"),
            "the system disk's mounts must render: {text}"
        );
    }

    /// Regression, found on real hardware and NOT by the test above.
    /// That test used a two-entry mount list (`/`, `/boot`), which fit. The
    /// actual guest reports three (`/`, `/boot`, `/boot/efi`), and with the
    /// mounts appended to the marker on one line the cell measured 54
    /// display cells (the marker was Korean at the time, double-width)
    /// against a 48-cell column -- the pty showed
    /// `시스템디스크--건드리지마세요·/,/boot,/bo`, silently truncating the
    /// last mount. Fixed by giving the mount list its own line; this asserts
    /// the LAST mount survives, which is the one truncation eats first. The
    /// marker text was later replaced with English, but the layout property
    /// under test -- the marker renders whole and the mount line is never
    /// clipped -- is unchanged, so this test still exercises it.
    #[test]
    fn disks_tab_does_not_truncate_a_realistic_three_entry_mount_list() {
        let ok_smart = SmartSummary {
            state: SmartState::Ok,
            temperature_c: None,
            power_on_hours: None,
            pending_sectors: None,
            reallocated_sectors: None,
            uncorrectable_sectors: None,
            nvme_critical_warning: None,
        };
        let mut system_disk = disk_fixture("vda", ok_smart);
        system_disk.system_disk = true;
        system_disk.system_mounts = vec!["/".to_string(), "/boot".to_string(), "/boot/efi".to_string()];

        let text = render_disks_text(disks_report(vec![system_disk]), 150);

        assert!(
            text.contains("SYSTEM DISK -- OS runs here, do not touch"),
            "the marker itself must render whole, not clipped mid-phrase: {text}"
        );
        assert!(
            text.contains("/boot/efi"),
            "the LAST mount is what truncation eats first -- it must survive: {text}"
        );
    }

    /// Honesty requirement: a system disk with no observed mounts
    /// (`system_mounts` empty) must still show the marker itself but must
    /// never invent a mount path to go with it.
    #[test]
    fn disks_tab_never_fabricates_a_mount_path_for_a_system_disk_with_none_observed() {
        let ok_smart = SmartSummary {
            state: SmartState::Ok,
            temperature_c: None,
            power_on_hours: None,
            pending_sectors: None,
            reallocated_sectors: None,
            uncorrectable_sectors: None,
            nvme_critical_warning: None,
        };
        let mut system_disk = disk_fixture("vda", ok_smart);
        system_disk.system_disk = true;
        // system_mounts left empty -- e.g. mount detection failed.

        let text = render_disks_text(disks_report(vec![system_disk]), 150);
        assert!(
            text.contains("SYSTEM DISK"),
            "the marker itself must still show: {text}"
        );
    }

    fn tolerance_group(mode: &str, bands: Vec<GroupBandStatus>) -> GroupStatus {
        GroupStatus {
            name: "g".into(),
            mode: mode.into(),
            layout_version: 1,
            mount_point: "/mnt/g".into(),
            fs_uuid: None,
            vg_name: "vg".into(),
            lv_name: "data".into(),
            compression: "zstd:3".into(),
            usable_bytes: 1,
            resize_pending: false,
            disks: vec![],
            bands,
        }
    }

    fn member(name: &str, faulty: bool) -> MemberStatus {
        MemberStatus {
            name: name.into(),
            role: Some(0),
            faulty,
            spare: false,
            write_mostly: false,
            replacement: false,
        }
    }

    fn band(md_name: &str, member_states: Vec<MemberStatus>) -> GroupBandStatus {
        GroupBandStatus {
            index: 0,
            level: "raid6".into(),
            md_name: md_name.into(),
            md_uuid: None,
            usable_bytes: 1,
            resize_pending: false,
            members: member_states.iter().map(|m| m.name.clone()).collect(),
            member_states,
            sync: None,
            last_scrub: None,
            scrub_in_progress: false,
            pending_member_removal: None,
            ..Default::default()
        }
    }

    fn render_groups_text(report: StatusReport, width: u16) -> String {
        let app = App::new(report);
        let backend = TestBackend::new(width, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_groups(frame, &app, frame.area()))
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// Known-value case, ported faithfully from Cockpit's
    /// `groupToleranceStatus` (`model.ts:862-882`): a 2-band SHR-2
    /// group (nominal tolerance 2) where band 0 (the worse one) has one
    /// faulty member and band 1 is fully healthy. The GROUP's remaining
    /// margin must be driven by the worst band (1), not an average/sum
    /// across bands, and not band 1's healthier count.
    #[test]
    fn groups_tab_shows_remaining_tolerance_driven_by_the_worst_band() {
        let group = tolerance_group(
            "shr2",
            vec![
                band(
                    "md0",
                    vec![
                        member("a", true),
                        member("b", false),
                        member("c", false),
                        member("d", false),
                    ],
                ),
                band("md1", vec![member("e", false), member("f", false)]),
            ],
        );
        let report = StatusReport {
            schema_version: 2,
            health: Health::Degraded,
            disks: vec![],
            arrays: vec![],
            groups: vec![group],
            state_path: None,
        };
        let text = render_groups_text(report, 140);
        assert!(
            text.contains("1 remaining"),
            "remaining margin (2 - 1 faulty) must render: {text}"
        );
        assert!(
            text.contains("designed for 2-disk loss"),
            "nominal SHR-2 tolerance must render: {text}"
        );
    }

    /// Honesty requirement, also ported from `groupToleranceStatus`:
    /// when ANY band lacks live `member_states` (empty vec -- see
    /// `GroupBandStatus::member_states`'s doc comment on what empty means),
    /// the group's remaining margin is unknown, not assumed healthy. This
    /// must never render as "tolerates 1-disk loss" (the fully-healthy
    /// label) when the truth is "we don't know".
    #[test]
    fn groups_tab_shows_unknown_marker_not_a_fabricated_full_tolerance_when_a_band_has_no_live_data() {
        let group = tolerance_group("shr", vec![band("md0", vec![])]);
        let report = StatusReport {
            schema_version: 2,
            health: Health::Unknown,
            disks: vec![],
            arrays: vec![],
            groups: vec![group],
            state_path: None,
        };
        let text = render_groups_text(report, 140);
        assert!(
            text.contains("no live member data"),
            "must say live member data is missing, not assume full health: {text}"
        );
        // English's "tolerates N-disk loss" (bare healthy label) is NOT a
        // substring of "designed for N-disk loss (no live member data)" the
        // way the earlier Korean strings were (there the bare label sat
        // literally inside the fuller sentence, requiring a position check
        // rather than a plain `contains`). Still verify by finding the
        // "-disk loss" phrase and checking what precedes it, so a future
        // wording change that reintroduces the same collision keeps this
        // test honest rather than silently trusting `contains`.
        let label_at = text
            .find("-disk loss")
            .expect("tolerance label must render: {text}");
        assert!(
            text[..label_at].ends_with("designed for 1"),
            "the tolerance phrase must only appear inside the unknown-data sentence, never as the bare fully-healthy label: {text}"
        );
    }

    /// A band whose mdadm array has been stopped/lost (e.g. a reboot
    /// where loopback devices/mdadm arrays don't come back but `state.toml`
    /// survives) must not render identically to a live, healthy band. Ports
    /// the CLI's `render_band_detail_row` guard to the TUI's Bands
    /// tab, which had none at all -- `render_array_table` (its sibling table
    /// on the Arrays tab) already checked `array.members.is_empty()`, but
    /// `render_bands` printed `band.level`/`md_name`/`usable_bytes`
    /// unconditionally regardless of whether `GroupBandStatus::members` was
    /// empty.
    #[test]
    fn bands_tab_marks_a_band_with_no_live_mdadm_array() {
        let group = tolerance_group("shr", vec![band("md0", vec![])]);
        let report = StatusReport {
            schema_version: 2,
            health: Health::Unknown,
            disks: vec![],
            arrays: vec![],
            groups: vec![group],
            state_path: None,
        };
        let app = App::new(report);
        let backend = TestBackend::new(140, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_bands(frame, &app, frame.area()))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("no live RAID array"),
            "a band with no live mdadm array must say so, not render as a normal idle band: {text}"
        );
    }

    /// Regression guard: none of the TUI's operator-facing strings may
    /// contain a Hangul-range character (U+AC00-U+D7A3) ever again -- the
    /// TUI is English-only, while Cockpit stays Korean (the design
    ///, M10; this split is a firm decision, not to be re-litigated).
    /// Comments are exempt: this file intentionally keeps a couple of
    /// historical Korean quotes in doc comments (e.g. the literal garbled
    /// pty output quoted in `disks_tab_does_not_truncate_a_realistic_
    /// three_entry_mount_list`'s doc comment above, which documents a real
    /// defect precisely) -- only the non-comment portion of each line is
    /// checked. Covers every TUI source file, not just this one.
    #[test]
    fn no_hangul_outside_comments_in_owned_tui_files() {
        const FILES: &[(&str, &str)] = &[
            ("src/ui.rs", include_str!("ui.rs")),
            ("src/app.rs", include_str!("app.rs")),
            ("src/wizard.rs", include_str!("wizard.rs")),
            ("src/scrub.rs", include_str!("scrub.rs")),
            ("src/runtime.rs", include_str!("runtime.rs")),
            ("tests/app.rs", include_str!("../tests/app.rs")),
            ("tests/scrub.rs", include_str!("../tests/scrub.rs")),
        ];
        for (name, source) in FILES {
            for (i, line) in source.lines().enumerate() {
                let code_part = match line.find("//") {
                    Some(idx) => &line[..idx],
                    None => line,
                };
                assert!(
                    !code_part.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)),
                    "{name}:{}: a Hangul-range character survives outside a comment -- \
                     the TUI is English-only: {line}",
                    i + 1
                );
            }
        }
    }
}
