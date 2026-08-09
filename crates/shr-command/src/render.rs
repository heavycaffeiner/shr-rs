//! Human-readable ASCII rendering of the reports (the CLI's non-JSON output).

use std::fmt::Write as _;

use shr_core::{Disk, DiskId, PlannerOutput};

use crate::report::{
    ArrayStatus, DiskStatus, FsDfReport, GroupBandStatus, GroupDfStatus, GroupStatus, Health, MemberStatus,
    PlanReport, ScrubOutcome, SmartState, StatusReport,
};

/// Format a byte count in decimal units (matching the plan/mockup, e.g. 4.0 TB).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, f64); 5] = [("PB", 1e15), ("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)];
    let b = bytes as f64;
    for (unit, factor) in UNITS {
        if b >= factor {
            return format!("{:.1} {}", b / factor, unit);
        }
    }
    format!("{bytes} B")
}

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("-")
}

/// Render a status report as a couple of aligned tables.
#[allow(clippy::write_literal)] // column headers are padded literals by design
pub fn render_status(r: &StatusReport) -> String {
    let mut out = String::new();
    let health = match r.health {
        Health::Healthy => "● HEALTHY",
        Health::Degraded => "▲ DEGRADED",
        Health::Unknown => "○ UNKNOWN (no RAID array found)",
    };
    let _ = writeln!(out, "SHR-RS status: {health}");
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "Disks ({}):  {:<10}{:>10}  {:<24}{:<10}{:<8}{}",
        r.disks.len(),
        "NODE",
        "SIZE",
        "MODEL",
        "SMART",
        "TEMP",
        "ARRAYS"
    );
    for d in &r.disks {
        let size = d.size.map(human_bytes).unwrap_or_else(|| "?".into());
        let smart = match d.smart.state {
            SmartState::Ok => "ok",
            SmartState::Warning => "WARN",
            SmartState::Unknown => "?",
        };
        let temp = d
            .smart
            .temperature_c
            .map(|t| format!("{t}C"))
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "             {:<10}{:>10}  {:<24}{:<10}{:<8}{}",
            d.name,
            size,
            opt(&d.model),
            smart,
            temp,
            d.arrays.join(",")
        );
    }

    if !r.arrays.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Arrays ({}): {:<8}{:<8}{:<10}{:<8}{}",
            r.arrays.len(),
            "NAME",
            "LEVEL",
            "STATE",
            "DISKS",
            "PROGRESS"
        );
        for a in &r.arrays {
            let _ = writeln!(out, "             {}", render_array_row(a));
        }
    }

    // Deliberately rendered even when empty is skipped -- a fresh host with
    // no state.toml has `groups == []`, and printing nothing here (rather
    // than e.g. "Groups (0):") matches how the disks/arrays sections above
    // already omit themselves when there's nothing to show.
    if !r.groups.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            // No VERSION column: `layout_version` is an internal on-disk
            // revision number. Disk count is what a human scanning this
            // table actually wants next to the mode.
            "Groups ({}): {:<10}{:<8}{:<8}{:<10}{}",
            r.groups.len(),
            "NAME",
            "MODE",
            "DISKS",
            "USABLE",
            "MOUNT"
        );
        for g in &r.groups {
            let _ = writeln!(out, "             {}", render_group_row(g));
            for b in &g.bands {
                let pending = if b.resize_pending {
                    " [expansion unfinished]"
                } else {
                    ""
                };
                // Real reboot observation -- state.toml survived a hard
                // reboot, loopback devices/mdadm arrays did not, and this
                // (plain, DEFAULT) view printed `band0 raid5 md0  17.2 GB`
                // with no marker that `md0` doesn't exist. `--detail`'s
                // `render_band_detail_row` already had this guard
                // (`b.members.is_empty()` == "no live mdadm array with this
                // name right now", see `GroupBandStatus::members`'s doc
                // comment); it just never reached this sibling path.
                let live = if b.members.is_empty() {
                    " (no live mdadm array)"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "                 band{:<3}{:<8}{:<12}{:>12}{}{}",
                    b.index,
                    b.level,
                    b.md_name,
                    human_bytes(b.usable_bytes),
                    pending,
                    live
                );
            }
        }
    }

    out
}

fn render_group_row(g: &GroupStatus) -> String {
    let resize = if g.resize_pending {
        " [expansion unfinished]"
    } else {
        ""
    };
    format!(
        "{:<10}{:<8}{:<8}{:<10}{}{}",
        g.name,
        g.mode,
        g.disks.len(),
        human_bytes(g.usable_bytes),
        g.mount_point,
        resize
    )
}

fn render_array_row(a: &ArrayStatus) -> String {
    let level = a.level.clone().unwrap_or_else(|| "-".into());
    let mut state = a.state.clone();
    if a.read_only {
        state.push_str(",ro");
    }
    if a.degraded {
        state.push_str(",degraded");
    }
    let disks = match (a.active_disks, a.raid_disks) {
        (Some(act), Some(tot)) => format!("{act}/{tot}"),
        _ => format!("{}", a.members.len()),
    };
    let progress = a
        .sync
        .as_ref()
        .map(|s| match s.percent {
            Some(p) => format!("{} {:.1}%", s.action, p),
            None => format!("{} pending", s.action),
        })
        .unwrap_or_else(|| "idle".into());
    format!("{:<8}{:<8}{:<10}{:<8}{}", a.name, level, state, disks, progress)
}

/// Truncate `s` (with a trailing `>`) to fit `width` columns -- keeps a
/// column's width independent of arbitrary input data (long models, serials,
/// timestamps). Generic sibling of [`short_label`] for plain strings.
fn bounded(s: &str, width: usize) -> String {
    if s.chars().count() <= width || width == 0 {
        return s.to_string();
    }
    let head: String = s.chars().take(width - 1).collect();
    format!("{head}>")
}

fn opt_count(n: Option<u64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
}

