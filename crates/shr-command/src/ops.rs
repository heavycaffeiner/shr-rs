//! The two read-only operations: build a status report from an inspector, and
//! a dry-run plan report from a proposed disk set.

use std::collections::BTreeMap;

use shr_core::{plan_initial, Disk, PlannerInput, PlannerOutput, PlannerWarning, RedundancyMode};
use shr_inspect::{BlockDevice, Inspector, LsblkOutput, MdArray, SmartInfo};
use shr_state::{ArrayState, StateBand, StateFile, StateScrubResult};

use crate::report::*;
use crate::ShrError;

/// Build a status report from whatever an [`Inspector`] reports, plus
/// whatever `state.toml` content the caller has (or hasn't) loaded.
///
/// `state` is `Option<&StateFile>` rather than, say, a `StateStore` path or a
/// trait to load one, for two reasons:
/// 1. Loading is the caller's job (`shr-cli`'s `Status` handler reads the real
///    `/var/lib/shr-rs/state.toml` via `StateStore`; tests here construct a
///    `StateFile` fixture directly) -- `build_status` itself never touches a
///    filesystem, matching how it already takes `&dyn Inspector` instead of
///    reaching for `/proc/mdstat` itself. That keeps this function unit
///    testable without a real host's `/var/lib/shr-rs` (a hard rule here: no
///    test may depend on host filesystem state).
/// 2. `None` is a real, valid case -- a fresh host has no state.toml at all
///    (nothing has ever been `create`d) -- and must produce an EMPTY
///    `groups` list, never an error and never fabricated data. `Option`
///    makes that case explicit at the call site instead of requiring an
///    empty-but-still-real `StateFile` sentinel value.
pub fn build_status(inspector: &dyn Inspector, state: Option<&StateFile>) -> Result<StatusReport, ShrError> {
    let lsblk = inspector.block_devices()?;
    let md = inspector.mdstat()?;
    let by_id = inspector.by_id_index()?;

    let mut disks = Vec::new();
    for d in lsblk.disks() {
        let smart = inspector.smart(&d.name).unwrap_or_default();
        let arrays: Vec<String> = md
            .arrays
            .iter()
            .filter(|a| a.members.iter().any(|m| member_belongs(&m.name, &d.name)))
            .map(|a| a.name.clone())
            .collect();
        let system_mounts = shr_inspect::system_mounts_on(d);
        let system_disk = !system_mounts.is_empty();
        disks.push(DiskStatus {
            name: d.name.clone(),
            id: by_id.id_for_kernel(&d.name).map(|id| id.as_str().to_string()),
            size: d.size,
            model: d.model_trimmed(),
            serial: d.serial_trimmed(),
            rotational: d.rota,
            smart: smart_summary(&smart),
            arrays,
            system_disk,
            system_mounts,
        });
    }

    let arrays: Vec<ArrayStatus> = md.arrays.iter().map(array_status).collect();

    // Empty, never an error, when there's no state.toml yet (fresh host, or
    // Cockpit polling before anything has been `create`d) -- `state` being
    // `None` must not be conflated with "state.toml exists but is empty."
    let groups: Vec<GroupStatus> = state
        .map(|s| {
            s.groups
                .iter()
                .map(|g| group_status(inspector, g, &arrays, &lsblk))
                .collect()
        })
        .unwrap_or_default();

    // Computed from the flat live-array list ALONE used to miss a
    // whole group going unassembled. Real shape: host has group A (live,
    // healthy) and group B (all disks/mdadm arrays absent, e.g. after a
    // reboot without persistent loop devices). B's arrays never appear in
    // `md.arrays`, so `md.arrays.is_empty()` is false (A keeps it
    // non-empty) and `array_needs_attention` never even looks at B -- the
    // report claimed Healthy while B was entirely gone. `band_status`
    // already leaves `members` empty for exactly this case (no live mdadm
    // array sharing the band's `md_name`); this just consults it.
    //
    // Folded into `Degraded` rather than a new `Health` variant: this
    // report's `--json` health field is a closed 3-value enum the Cockpit
    // plugin (`model.ts`) hard-rejects any other value of, gated by
    // `SCHEMA_VERSION`. A 4th variant would need a version bump, and
    // `parseStatusOutput` rejects any `schema_version` other than exactly 2
    // outright -- bumping it here would break EVERY status call (not just
    // this edge case) for every existing consumer until they're updated,
    // which is out of scope for this fix (see the accompanying report).
    // `Degraded` is the closest fit already: "something needs attention,
    // not fully fine" -- a group with no live array is a stronger case of
    // exactly that, not a different question ("no array present yet" is
    // `Unknown`'s meaning, which does not apply here since group A IS
    // live).
    let any_group_unassembled = groups
        .iter()
        .any(|g| g.bands.iter().any(|b| b.members.is_empty()));

    // Healthy only if every array is active/clean, writable, structurally
    // possible, not degraded, AND no state.toml group is missing its live
    // array entirely.
    let health = if md.arrays.is_empty() && !any_group_unassembled {
        Health::Unknown
    } else if any_group_unassembled || md.arrays.iter().any(array_needs_attention) {
        Health::Degraded
    } else {
        Health::Healthy
    };

    Ok(StatusReport {
        schema_version: SCHEMA_VERSION,
        health,
        disks,
        arrays,
        groups,
        // This function has no filesystem access (see the doc comment
        // on `state` above) and so cannot know which path the caller loaded
        // `state` from -- the caller (shr-cli's `Status` handler) stamps the
        // real one in afterward.
        state_path: None,
    })
}

/// Project one `state.toml` group down to the fields Cockpit/the CLI need.
/// See `GroupStatus`'s doc comment for why this doesn't just re-export
/// `ArrayState` wholesale. `arrays` is the same live-`mdstat`-derived list
/// `build_status` already computed -- passed through so each band can be
/// matched to its live array by `md_name` (see `band_status`). `lsblk` is
/// `build_status`'s own already-fetched inventory, threaded through so
/// `band_status` can resolve `pending_member_removal`'s by-partuuid path
/// without a second `lsblk` call.
fn group_status(
    inspector: &dyn Inspector,
    g: &ArrayState,
    arrays: &[ArrayStatus],
    lsblk: &LsblkOutput,
) -> GroupStatus {
    let bands: Vec<GroupBandStatus> = g
        .bands
        .iter()
        .map(|b| band_status(inspector, b, arrays, lsblk))
        .collect();
    let usable_bytes = bands.iter().map(|b| b.usable_bytes).sum();
    let resize_pending = bands.iter().any(|b| b.resize_pending);

    GroupStatus {
        name: g.name.clone(),
        mode: g.mode.clone(),
        layout_version: g.layout_version,
        mount_point: g.filesystem.mount_point.clone(),
        fs_uuid: g.filesystem.fs_uuid.clone(),
        vg_name: g.filesystem.vg_name.clone(),
        lv_name: g.filesystem.lv_name.clone(),
        compression: g.filesystem.compression.clone(),
        usable_bytes,
        resize_pending,
        disks: g.disks.iter().map(|d| d.id.clone()).collect(),
        bands,
    }
}