/// Render a more detailed status view than [`render_status`]: a wider SMART
/// table, per-band RAID detail (mdadm device, live members, usable capacity,
/// live resync/check/reshape progress), per-band scrub history, and any disk
/// not currently backing an array ("unassigned"). Same discipline as every
/// other renderer here: pure, ASCII-only, and an unknown field prints `?`
/// rather than a guess (see `StatusReport`'s doc comments for what "unknown"
/// means at each field).
///
/// Note on estimated completion time: `SyncSummary::finish_min`
/// is `/proc/mdstat`'s own "minutes remaining" estimate -- this function
/// reports that directly ("finish ~N min") rather than adding it to a
/// wall-clock "now" to print an absolute time, since this module takes no
/// wall-clock input (pure function of the report alone, like every other
/// renderer here) and turning a relative estimate into an absolute one is a
/// one-line addition for whichever caller already has "now" (the CLI's watch
/// loop).
pub fn render_status_detail(r: &StatusReport) -> String {
    let mut out = String::new();
    let health = match r.health {
        Health::Healthy => "HEALTHY",
        Health::Degraded => "DEGRADED",
        Health::Unknown => "UNKNOWN (no RAID array found)",
    };
    let _ = writeln!(out, "SHR-RS status (detail): {health}");

    let _ = writeln!(out);
    let _ = writeln!(out, "Disks ({}):", r.disks.len());
    let _ = writeln!(
        out,
        "  {:<10}{:>10}  {:<20}{:<16}{:<6}{:<6}{:>9}{:>6}{:>8}{:>8}",
        "NODE", "SIZE", "MODEL", "SERIAL", "SMART", "TEMP", "POWERON", "PEND", "REALLOC", "UNCORR"
    );
    for d in &r.disks {
        let size = d.size.map(human_bytes).unwrap_or_else(|| "?".into());
        let smart = match d.smart.state {
            SmartState::Ok => "ok",
            SmartState::Warning => "WARN",
            SmartState::Unknown => "?",
        };
        let temp = d
            .smart
            .temperature_c
            .map(|t| format!("{t}C"))
            .unwrap_or_else(|| "-".into());
        let poweron = d
            .smart
            .power_on_hours
            .map(|h| format!("{h}h"))
            .unwrap_or_else(|| "?".into());
        let _ = writeln!(
            out,
            "  {:<10}{:>10}  {:<20}{:<16}{:<6}{:<6}{:>9}{:>6}{:>8}{:>8}",
            d.name,
            size,
            bounded(opt(&d.model), 20),
            bounded(opt(&d.serial), 16),
            smart,
            temp,
            poweron,
            opt_count(d.smart.pending_sectors),
            opt_count(d.smart.reallocated_sectors),
            opt_count(d.smart.uncorrectable_sectors),
        );
    }

    let _ = writeln!(out);
    if r.groups.is_empty() {
        let _ = writeln!(out, "Groups: (none)");
    } else {
        let _ = writeln!(out, "Groups ({}):", r.groups.len());
        for g in &r.groups {
            // Compression sits next to mount_point -- the neighbouring
            // fact about the same filesystem, and the one `fs recompress`
            // (the operational trigger for this field) changes. `compression`
            // is a required String (never Option, see GroupStatus's doc
            // comment), so an empty value would mean something went wrong
            // upstream -- shown as "-" rather than fabricated.
            let compression = if g.compression.is_empty() {
                "-"
            } else {
                g.compression.as_str()
            };
            let _ = writeln!(
                out,
                // No layout version here either -- see render_group_row.
                "  {} (mode={}, disks={}, compression={}) -> {}",
                g.name,
                g.mode,
                g.disks.len(),
                compression,
                g.mount_point
            );
            for b in &g.bands {
                let _ = writeln!(out, "    {}", render_band_detail_row(b));
            }
        }
    }

    let unassigned: Vec<&DiskStatus> = r.disks.iter().filter(|d| d.arrays.is_empty()).collect();
    let _ = writeln!(out);
    if unassigned.is_empty() {
        let _ = writeln!(out, "Unassigned capacity: none");
    } else {
        let total: u64 = unassigned.iter().filter_map(|d| d.size).sum();
        let _ = writeln!(
            out,
            "Unassigned disks ({}): {} not backing any array",
            unassigned.len(),
            human_bytes(total)
        );
        for d in &unassigned {
            let size = d.size.map(human_bytes).unwrap_or_else(|| "?".into());
            let _ = writeln!(out, "    {:<10}{}", d.name, size);
        }
    }

    out
}

/// `members` annotated with mdadm's own bracket vocabulary for any
/// name `member_states` flags faulty/spare/write-mostly/replacement -- falls
/// back to the bare name when no state is known for it. Faulty takes
/// precedence over spare (a device is never reported as both); write-mostly
/// and replacement are independent bits mdadm can set alongside either, so
/// they're appended rather than folded into that same either/or choice.
///
/// Takes plain slices (not `&GroupBandStatus`) so `shr-tui`'s `ui.rs` --
/// which annotates `ArrayStatus.members`/`.member_states`, a same-shaped but
/// distinct type -- can call this directly instead of re-implementing
/// the same `(F)`/`(S)`/`(W)`/`(R)` mapping a second time.
pub fn annotated_members(members: &[String], member_states: &[MemberStatus]) -> Vec<String> {
    members
        .iter()
        .map(|name| {
            let Some(m) = member_states.iter().find(|m| &m.name == name) else {
                return name.clone();
            };
            let mut suffix = String::new();
            if m.faulty {
                suffix.push_str("(F)");
            } else if m.spare {
                suffix.push_str("(S)");
            }
            if m.write_mostly {
                suffix.push_str("(W)");
            }
            // `(R)` is mdadm's live marker that this member is the copy
            // target of an in-progress `--replace` -- the single most
            // relevant state during the riskiest operation this tool
            // performs (`disk replace`).
            if m.replacement {
                suffix.push_str("(R)");
            }
            if suffix.is_empty() {
                name.clone()
            } else {
                format!("{name}{suffix}")
            }
        })
        .collect()
}

/// One band's line in [`render_status_detail`]: level, mdadm device, usable
/// capacity, live members, live sync progress, and scrub history/in-progress
/// flag. `b.members.is_empty()` is read as "no live mdadm array with this
/// name right now" (see `GroupBandStatus::members`'s doc comment) rather than
/// silently reporting a made-up "idle".
fn render_band_detail_row(b: &GroupBandStatus) -> String {
    let resize = if b.resize_pending {
        " [expansion unfinished]"
    } else {
        ""
    };
    let live = if b.members.is_empty() {
        "no live mdadm array".to_string()
    } else if let Some(s) = &b.sync {
        let eta = s
            .finish_min
            .map(|m| format!(", finish ~{m:.1} min"))
            .unwrap_or_default();
        match s.percent {
            Some(p) => format!("{} {:.1}%{eta}", s.action, p),
            None => format!("{} pending{eta}", s.action),
        }
    } else {
        "idle".to_string()
    };
    let scrub_prefix = if b.scrub_in_progress {
        "scrub running; "
    } else {
        ""
    };
    let scrub = match &b.last_scrub {
        Some(s) => {
            let outcome = match s.outcome {
                ScrubOutcome::Completed => "completed",
                ScrubOutcome::Cancelled => "cancelled",
                ScrubOutcome::Failed => "FAILED",
            };
            format!(
                "{scrub_prefix}last scrub {outcome} at {} ({} errors)",
                s.finished_at, s.error_count
            )
        }
        None => format!("{scrub_prefix}last scrub: never"),
    };
    let members = if b.members.is_empty() {
        "-".to_string()
    } else {
        join_bounded(
            &annotated_members(&b.members, &b.member_states),
            DIAGRAM_MEMBERS_WIDTH,
        )
    };
    // A member `annotated_members` marked `(F)` above can be a genuine
    // new fault OR the harmless, self-clearing tail of a `disk replace`
    // still finishing (defer its `--remove`) -- the two look
    // identical in `member_states` alone. Naming which member and saying so
    // explicitly is the whole point; a bare "pending" flag with no name, or
    // no explanation at all, would leave the operator exactly as unable to
    // tell the two apart as before this field existed.
    let pending_removal = match &b.pending_member_removal {
        Some(name) => {
            format!("  pending-removal: {name} (its replace copy is still finishing -- not a new fault)")
        }
        None => String::new(),
    };
    format!(
        "band{:<3}{:<7}{:<10}{:>12}{}  members: {}  [{live}]  {scrub}{pending_removal}",
        b.index,
        b.level,
        b.md_name,
        human_bytes(b.usable_bytes),
        resize,
        members,
    )
}

/// Terminal geometry a `status --watch` frame must fit -- measured by the
/// caller's real terminal (the same kind of query `is_interactive_terminal`
/// does for the confirm gate; out of scope for this module, which stays pure
/// like every other renderer here). `render_status_watch_frame` guarantees
/// its output is exactly `max_height` lines, each exactly `width` columns,
/// regardless of how much content `StatusReport` carries -- see that
/// function's doc comment for why a predictable frame size matters for a
/// redraw loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchFrameMeta {
    pub width: usize,
    pub max_height: usize,
}

/// Bar width (in columns) for the inline progress bars
/// [`render_status_watch_frame`] draws next to each array/band.
const WATCH_BAR_WIDTH: usize = 20;