/// Project one `state.toml` band down to the fields Cockpit/the CLI need,
/// enriched with whatever live `mdstat` array shares its `md_name` --
/// `members`/`sync` come from there (state.toml doesn't track live progress
/// or human-readable member names), `last_scrub`/`scrub_in_progress` come
/// from `state.toml` itself (mdstat has no scrub history). `None`/empty when
/// no live array with this name exists right now (e.g. between a crash and
/// `reconcile` re-assembling it) -- never guessed.
fn band_status(
    inspector: &dyn Inspector,
    b: &StateBand,
    arrays: &[ArrayStatus],
    lsblk: &LsblkOutput,
) -> GroupBandStatus {
    let live = arrays.iter().find(|a| a.name == b.md_name);
    // Read live rather than mirrored from `state.toml`: what the kernel
    // actually has in force is the thing an operator needs, and it can
    // differ from what this project last wrote (an operator's own `echo`,
    // a host-wide value left behind by something else).
    let limits = inspector.md_sync_limits(&b.md_name);
    GroupBandStatus {
        index: b.index,
        level: b.level.clone(),
        md_name: b.md_name.clone(),
        md_uuid: b.md_uuid.clone(),
        usable_bytes: b.usable_bytes,
        resize_pending: b.resize_pending,
        members: live.map(|a| a.members.clone()).unwrap_or_default(),
        member_states: live.map(|a| a.member_states.clone()).unwrap_or_default(),
        sync: live.and_then(|a| a.sync.clone()),
        last_scrub: b.last_scrub.as_ref().map(scrub_summary),
        scrub_in_progress: b.scrub_in_progress,
        pending_member_removal: b
            .pending_member_removal
            .as_deref()
            .map(|raw| resolve_pending_removal(lsblk, raw)),
        sync_speed_kb: limits.speed_kb,
        sync_speed_min_kb: limits.min_kb,
        sync_speed_max_kb: limits.max_kb,
        sync_priority: b.sync_priority.clone(),
        sync_capability_kb: b.sync_capability_kb,
        sync_capability_observed_at: b.sync_capability_observed_at.clone(),
        last_throttle_decision: b.last_throttle_decision.clone(),
        last_throttle_reason: b.last_throttle_reason.clone(),
        last_throttle_speed_kb: b.last_throttle_speed_kb,
    }
}

/// Resolve `raw` (`StateBand::pending_member_removal`, typically
/// `/dev/disk/by-partuuid/<uuid>`) to whatever kernel device name (e.g.
/// `sdb1`) live `lsblk` currently reports owning that PARTUUID -- the same
/// naming `MemberStatus::name`/`ArrayStatus::members` use (see
/// `member_status`), which is the only thing a frontend can actually match
/// against `member_states` to find the specific row this refers to. Falls
/// back to `raw` verbatim when resolution fails (not a by-partuuid path, or
/// no currently-visible partition carries that PARTUUID -- e.g. the disk has
/// been physically pulled) so the FACT that a removal is pending never
/// silently disappears just because per-member correlation couldn't be done.
fn resolve_pending_removal(lsblk: &LsblkOutput, raw: &str) -> String {
    kernel_name_for_partuuid(lsblk, raw).unwrap_or_else(|| raw.to_string())
}

/// `None` when `raw` isn't a `/dev/disk/by-partuuid/<uuid>` path, or no
/// device (disk or partition, at any depth) in `lsblk`'s tree currently
/// reports that PARTUUID. Case-insensitive: GPT PARTUUIDs from `lsblk` and
/// from `StatePartition::part_uuid` (the source of the by-partuuid paths
/// `replace_disk`/`reconcile_pending_member_removals` build, in
/// shr-orchestrate) are not guaranteed to share letter case.
fn kernel_name_for_partuuid(lsblk: &LsblkOutput, raw: &str) -> Option<String> {
    let uuid = raw.strip_prefix("/dev/disk/by-partuuid/")?;
    fn find<'a>(devices: &'a [BlockDevice], uuid: &str) -> Option<&'a str> {
        for d in devices {
            if d.partuuid
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(uuid))
            {
                return Some(d.name.as_str());
            }
            if let Some(found) = find(&d.children, uuid) {
                return Some(found);
            }
        }
        None
    }
    find(&lsblk.blockdevices, uuid).map(str::to_string)
}

fn scrub_summary(s: &StateScrubResult) -> ScrubSummary {
    ScrubSummary {
        finished_at: s.finished_at.clone(),
        outcome: match s.outcome {
            shr_state::ScrubOutcome::Completed => ScrubOutcome::Completed,
            shr_state::ScrubOutcome::Cancelled => ScrubOutcome::Cancelled,
            shr_state::ScrubOutcome::Failed => ScrubOutcome::Failed,
        },
        error_count: s.error_count,
    }
}

fn array_is_active(a: &MdArray) -> bool {
    a.state == "active" || a.state == "clean"
}