/// Render one frame of `status --watch`: a pure function of the current
/// `StatusReport` plus the terminal geometry it must fit. The CLI owns the
/// redraw loop (sleep, re-fetch status, re-call this, overwrite the
/// terminal) -- this function only draws a single frame, and draws it the
/// same way every time for the same inputs, which is what makes the loop
/// possible to implement without flicker or scroll:
///
/// - **Fixed height**: always exactly `meta.max_height` lines. Content past
///   that budget collapses into one "+N more" summary line instead of
///   overflowing; content short of it is padded with blank lines. A redraw
///   loop that always emits the same line count can move the cursor back up
///   and overwrite in place instead of re-clearing the screen (which is what
///   causes visible flicker/scroll-jump in a real terminal).
/// - **Fixed width**: every line is padded or truncated to exactly
///   `meta.width` columns, so a shorter new frame can't leave stale
///   characters from a longer previous frame trailing after it.
/// - **Idempotent**: the same `StatusReport` + `WatchFrameMeta` always
///   produces the exact same `String` -- no wall-clock, no randomness, byte
///   for byte -- so a redraw loop can diff frames and skip repainting when
///   nothing changed (see `render_status_detail`'s doc comment for why this
///   function reports sync progress as "finish ~N min" rather than an
///   absolute wall-clock ETA: it takes no time input).
pub fn render_status_watch_frame(r: &StatusReport, meta: &WatchFrameMeta) -> String {
    let health = match r.health {
        Health::Healthy => "HEALTHY",
        Health::Degraded => "DEGRADED",
        Health::Unknown => "UNKNOWN (no RAID array found)",
    };
    let mut lines: Vec<String> = vec![format!("SHR-RS status (watch): {health}"), String::new()];

    if r.arrays.is_empty() {
        lines.push("No arrays.".to_string());
    } else {
        lines.push(format!("Arrays ({}):", r.arrays.len()));
        for a in &r.arrays {
            lines.push(format!("  {}", watch_array_row(a)));
        }
    }

    let bands: Vec<&GroupBandStatus> = r.groups.iter().flat_map(|g| g.bands.iter()).collect();
    if !bands.is_empty() {
        lines.push(String::new());
        lines.push(format!("Bands ({}):", bands.len()));
        for b in bands {
            lines.push(format!("  {}", watch_band_row(b)));
        }
    }

    fit_frame(lines, meta).join("\n")
}

fn watch_array_row(a: &ArrayStatus) -> String {
    let percent = a.sync.as_ref().and_then(|s| s.percent);
    let bar = watch_progress_bar(percent, WATCH_BAR_WIDTH);
    let action = watch_action_label(
        a.sync
            .as_ref()
            .map(|s| (s.action.as_str(), s.percent, s.finish_min)),
    );
    format!("{:<8}[{bar}] {action}", a.name)
}

fn watch_band_row(b: &GroupBandStatus) -> String {
    let percent = b.sync.as_ref().and_then(|s| s.percent);
    let bar = watch_progress_bar(percent, WATCH_BAR_WIDTH);
    let action = if b.sync.is_none() && b.members.is_empty() {
        "no live mdadm array".to_string()
    } else {
        watch_action_label(
            b.sync
                .as_ref()
                .map(|s| (s.action.as_str(), s.percent, s.finish_min)),
        )
    };
    let scrub = if b.scrub_in_progress { " [scrubbing]" } else { "" };
    format!("band{:<3}{:<10}[{bar}] {action}{scrub}", b.index, b.md_name)
}

/// `(action, percent, finish_min)` -> `"recover 42.5%, finish ~12.3m"` /
/// `"resync pending"` / `"idle"`. Shared by [`watch_array_row`] and
/// [`watch_band_row`] so both draw progress identically.
fn watch_action_label(sync: Option<(&str, Option<f64>, Option<f64>)>) -> String {
    match sync {
        None => "idle".to_string(),
        Some((action, percent, finish_min)) => {
            let eta = finish_min
                .map(|m| format!(", finish ~{m:.1}m"))
                .unwrap_or_default();
            match percent {
                Some(p) => format!("{action} {p:.1}%{eta}"),
                None => format!("{action} pending{eta}"),
            }
        }
    }
}

/// `width`-column bar: `#` for the completed fraction of `percent`, `.` for
/// the rest. `None` (no live sync data) draws as all spaces -- deliberately
/// blank rather than empty/full, so "unknown" is never mistaken for "0%" or
/// "100%" at a glance.
fn watch_progress_bar(percent: Option<f64>, width: usize) -> String {
    match percent {
        Some(p) => {
            let filled = ((p.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
            let filled = filled.min(width);
            (0..width).map(|i| if i < filled { '#' } else { '.' }).collect()
        }
        None => " ".repeat(width),
    }
}

/// Pad `s` with trailing spaces, or truncate (with a trailing `>`) to fit --
/// always returns exactly `width` columns (or "" when `width == 0`).
fn pad_or_truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count > width {
        let head: String = s.chars().take(width - 1).collect();
        format!("{head}>")
    } else {
        format!("{s}{}", " ".repeat(width - count))
    }
}

/// Fix `lines` to exactly `meta.max_height` entries, each exactly
/// `meta.width` columns -- see [`render_status_watch_frame`]'s doc comment
/// for why both are load-bearing for a flicker-free redraw loop.
fn fit_frame(mut lines: Vec<String>, meta: &WatchFrameMeta) -> Vec<String> {
    if meta.max_height == 0 {
        return Vec::new();
    }
    if lines.len() > meta.max_height {
        let hidden = lines.len() - (meta.max_height - 1);
        lines.truncate(meta.max_height - 1);
        lines.push(format!(
            "... +{hidden} more line(s), resize terminal for full detail"
        ));
    } else {
        while lines.len() < meta.max_height {
            lines.push(String::new());
        }
    }
    lines
        .into_iter()
        .map(|l| pad_or_truncate(&l, meta.width))
        .collect()
}

/// Render a dry-run plan report: band table, capacity accounting, and a bar.
#[allow(clippy::write_literal)] // column headers are padded literals by design
pub fn render_plan(r: &PlanReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Planned layout (mode: {}, DRY RUN)", r.mode);
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "  {:<7}{:<7}{:>12}{:>8}{:>12}",
        "BAND", "LEVEL", "SLICE", "MEMBERS", "USABLE"
    );
    for b in &r.bands {
        let _ = writeln!(
            out,
            "  band{:<3}{:<7}{:>12}{:>8}{:>12}",
            b.index,
            b.level,
            human_bytes(b.size),
            b.members.len(),
            human_bytes(b.usable)
        );
    }

    let m = &r.metrics;
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  Usable {}   Parity {}   Stranded {}   Raw {}",
        human_bytes(m.total_usable),
        human_bytes(m.redundancy_overhead),
        human_bytes(m.stranded_bytes),
        human_bytes(m.total_raw),
    );
    let _ = writeln!(
        out,
        "  [{}]  ({:.0}% wasted)",
        capacity_bar(
            m.total_usable,
            m.redundancy_overhead,
            m.stranded_bytes,
            m.total_raw,
            40
        ),
        m.waste_ratio * 100.0,
    );

    if !r.warnings.is_empty() {
        let _ = writeln!(out);
        for w in &r.warnings {
            let _ = writeln!(out, "  ! {w}");
        }
    }

    out
}

/// A `width`-wide bar: `#` usable, `+` parity, `.` stranded, ` ` free.
///
/// Uses cumulative integer boundaries so segments are monotonic and always sum
/// to exactly `width` (no per-segment rounding over/underflow).
fn capacity_bar(usable: u64, parity: u64, stranded: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return " ".repeat(width);
    }
    let pos = |part: u64| -> usize { ((part as u128 * width as u128) / total as u128) as usize };
    // saturating sums so an externally-constructed inconsistent report can't
    // overflow the cumulative boundaries.
    let u = pos(usable);
    let up = pos(usable.saturating_add(parity));
    let ups = pos(usable.saturating_add(parity).saturating_add(stranded));
    (0..width)
        .map(|i| {
            if i < u {
                '#'
            } else if i < up {
                '+'
            } else if i < ups {
                '.'
            } else {
                ' '
            }
        })
        .collect()
}

/// Bar width (in columns, inside the `[...]` brackets) for
/// [`render_layout_diagram`] rows. Matches `capacity_bar`'s 40-column
/// convention in `render_plan`.
const DIAGRAM_BAR_WIDTH: usize = 40;
/// Fixed disk-label column width for [`render_layout_diagram`] -- long by-id
/// names are truncated to this so the diagram's line width never depends on
/// input data (see `short_label`).
const DIAGRAM_LABEL_WIDTH: usize = 14;
/// Column budget for a band's member list in [`render_layout_diagram`]'s
/// per-band legend, past which it's truncated with a "+N more" suffix.
const DIAGRAM_MEMBERS_WIDTH: usize = 48;

/// Render the planner's output as a per-disk ASCII layout diagram: one row
/// per physical disk, showing how it is sliced into bands at the byte
/// offsets the planner actually chose -- the picture behind SHR's "different
/// disk sizes carved into offset bands, each band its own mdadm array"
/// design, which is much harder to follow from the `render_plan` table alone.
///
/// `disks` must be the same slice that produced `out` (typically via
/// `shr_core::plan_initial`) -- this function only visualizes an existing
/// plan, it never plans or touches any device. Pure, like every other
/// renderer in this module.
///
/// All bars share one scale (bytes-per-column derived from the *largest*
/// disk in `disks`), so a band's offset range lines up in the same columns
/// across every disk it spans -- that alignment is the whole point of the
/// picture. Per disk, a row is drawn left to right as:
/// - `#` -- data-bearing bytes of a band this disk is a member of;
/// - `+` -- that same band's parity/mirror bytes on this disk;
/// - `.` -- stranded tail with no redundancy (`PlannerOutput::unusable_per_disk`);
/// - `~` -- reserved/alignment slack the planner never assigned to any band
///   or stranded-tail entry (reserved head/tail, sub-alignment remainder).
///   This is derived by subtraction (`disk size - everything accounted for
///   above`), not read from a field, since `PlannerOutput` doesn't carry
///   `PlannerInput`'s reserve sizes -- see the doc comment on `disk_bar`.
///
/// Below the rows, a short per-band legend states each band's RAID level and
/// members explicitly (the bar encodes membership structurally -- which rows
/// have non-blank segments at a band's columns -- but doesn't spell out the
/// level or name the disks).
pub fn render_layout_diagram(disks: &[Disk], out: &PlannerOutput) -> String {
    let mut result = String::new();
    let max_size = disks.iter().map(|d| d.size_bytes).max().unwrap_or(0);

    let _ = writeln!(result, "Layout diagram (DRY RUN)");
    let _ = writeln!(result);
    let _ = writeln!(
        result,
        "  # data   + parity   . stranded (no redundancy)   ~ reserved/alignment"
    );
    let _ = writeln!(result);

    for d in disks {
        let bar = disk_bar(d, out, max_size, DIAGRAM_BAR_WIDTH);
        let _ = writeln!(
            result,
            "  {:<label_width$}{:>10}  [{}]",
            short_label(&d.id, DIAGRAM_LABEL_WIDTH),
            human_bytes(d.size_bytes),
            bar,
            label_width = DIAGRAM_LABEL_WIDTH
        );
    }

    if !out.bands.is_empty() {
        let _ = writeln!(result);
        for band in &out.bands {
            let level = format!("{:?}", band.level()).to_lowercase();
            let members: Vec<String> = band
                .members()
                .iter()
                .map(|m| short_label(m, DIAGRAM_LABEL_WIDTH))
                .collect();
            let _ = writeln!(
                result,
                "  band{:<3}{:<7}members: {}",
                band.band_index(),
                level,
                join_bounded(&members, DIAGRAM_MEMBERS_WIDTH)
            );
        }
    }

    result
}

/// One disk's row for [`render_layout_diagram`]: a `width`-column bar over a
/// shared `max_size`-byte scale (see that function's doc comment for the
/// alignment rationale).
///
/// The `~` reserved/alignment tail is computed by subtraction rather than
/// read from `PlannerOutput`: `plan_initial` (shr-core's `planner.rs`) plans
/// over each disk's *usable* length -- raw size minus `reserved_head` and
/// `reserved_tail`, rounded down to `band_alignment` -- but `PlannerOutput`
/// only carries the resulting bands and `unusable_per_disk` (the tracked
/// no-redundancy stranded tail within that usable length), not the reserve
/// sizes themselves. So the bytes between "last accounted-for offset" and
/// `disk.size_bytes` are real and exact (`size_bytes` minus two sums the
/// planner did report), but this function cannot say how much of that
/// remainder is reserved head vs. tail vs. alignment loss -- only that the
/// planner never assigned it to a band or a tracked stranded entry. `~`
/// deliberately reads as one undifferentiated "not addressed by the plan"
/// category rather than inventing a head/tail split the input data doesn't
/// support.
fn disk_bar(disk: &Disk, out: &PlannerOutput, max_size: u64, width: usize) -> String {
    if max_size == 0 {
        return " ".repeat(width);
    }
    let pos = |bytes: u64| -> usize { ((bytes as u128 * width as u128) / max_size as u128) as usize };

    let mut cols = vec![' '; width];
    let mut cursor = 0u64;

    // `out.bands` is built by `plan_initial` in ascending offset order, and a
    // disk's participating bands are always a contiguous prefix of that
    // sequence starting at offset 0 (a disk drops out of every window past
    // the one where its own usable length ends) -- so filtering in place and
    // tracking `cursor` needs no separate sort or gap handling.
    for band in out.bands.iter().filter(|b| b.contains(&disk.id)) {
        let start = band.offset();
        let end = band.end();
        let n = band.members().len();
        let data_n = band.level().data_members(n);
        let data_end = if n == 0 {
            start
        } else {
            start + (band.size() as u128 * data_n as u128 / n as u128) as u64
        };
        fill(&mut cols, pos(start), pos(data_end), '#');
        fill(&mut cols, pos(data_end), pos(end), '+');
        cursor = end;
    }

    if let Some(&stranded) = out.unusable_per_disk.get(&disk.id) {
        let end = cursor.saturating_add(stranded);
        fill(&mut cols, pos(cursor), pos(end), '.');
        cursor = end;
    }

    fill(&mut cols, pos(cursor), pos(disk.size_bytes), '~');

    cols.into_iter().collect()
}

fn fill(cols: &mut [char], start: usize, end: usize, ch: char) {
    let end = end.min(cols.len());
    for c in cols.iter_mut().take(end).skip(start) {
        *c = ch;
    }
}

/// `id.short()`, truncated (with a trailing `>`) to fit `width` columns --
/// the fixed budget that keeps [`render_layout_diagram`]'s line width
/// independent of how long a by-id disk name is.
fn short_label(id: &DiskId, width: usize) -> String {
    let s = id.short();
    if s.chars().count() <= width || width == 0 {
        return s.to_string();
    }
    let head: String = s.chars().take(width - 1).collect();
    format!("{head}>")
}