fn array_needs_attention(a: &MdArray) -> bool {
    let impossible_raid6 = a
        .level
        .as_deref()
        .is_some_and(|level| level.eq_ignore_ascii_case("raid6"))
        && a.raid_disks.unwrap_or(a.members.len()) < 4;
    a.is_degraded() || a.read_only || !array_is_active(a) || impossible_raid6
}

/// Does member device `member` belong to disk `disk`? The member is either the
/// whole disk, or a partition of it. Partition naming depends on whether the
/// disk name ends in a digit:
/// - ends in a letter (`sda`) → partition is `sda` + digits (`sda1`);
/// - ends in a digit (`nvme0n1`, `mmcblk0`) → partition is `…` + `p` + digits
///   (`nvme0n1p1`).
///
/// This avoids prefix collisions like `nvme0n1` vs `nvme0n10p1` and
/// `dm-1` vs `dm-10`.
///
/// Limitation: this only follows one level (disk → partition). Stacked members
/// (a `dm-*`/multipath device, or a nested md array used as a member) are not
/// resolved back to their physical backing disks — that needs full topology
/// walking and is deferred.
fn member_belongs(member: &str, disk: &str) -> bool {
    if disk.is_empty() || member.is_empty() {
        return false;
    }
    if member == disk {
        return true;
    }
    let Some(rest) = member.strip_prefix(disk) else {
        return false;
    };
    let disk_ends_digit = disk.chars().last().is_some_and(|c| c.is_ascii_digit());
    let part = if disk_ends_digit {
        match rest.strip_prefix('p') {
            Some(r) => r,
            None => return false,
        }
    } else {
        rest
    };
    !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())
}

fn smart_summary(s: &SmartInfo) -> SmartSummary {
    let state = if s.has_warning() {
        SmartState::Warning
    } else if s.is_unknown() {
        SmartState::Unknown
    } else {
        SmartState::Ok
    };
    SmartSummary {
        state,
        temperature_c: s.temperature_c,
        power_on_hours: s.power_on_hours,
        pending_sectors: s.pending_sectors,
        reallocated_sectors: s.reallocated_sectors,
        uncorrectable_sectors: s.uncorrectable_sectors,
        nvme_critical_warning: s.nvme_critical_warning,
    }
}

fn array_status(a: &MdArray) -> ArrayStatus {
    ArrayStatus {
        name: a.name.clone(),
        level: a.level.clone(),
        state: a.state.clone(),
        read_only: a.read_only,
        degraded: a.is_degraded(),
        raid_disks: a.raid_disks,
        active_disks: a.active_disks,
        members: a.members.iter().map(|m| m.name.clone()).collect(),
        member_states: a.members.iter().map(member_status).collect(),
        sync: a.sync.as_ref().map(|s| SyncSummary {
            action: s.action.clone(),
            percent: s.percent,
            finish_min: s.finish_min,
        }),
    }
}

/// Project one live `/proc/mdstat` member (`shr_inspect::MdMember`)
/// down to the JSON contract's [`MemberStatus`].
fn member_status(m: &shr_inspect::MdMember) -> MemberStatus {
    MemberStatus {
        name: m.name.clone(),
        role: m.role,
        faulty: m.faulty,
        spare: m.spare,
        write_mostly: m.write_mostly,
        replacement: m.replacement,
    }
}

/// Build an `fs df` report from already-known group capacity (`groups`, the
/// same slice `StatusReport::groups` carries) plus whatever live Btrfs usage
/// figures the caller has for each one (`usage`, keyed by group name).
///
/// Pure and total: a group with no entry in `usage` (e.g. a caller that
/// hasn't run `shr_exec::BtrfsExecutor::usage` for it) simply reports every
/// `FsUsageInput` field as `None`, which `render_fs_df` shows as `?`. Never
/// fabricated.
pub fn build_fs_df(groups: &[GroupStatus], usage: &BTreeMap<String, FsUsageInput>) -> FsDfReport {
    let rows = groups
        .iter()
        .map(|g| {
            let u = usage.get(&g.name).cloned().unwrap_or_default();
            GroupDfStatus {
                name: g.name.clone(),
                mount_point: g.mount_point.clone(),
                usable_bytes: g.usable_bytes,
                data_used_bytes: u.data_used_bytes,
                data_total_bytes: u.data_total_bytes,
                metadata_used_bytes: u.metadata_used_bytes,
                metadata_total_bytes: u.metadata_total_bytes,
                unallocated_bytes: u.unallocated_bytes,
                statvfs_avail_bytes: u.statvfs_avail_bytes,
            }
        })
        .collect();
    FsDfReport {
        schema_version: SCHEMA_VERSION,
        groups: rows,
    }
}