/// Join `items` with `", "`, truncating to a "+N more" suffix once the
/// result would exceed `max_width` columns -- keeps a band's member-list
/// legend line bounded regardless of how many disks are in the band.
fn join_bounded(items: &[String], max_width: usize) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let sep = if i == 0 { "" } else { ", " };
        let remaining = items.len() - i;
        let more_suffix_len = format!(" +{remaining} more").len();
        if !out.is_empty()
            && out.chars().count() + sep.len() + item.chars().count() + more_suffix_len > max_width
        {
            let _ = write!(out, " +{remaining} more");
            return out;
        }
        out.push_str(sep);
        out.push_str(item);
    }
    out
}

fn opt_bytes(v: Option<u64>) -> String {
    v.map(human_bytes).unwrap_or_else(|| "?".into())
}

/// Render an `fs df` report: one row per group juxtaposing the always-known
/// logical usable capacity (from `state.toml` bands) against whatever live
/// Btrfs chunk-allocation figures the caller supplied to `build_fs_df` --
/// `?` for anything unknown. See `FsUsageInput`'s doc comment for why this
/// never invents a number: a group's row falls back to all `?` except
/// `USABLE` whenever the caller has no live Btrfs usage figures for it.
///
/// The header note exists because a plain `df`'s "available" figure is a
/// well-known Btrfs footgun: Btrfs allocates data and metadata into chunks
/// separately from what `statvfs` reports, so `df`'s number can read far
/// more optimistic (before a chunk is claimed) or pessimistic (after
/// fragmentation) than the unallocated raw space actually backing it.
pub fn render_fs_df(r: &FsDfReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Filesystem capacity");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  NOTE: Btrfs allocates data/metadata chunks separately from a plain"
    );
    let _ = writeln!(
        out,
        "  df's view -- DF-AVAIL below can read more optimistic or pessimistic"
    );
    let _ = writeln!(
        out,
        "  than reality. UNALLOC (raw space not yet claimed by any chunk) is"
    );
    let _ = writeln!(out, "  the more trustworthy free-space signal when known.");
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "  {:<10}{:<18}{:>10}{:>22}{:>22}{:>10}{:>10}",
        "GROUP", "MOUNT", "USABLE", "DATA (used/total)", "META (used/total)", "UNALLOC", "DF-AVAIL"
    );
    for g in &r.groups {
        let _ = writeln!(out, "  {}", render_df_row(g));
    }
    out
}

fn render_df_row(g: &GroupDfStatus) -> String {
    let data = format!(
        "{}/{}",
        opt_bytes(g.data_used_bytes),
        opt_bytes(g.data_total_bytes)
    );
    let meta = format!(
        "{}/{}",
        opt_bytes(g.metadata_used_bytes),
        opt_bytes(g.metadata_total_bytes)
    );
    format!(
        "{:<10}{:<18}{:>10}{:>22}{:>22}{:>10}{:>10}",
        bounded(&g.name, 10),
        bounded(&g.mount_point, 18),
        human_bytes(g.usable_bytes),
        data,
        meta,
        opt_bytes(g.unallocated_bytes),
        opt_bytes(g.statvfs_avail_bytes),
    )
}