/// Dry-run: plan the initial layout for `disks` in `mode` and report it. Never
/// touches any device.
pub fn build_plan_report(mode: RedundancyMode, disks: Vec<Disk>) -> Result<PlanReport, ShrError> {
    let out = plan_initial(&PlannerInput::new(disks, mode))?;
    Ok(plan_to_report(mode, &out))
}

/// Run Stage A write preflight for kernel device names (create/expand targets).
///
/// Does not plan or execute; only reports blockers (system disk, missing
/// by-id, unknown size, existing content unless `force_content`).
pub fn preflight_create(
    inspector: &dyn Inspector,
    kernel_names: &[String],
    force_content: bool,
) -> Result<shr_inspect::WritePreflight, ShrError> {
    let lsblk = inspector.block_devices()?;
    let by_id = inspector.by_id_index()?;
    Ok(shr_inspect::preflight_write_targets(
        kernel_names,
        &lsblk,
        &by_id,
        force_content,
    ))
}

/// Every alias -- kernel name and, if known, by-id name -- of every disk on
/// the host that currently holds a system mountpoint (`/`, `/boot`, ...).
///
/// This scans the *whole* inventory, deliberately independent of whatever
/// disks a create/expand request targets: a request for only data disks is
/// the normal case, and if this were built only from the requested set (as
/// an earlier version did, via `preflight_create`'s `kernel_names` filter)
/// it would come back empty for exactly that normal case -- which then made
/// `shr_exec::SafetyGuard::validate_disk_target`'s "empty list is an error"
/// rule (D4) reject every legitimate request. Feed the result of this
/// function to that guard's `system_disks` list: it does exact-string
/// comparison, so every spelling a destructive-command disk_path might
/// arrive in must be present, not just the kernel name.
pub fn system_disk_aliases(inspector: &dyn Inspector) -> Result<Vec<String>, ShrError> {
    let lsblk = inspector.block_devices()?;
    let by_id = inspector.by_id_index()?;
    let mut out = Vec::new();
    for d in lsblk.disks() {
        if shr_inspect::is_system_disk(d) {
            out.push(d.name.clone());
            if let Some(id) = by_id.id_for_kernel(&d.name) {
                out.push(id.as_str().to_string());
            }
        }
    }
    Ok(out)
}

fn plan_to_report(mode: RedundancyMode, out: &PlannerOutput) -> PlanReport {
    let bands = out
        .bands
        .iter()
        .map(|b| BandReport {
            index: b.band_index(),
            level: format!("{:?}", b.level()).to_lowercase(),
            size: b.size(),
            members: b.members().iter().map(|m| m.to_string()).collect(),
            usable: b.usable_bytes(),
            raw: b.raw_bytes(),
        })
        .collect();

    let m = &out.metrics;
    let metrics = MetricsReport {
        total_usable: m.total_usable,
        total_raw: m.total_raw,
        redundancy_overhead: m.redundancy_overhead,
        stranded_bytes: m.stranded_bytes,
        waste_ratio: m.waste_ratio,
        utilization: m.utilization.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
    };

    PlanReport {
        schema_version: SCHEMA_VERSION,
        mode: format!("{mode:?}").to_lowercase(),
        bands,
        metrics,
        unusable_per_disk: out
            .unusable_per_disk
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect(),
        warnings: out.warnings.iter().map(fmt_warning).collect(),
    }
}

fn fmt_warning(w: &PlannerWarning) -> String {
    match w {
        PlannerWarning::TooFewForBand {
            offset,
            size,
            members,
            needed,
        } => format!("slice at offset {offset} ({size} B) has only {members} member(s), needs {needed}"),
        PlannerWarning::UnusableTail { disk, bytes } => {
            format!("{disk}: {bytes} B stranded (no redundancy)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_fs_df, member_belongs};
    use crate::report::{FsUsageInput, GroupStatus};
    use std::collections::BTreeMap;

    #[test]
    fn member_belongs_partition_naming_and_collisions() {
        assert!(member_belongs("sda1", "sda"));
        assert!(member_belongs("sda", "sda")); // whole-disk member
        assert!(!member_belongs("sdb1", "sda"));
        assert!(!member_belongs("sdcd1", "sdc")); // sdcd1 is not a partition of sdc
        assert!(member_belongs("nvme0n1p1", "nvme0n1"));
        assert!(!member_belongs("nvme0n10p1", "nvme0n1")); // prefix collision avoided
        assert!(member_belongs("mmcblk0p2", "mmcblk0"));
        assert!(!member_belongs("dm-10", "dm-1")); // prefix collision avoided
                                                   // Empty names never match.
        assert!(!member_belongs("", ""));
        assert!(!member_belongs("1", ""));
        assert!(!member_belongs("", "sda"));
    }

    fn group(name: &str, usable_bytes: u64) -> GroupStatus {
        GroupStatus {
            name: name.to_string(),
            mode: "shr".to_string(),
            layout_version: 1,
            mount_point: format!("/mnt/{name}"),
            fs_uuid: None,
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
            usable_bytes,
            resize_pending: false,
            disks: vec![],
            bands: vec![],
        }
    }

    #[test]
    fn build_fs_df_carries_the_known_usable_bytes_through_untouched() {
        let groups = vec![group("g1", 4_000_000_000_000)];
        let report = build_fs_df(&groups, &BTreeMap::new());
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].name, "g1");
        assert_eq!(report.groups[0].mount_point, "/mnt/g1");
        assert_eq!(report.groups[0].usable_bytes, 4_000_000_000_000);
    }

    #[test]
    fn build_fs_df_reports_none_rather_than_a_fabricated_number_when_usage_is_missing() {
        // No entry in `usage` at all for "g1" -- every Btrfs-specific field
        // must come back `None`, not e.g. zero or a value derived from
        // `usable_bytes`. This is the whole point of `FsUsageInput` being
        // independently optional per field (see its doc comment).
        let groups = vec![group("g1", 4_000_000_000_000)];
        let report = build_fs_df(&groups, &BTreeMap::new());
        let row = &report.groups[0];
        assert_eq!(row.data_used_bytes, None);
        assert_eq!(row.data_total_bytes, None);
        assert_eq!(row.metadata_used_bytes, None);
        assert_eq!(row.metadata_total_bytes, None);
        assert_eq!(row.unallocated_bytes, None);
        assert_eq!(row.statvfs_avail_bytes, None);
    }

    #[test]
    fn build_fs_df_passes_through_supplied_usage_by_group_name() {
        let groups = vec![group("g1", 4_000_000_000_000), group("g2", 8_000_000_000_000)];
        let mut usage = BTreeMap::new();
        usage.insert(
            "g1".to_string(),
            FsUsageInput {
                data_used_bytes: Some(1_000_000_000_000),
                data_total_bytes: Some(4_000_000_000_000),
                metadata_used_bytes: Some(1_000_000_000),
                metadata_total_bytes: Some(2_000_000_000),
                unallocated_bytes: Some(2_000_000_000_000),
                statvfs_avail_bytes: Some(1_900_000_000_000),
            },
        );
        // g2 deliberately has no entry -- must not borrow g1's numbers.
        let report = build_fs_df(&groups, &usage);

        let g1 = report.groups.iter().find(|g| g.name == "g1").unwrap();
        assert_eq!(g1.data_used_bytes, Some(1_000_000_000_000));
        assert_eq!(g1.unallocated_bytes, Some(2_000_000_000_000));

        let g2 = report.groups.iter().find(|g| g.name == "g2").unwrap();
        assert_eq!(g2.data_used_bytes, None);
        assert_eq!(g2.usable_bytes, 8_000_000_000_000);
    }
}