/// A thin, focused disk inventory --
/// every field `status`'s disk table omits because that table optimizes for
/// "what's the array's health", not "what disks does this host have and how
/// do I refer to each one". Built from the same `StatusReport.disks`
/// `status`/`disk smart` already read (no new inspector logic), so this is
/// deliberately NOT a duplicate of `render_status`'s disk section.
pub fn render_disk_list(r: &StatusReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Disks ({}):  {:<10}{:<26}{:>10}  {:<24}{:<16}{:<6}{:<8}ARRAYS",
        r.disks.len(),
        "NODE",
        "ID",
        "SIZE",
        "MODEL",
        "SERIAL",
        "ROTA",
        "SYSTEM",
    );
    for d in &r.disks {
        let size = d.size.map(human_bytes).unwrap_or_else(|| "?".into());
        let rota = match d.rotational {
            Some(true) => "HDD",
            Some(false) => "SSD",
            None => "?",
        };
        let system = if d.system_disk { "yes" } else { "-" };
        let _ = writeln!(
            out,
            "             {:<10}{:<26}{:>10}  {:<24}{:<16}{:<6}{:<8}{}",
            d.name,
            opt(&d.id),
            size,
            opt(&d.model),
            opt(&d.serial),
            rota,
            system,
            d.arrays.join(",")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        capacity_bar, human_bytes, render_disk_list, render_fs_df, render_layout_diagram, render_plan,
        render_status, render_status_detail, render_status_watch_frame, WatchFrameMeta,
    };
    use crate::report::{
        ArrayStatus, BandReport, DiskStatus, FsDfReport, GroupBandStatus, GroupDfStatus, GroupStatus, Health,
        MemberStatus, MetricsReport, PlanReport, ScrubOutcome, ScrubSummary, SmartState, SmartSummary,
        StatusReport, SyncSummary,
    };
    use std::collections::BTreeMap;

    /// A `StatusReport` exercising every optional/multi-row branch of
    /// `render_status` (disks, a degraded array with sync progress, and a
    /// group with a resize-pending band): this is the real value case
    /// for `insta` this project didn't have before. The column-padded
    /// ASCII table this function produces is exactly the kind of output a
    /// hand-written `assert_eq!(rendered, "...")` is painful and fragile to
    /// review (any width/order change silently reformats every line); a
    /// snapshot makes an unintended layout change show up as an obvious
    /// diff instead.
    fn status_fixture() -> StatusReport {
        StatusReport {
            schema_version: 2,
            health: Health::Degraded,
            disks: vec![DiskStatus {
                name: "sdb".to_string(),
                id: Some("ata-DISK1".to_string()),
                size: Some(4_000_000_000_000),
                model: Some("WD Red Plus".to_string()),
                serial: Some("WD-SERIAL-B".to_string()),
                rotational: Some(true),
                smart: SmartSummary {
                    state: SmartState::Warning,
                    temperature_c: Some(38),
                    power_on_hours: Some(1000),
                    pending_sectors: Some(1),
                    reallocated_sectors: Some(0),
                    uncorrectable_sectors: Some(0),
                    nvme_critical_warning: None,
                },
                arrays: vec!["md0".to_string()],
                system_disk: false,
                system_mounts: vec![],
            }],
            arrays: vec![ArrayStatus {
                name: "md0".to_string(),
                level: Some("raid5".to_string()),
                state: "clean".to_string(),
                read_only: false,
                degraded: true,
                raid_disks: Some(3),
                active_disks: Some(2),
                members: vec!["sdb1".to_string(), "sdc1".to_string()],
                member_states: vec![healthy_member("sdb1", 0), healthy_member("sdc1", 1)],
                sync: Some(SyncSummary {
                    action: "recover".to_string(),
                    percent: Some(42.5),
                    finish_min: Some(12.3),
                }),
            }],
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
                resize_pending: true,
                disks: vec!["ata-DISK1".to_string()],
                bands: vec![GroupBandStatus {
                    index: 0,
                    level: "raid5".to_string(),
                    md_name: "md0".to_string(),
                    md_uuid: Some("12345678:9abcdef0:12345678:9abcdef0".to_string()),
                    usable_bytes: 8_000_000_000_000,
                    resize_pending: true,
                    members: vec!["sdb1".to_string(), "sdc1".to_string()],
                    member_states: vec![healthy_member("sdb1", 0), healthy_member("sdc1", 1)],
                    sync: None,
                    last_scrub: None,
                    scrub_in_progress: false,
                    pending_member_removal: None,
                }],
            }],
            // Not under test here -- rendering doesn't read this field.
            state_path: None,
        }
    }

    /// A healthy (non-faulty, non-spare) [`MemberStatus`] -- the common case
    /// in every fixture that doesn't specifically exercise the faulty/
    /// spare rendering.
    fn healthy_member(name: &str, role: u32) -> MemberStatus {
        MemberStatus {
            name: name.to_string(),
            role: Some(role),
            faulty: false,
            spare: false,
            write_mostly: false,
            replacement: false,
        }
    }

    #[test]
    fn render_status_snapshot() {
        insta::assert_snapshot!(render_status(&status_fixture()));
    }

    /// `disk list`'s dedicated rendering -- must show fields `status`'s
    /// disk table never did (by-id, serial, rotational, system-disk flag),
    /// proving this is a distinct, focused view rather than a re-skin.
    #[test]
    fn render_disk_list_snapshot() {
        insta::assert_snapshot!(render_disk_list(&status_fixture()));
    }

    fn plan_fixture() -> PlanReport {
        let mut utilization = BTreeMap::new();
        utilization.insert("ata-DISK1".to_string(), 1.0);
        utilization.insert("ata-DISK2".to_string(), 1.0);
        let mut unusable_per_disk = BTreeMap::new();
        unusable_per_disk.insert("ata-DISK3".to_string(), 500_000_000_000);
        PlanReport {
            schema_version: 2,
            mode: "shr".to_string(),
            bands: vec![BandReport {
                index: 0,
                level: "raid5".to_string(),
                size: 4_000_000_000_000,
                members: vec![
                    "ata-DISK1".to_string(),
                    "ata-DISK2".to_string(),
                    "ata-DISK3".to_string(),
                ],
                usable: 8_000_000_000_000,
                raw: 12_000_000_000_000,
            }],
            metrics: MetricsReport {
                total_usable: 8_000_000_000_000,
                total_raw: 12_000_000_000_000,
                redundancy_overhead: 4_000_000_000_000,
                stranded_bytes: 500_000_000_000,
                waste_ratio: 0.375,
                utilization,
            },
            unusable_per_disk,
            warnings: vec!["ata-DISK3: 500000000000 B stranded (no redundancy)".to_string()],
        }
    }

    #[test]
    fn render_plan_snapshot() {
        insta::assert_snapshot!(render_plan(&plan_fixture()));
    }

    #[test]
    fn capacity_bar_is_always_exactly_width() {
        for (u, p, s, t) in [(8, 4, 2, 14), (1, 1, 1, 3), (0, 0, 0, 0), (100, 0, 0, 100)] {
            assert_eq!(capacity_bar(u, p, s, t, 40).chars().count(), 40);
        }
    }

    #[test]
    fn capacity_bar_segments_are_monotonic() {
        let bar = capacity_bar(8, 4, 2, 14, 40);
        // No '#' after a '+', no '+' after a '.', etc.
        let order = |c: char| match c {
            '#' => 0,
            '+' => 1,
            '.' => 2,
            _ => 3,
        };
        let ranks: Vec<u8> = bar.chars().map(order).collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "bar not monotonic: {bar}");
    }

    #[test]
    fn human_bytes_units_and_extremes() {
        assert_eq!(human_bytes(4_000_000_000_000), "4.0 TB");
        assert_eq!(human_bytes(512_000_000_000), "512.0 GB");
        assert_eq!(human_bytes(0), "0 B");
        let _ = human_bytes(u64::MAX); // must not panic
    }

    // --- render_layout_diagram ---------------------------------------------

    use shr_core::{plan_initial, Disk, PlannerInput, PlannerOutput, RedundancyMode};

    const TB: u64 = 1_000_000_000_000;

    /// The heterogeneous case that's the whole reason this diagram exists:
    /// two 8 TB and two 4 TB disks under SHR. `PlannerInput::exact` (no
    /// reserves/alignment) keeps the byte math readable in the snapshot --
    /// `render_layout_diagram_marks_reserved_slack_from_real_reserves` below
    /// separately exercises the `~` reserved/alignment path that the default
    /// reserves would otherwise mix into this fixture.
    ///
    /// Expected plan: band0 is a 4-member raid5 across all four disks
    /// (offset 0..4TB, since Shr picks raid5 at n>=3); band1 is a 2-member
    /// raid1 across just the two 8 TB disks (offset 4TB..8TB, since Shr picks
    /// raid1 at n==2) -- i.e. two different RAID levels in the same layout,
    /// which is exactly the case a uniform-capacity fixture can't cover.
    fn heterogeneous_layout() -> (Vec<Disk>, PlannerOutput) {
        let disks = vec![
            Disk::new("ata-WDC_BIG1", 8 * TB),
            Disk::new("ata-WDC_BIG2", 8 * TB),
            Disk::new("ata-WDC_SMALL1", 4 * TB),
            Disk::new("ata-WDC_SMALL2", 4 * TB),
        ];
        let out = plan_initial(&PlannerInput::exact(disks.clone(), RedundancyMode::Shr)).unwrap();
        (disks, out)
    }

    #[test]
    fn render_layout_diagram_heterogeneous_snapshot() {
        let (disks, out) = heterogeneous_layout();
        assert_eq!(out.bands.len(), 2, "expected two tiers, got {:#?}", out.bands);
        insta::assert_snapshot!(render_layout_diagram(&disks, &out));
    }

    #[test]
    fn render_layout_diagram_marks_reserved_slack_from_real_reserves() {
        // Two equal 1000-byte disks, no alignment, but a 100-byte reserved
        // head -- `plan_initial` plans over the remaining 900 bytes only, so
        // the diagram's `~` segment must cover exactly the last 100 bytes of
        // each disk's bar (a `PlannerOutput` never carries the reserve sizes
        // themselves -- see `disk_bar`'s doc comment -- so this is the only
        // way to check that subtraction lands on the right bytes).
        let disks = vec![Disk::new("d0", 1000), Disk::new("d1", 1000)];
        let input = PlannerInput {
            disks: disks.clone(),
            mode: RedundancyMode::Shr,
            band_alignment: 1,
            reserved_head: 100,
            reserved_tail: 0,
        };
        let out = plan_initial(&input).unwrap();
        assert_eq!(out.bands[0].size(), 900);
        assert!(out.unusable_per_disk.is_empty());

        let text = render_layout_diagram(&disks, &out);
        let bar_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("d0"))
            .expect("d0 row present");
        let bar = bar_line.split('[').nth(1).unwrap().trim_end_matches(']');
        // 900/1000 of a 40-column bar is column 36 -- the tail from there on
        // (the reserved 100 bytes) must be `~`, and nothing before it.
        assert!(!bar[..36].contains('~'), "bar: {bar}");
        assert!(bar[36..].chars().all(|c| c == '~'), "bar: {bar}");
    }

    #[test]
    fn render_layout_diagram_line_width_is_bounded() {
        // Many disks, and one absurdly long by-id name -- neither should
        // widen a single line of output.
        let mut disks: Vec<Disk> = (0..8)
            .map(|i| Disk::new(format!("d{i}"), (i as u64 + 1) * TB))
            .collect();
        disks.push(Disk::new(
            "ata-WDC_WD80EFZZ-68BTXN0_A_VERY_LONG_SERIAL_NUMBER_THAT_KEEPS_GOING_ON_AND_ON",
            9 * TB,
        ));
        let out = plan_initial(&PlannerInput::exact(disks.clone(), RedundancyMode::Shr)).unwrap();

        let text = render_layout_diagram(&disks, &out);
        const MAX_LINE_WIDTH: usize = 100;
        for line in text.lines() {
            assert!(
                line.chars().count() <= MAX_LINE_WIDTH,
                "line exceeds {MAX_LINE_WIDTH} columns ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }

    // --- render_status_detail -----------------------------------------------

    /// Heterogeneous capacities, two bands (one mid-resync with a clean
    /// scrub history, one idle-but-live with a *failed* scrub history and a
    /// scrub *currently running*), a WARN-state disk, and one disk backing
    /// no array at all ("unassigned") -- every branch `render_status_detail`
    /// has to draw at once, not just the uniform/static case.
    fn status_detail_fixture() -> StatusReport {
        StatusReport {
            schema_version: 2,
            health: Health::Degraded,
            disks: vec![
                DiskStatus {
                    name: "sdb".to_string(),
                    id: Some("ata-DISK1".to_string()),
                    size: Some(4_000_000_000_000),
                    model: Some("WD Red Plus".to_string()),
                    serial: Some("WD-SERIAL-B".to_string()),
                    rotational: Some(true),
                    smart: SmartSummary {
                        state: SmartState::Warning,
                        temperature_c: Some(38),
                        power_on_hours: Some(10_000),
                        pending_sectors: Some(1),
                        reallocated_sectors: Some(0),
                        uncorrectable_sectors: Some(0),
                        nvme_critical_warning: None,
                    },
                    arrays: vec!["md0".to_string()],
                    system_disk: false,
                    system_mounts: vec![],
                },
                DiskStatus {
                    name: "sdc".to_string(),
                    id: Some("ata-DISK2".to_string()),
                    size: Some(8_000_000_000_000),
                    model: Some("Seagate Exos X18 A Very Long Model String".to_string()),
                    serial: Some("SEAGATE-SERIAL-C-LONG-ENOUGH-TO-TRUNCATE".to_string()),
                    rotational: Some(true),
                    smart: SmartSummary {
                        state: SmartState::Ok,
                        temperature_c: Some(35),
                        power_on_hours: Some(500),
                        pending_sectors: Some(0),
                        reallocated_sectors: Some(0),
                        uncorrectable_sectors: Some(0),
                        nvme_critical_warning: None,
                    },
                    arrays: vec!["md1".to_string()],
                    system_disk: false,
                    system_mounts: vec![],
                },
                DiskStatus {
                    name: "sdd".to_string(),
                    id: Some("ata-DISK3".to_string()),
                    size: Some(2_000_000_000_000),
                    model: Some("Spare".to_string()),
                    serial: Some("SPARE-D".to_string()),
                    rotational: Some(false),
                    smart: SmartSummary {
                        state: SmartState::Unknown,
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
                },
            ],
            arrays: vec![],
            groups: vec![GroupStatus {
                name: "default".to_string(),
                mode: "shr".to_string(),
                layout_version: 2,
                mount_point: "/mnt/shr_data".to_string(),
                fs_uuid: Some("11111111-2222-3333-4444-555555555555".to_string()),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
                usable_bytes: 12_000_000_000_000,
                resize_pending: true,
                disks: vec!["ata-DISK1".to_string(), "ata-DISK2".to_string()],
                bands: vec![
                    GroupBandStatus {
                        index: 0,
                        level: "raid5".to_string(),
                        md_name: "md0".to_string(),
                        md_uuid: Some("12345678:9abcdef0:12345678:9abcdef0".to_string()),
                        usable_bytes: 8_000_000_000_000,
                        resize_pending: false,
                        members: vec!["sdb1".to_string(), "sdc1".to_string()],
                        member_states: vec![healthy_member("sdb1", 0), healthy_member("sdc1", 1)],
                        sync: Some(SyncSummary {
                            action: "recover".to_string(),
                            percent: Some(42.5),
                            finish_min: Some(12.3),
                        }),
                        last_scrub: Some(ScrubSummary {
                            finished_at: "2026-07-01T00:00:00Z".to_string(),
                            outcome: ScrubOutcome::Completed,
                            error_count: 0,
                        }),
                        scrub_in_progress: false,
                        pending_member_removal: None,
                    },
                    // A band with a FAULTY member -- the exact repro
                    // this fixture must render distinguishably (`sdd1(F)`),
                    // not just as another plain member name. Also: that
                    // same member is state.toml's `pending_member_removal`
                    // (its by-partuuid path already resolved to `sdd1` by
                    // `ops::band_status`, as this report struct is built
                    // after resolution) -- the benign-replace-still-finishing
                    // case, not a fresh second fault.
                    GroupBandStatus {
                        index: 1,
                        level: "raid1".to_string(),
                        md_name: "md1".to_string(),
                        md_uuid: None,
                        usable_bytes: 4_000_000_000_000,
                        resize_pending: true,
                        members: vec!["sdd1".to_string()],
                        member_states: vec![MemberStatus {
                            name: "sdd1".to_string(),
                            role: Some(1),
                            faulty: true,
                            spare: false,
                            write_mostly: false,
                            replacement: false,
                        }],
                        sync: None,
                        last_scrub: Some(ScrubSummary {
                            finished_at: "2026-06-15T12:00:00Z".to_string(),
                            outcome: ScrubOutcome::Failed,
                            error_count: 3,
                        }),
                        scrub_in_progress: true,
                        pending_member_removal: Some("sdd1".to_string()),
                    },
                ],
            }],
            // Not under test here -- rendering doesn't read this field.
            state_path: None,
        }
    }

    #[test]
    fn render_status_detail_snapshot() {
        insta::assert_snapshot!(render_status_detail(&status_detail_fixture()));
    }

    #[test]
    fn render_status_detail_lists_unassigned_disks() {
        let text = render_status_detail(&status_detail_fixture());
        assert!(text.contains("Unassigned disks (1)"), "{text}");
        assert!(text.contains("sdd"), "{text}");
    }

    #[test]
    fn render_status_detail_shows_scrub_history_and_in_progress() {
        let text = render_status_detail(&status_detail_fixture());
        assert!(text.contains("last scrub completed"), "{text}");
        assert!(text.contains("last scrub FAILED"), "{text}");
        assert!(text.contains("scrub running"), "{text}");
    }

    /// `GroupStatus.compression` reached `--json` but never a
    /// human-readable view -- an operator deciding whether to run `fs
    /// recompress`, or checking whether it took effect, had to know to pass
    /// `--json` and parse it themselves.
    #[test]
    fn render_status_detail_shows_compression() {
        let text = render_status_detail(&status_detail_fixture());
        assert!(text.contains("compression=zstd:3"), "{text}");
    }

    #[test]
    fn render_status_detail_marks_a_faulty_member_distinguishably() {
        // `sdd1` is `member_states`-flagged faulty in the fixture --
        // the rendered member list must mark it, not print it identically
        // to a healthy member name.
        let text = render_status_detail(&status_detail_fixture());
        assert!(text.contains("sdd1(F)"), "{text}");
    }

    /// The fixture's band1 has BOTH `sdd1(F)` and
    /// `pending_member_removal: Some("sdd1")` -- the human `status --detail`
    /// view must name that member explicitly and say why the `(F)` is
    /// benign, not merely print `(F)` the same way it would for a genuinely
    /// new fault.
    #[test]
    fn render_status_detail_names_the_pending_removal_and_explains_it() {
        let text = render_status_detail(&status_detail_fixture());
        assert!(text.contains("pending-removal: sdd1"), "{text}");
        assert!(text.to_lowercase().contains("not a new fault"), "{text}");
    }

    /// The companion negative case: band0 has NO `pending_member_removal` in
    /// the fixture -- its rendered row must carry no "pending-removal" text
    /// at all (never printed as e.g. "pending-removal: none").
    #[test]
    fn render_status_detail_omits_pending_removal_text_when_none_is_pending() {
        let text = render_status_detail(&status_detail_fixture());
        let band0_line = text
            .lines()
            .find(|l| l.contains("md0"))
            .expect("band0 row present");
        assert!(!band0_line.contains("pending-removal"), "{band0_line}");
    }

    /// `write_mostly`/`replacement` are parsed all the way from
    /// `shr-inspect`'s mdstat parser through `MemberStatus`, but the pre-fix
    /// `annotated_members` only ever matched `faulty`/`spare`. `replacement`
    /// in particular is mdadm's live "this member is the copy target of an
    /// in-progress `--replace`" marker -- the single most relevant state
    /// during the riskiest operation this tool performs -- and it was
    /// rendered identically to a plain healthy member.
    #[test]
    fn render_status_detail_marks_write_mostly_and_replacement_members() {
        let mut r = status_detail_fixture();
        r.groups[0].bands[0].members = vec!["sdb1".to_string(), "sdc1".to_string()];
        r.groups[0].bands[0].member_states = vec![
            healthy_member("sdb1", 0),
            MemberStatus {
                name: "sdc1".to_string(),
                role: Some(1),
                faulty: false,
                spare: false,
                write_mostly: true,
                replacement: true,
            },
        ];
        let text = render_status_detail(&r);
        assert!(text.contains("sdc1(W)(R)"), "{text}");
    }

    #[test]
    fn render_status_detail_band_with_no_live_array_says_so_not_idle() {
        // A band whose md_name has no live mdadm array right now (e.g. right
        // after a crash, before `reconcile` re-assembles it) must not be
        // rendered as "idle" -- that would assert a live-but-quiescent array
        // that may not actually exist.
        let mut r = status_detail_fixture();
        r.groups[0].bands[1].members.clear();
        let text = render_status_detail(&r);
        assert!(text.contains("no live mdadm array"), "{text}");
    }

    /// Real reboot observation -- state.toml survived a hard reboot,
    /// loopback devices/mdadm arrays did not, and `--detail`'s guard above
    /// caught it there, but plain `status` (the DEFAULT view an operator
    /// actually sees) printed `band0  raid5   md0     17.2 GB` with no
    /// marker that `md0` doesn't exist. This mirrors
    /// `render_status_detail_band_with_no_live_array_says_so_not_idle` but
    /// against the plain `render_status` path, which had no guard at all.
    #[test]
    fn render_status_band_with_no_live_array_says_so() {
        let mut r = status_fixture();
        r.groups[0].bands[0].members.clear();
        let text = render_status(&r);
        assert!(text.contains("no live mdadm array"), "{text}");
    }

    // --- render_status_watch_frame -------------------------------------------

    fn watch_fixture() -> StatusReport {
        StatusReport {
            schema_version: 2,
            health: Health::Degraded,
            disks: vec![],
            arrays: vec![
                ArrayStatus {
                    name: "md0".to_string(),
                    level: Some("raid5".to_string()),
                    state: "clean".to_string(),
                    read_only: false,
                    degraded: true,
                    raid_disks: Some(3),
                    active_disks: Some(2),
                    members: vec!["sdb1".to_string(), "sdc1".to_string()],
                    member_states: vec![healthy_member("sdb1", 0), healthy_member("sdc1", 1)],
                    sync: Some(SyncSummary {
                        action: "recover".to_string(),
                        percent: Some(42.5),
                        finish_min: Some(12.3),
                    }),
                },
                ArrayStatus {
                    name: "md1".to_string(),
                    level: Some("raid1".to_string()),
                    state: "clean".to_string(),
                    read_only: false,
                    degraded: false,
                    raid_disks: Some(2),
                    active_disks: Some(2),
                    members: vec!["sdd1".to_string(), "sde1".to_string()],
                    member_states: vec![healthy_member("sdd1", 0), healthy_member("sde1", 1)],
                    sync: None,
                },
            ],
            groups: vec![],
            // Not under test here -- rendering doesn't read this field.
            state_path: None,
        }
    }

    #[test]
    fn render_status_watch_frame_snapshot() {
        let meta = WatchFrameMeta {
            width: 72,
            max_height: 12,
        };
        insta::assert_snapshot!(render_status_watch_frame(&watch_fixture(), &meta));
    }

    #[test]
    fn watch_frame_height_is_always_exactly_max_height() {
        // One meta that forces truncation (fewer rows than content needs),
        // one that forces padding (more rows than content needs) -- both
        // must still land on exactly `max_height` lines.
        for meta in [
            WatchFrameMeta {
                width: 60,
                max_height: 3,
            },
            WatchFrameMeta {
                width: 60,
                max_height: 40,
            },
        ] {
            let text = render_status_watch_frame(&watch_fixture(), &meta);
            assert_eq!(text.lines().count(), meta.max_height, "meta={meta:?}: {text}");
        }
    }

    #[test]
    fn watch_frame_width_is_always_exactly_meta_width() {
        let meta = WatchFrameMeta {
            width: 50,
            max_height: 10,
        };
        let text = render_status_watch_frame(&watch_fixture(), &meta);
        for line in text.lines() {
            assert_eq!(line.chars().count(), meta.width, "line: {line:?}");
        }
    }

    #[test]
    fn watch_frame_is_idempotent_for_the_same_input() {
        // No wall-clock, no randomness -- the same report+meta must produce
        // byte-for-byte the same frame, which is what lets a redraw loop
        // diff frames and skip repainting when nothing changed.
        let meta = WatchFrameMeta {
            width: 64,
            max_height: 16,
        };
        let a = render_status_watch_frame(&watch_fixture(), &meta);
        let b = render_status_watch_frame(&watch_fixture(), &meta);
        assert_eq!(a, b);
    }

    #[test]
    fn watch_frame_reflects_content_changes() {
        // Sanity check on the idempotence test above: this isn't a constant
        // frame regardless of input -- a real content change must change it.
        let meta = WatchFrameMeta {
            width: 64,
            max_height: 16,
        };
        let mut changed = watch_fixture();
        changed.arrays[0].sync.as_mut().unwrap().percent = Some(99.9);
        let a = render_status_watch_frame(&watch_fixture(), &meta);
        let b = render_status_watch_frame(&changed, &meta);
        assert_ne!(a, b);
    }

    // --- render_fs_df ---------------------------------------------------------

    /// Heterogeneous group sizes: one group with full live Btrfs usage known,
    /// one fresh group (long name/mount) with none of it known at all.
    fn fs_df_fixture() -> FsDfReport {
        FsDfReport {
            schema_version: 2,
            groups: vec![
                GroupDfStatus {
                    name: "default".to_string(),
                    mount_point: "/mnt/shr_data".to_string(),
                    usable_bytes: 8_000_000_000_000,
                    data_used_bytes: Some(3_000_000_000_000),
                    data_total_bytes: Some(8_000_000_000_000),
                    metadata_used_bytes: Some(10_000_000_000),
                    metadata_total_bytes: Some(20_000_000_000),
                    unallocated_bytes: Some(5_000_000_000_000),
                    statvfs_avail_bytes: Some(4_900_000_000_000),
                },
                GroupDfStatus {
                    name: "fresh-group-with-a-long-name".to_string(),
                    mount_point: "/mnt/shr_fresh_group_mountpoint".to_string(),
                    usable_bytes: 4_000_000_000_000,
                    data_used_bytes: None,
                    data_total_bytes: None,
                    metadata_used_bytes: None,
                    metadata_total_bytes: None,
                    unallocated_bytes: None,
                    statvfs_avail_bytes: None,
                },
            ],
        }
    }

    #[test]
    fn render_fs_df_snapshot() {
        insta::assert_snapshot!(render_fs_df(&fs_df_fixture()));
    }

    #[test]
    fn render_fs_df_never_fabricates_unknown_values() {
        let text = render_fs_df(&fs_df_fixture());
        let row = text
            .lines()
            .find(|l| l.contains("fresh-gro"))
            .expect("row for the group with no live usage data is present");
        assert!(row.contains('?'), "{row}");
    }
}
