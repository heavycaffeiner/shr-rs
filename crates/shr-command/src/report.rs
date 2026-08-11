//! Serializable report types — the JSON contract for the CLI (`--json`) and,
//! through it, the Cockpit plugin.

use std::collections::BTreeMap;

use serde::Serialize;

/// Version of the JSON report contract. Bump on breaking shape changes so the
/// Cockpit plugin (and any other consumer) can adapt.
///
/// Bumped 1 -> 2 when `StatusReport` gained `groups`: that's not merely an
/// additive field a v1 consumer could ignore, it's the whole reason Cockpit's
/// old "list of disks and md devices" view stopped being able to answer "how
/// many SHR groups does this host have, and which mode is each in" -- a
/// question the pre-multi-group inventory-only report had no way to answer at
/// all. Cockpit's `model.ts` deliberately hard-rejects anything other than
/// exactly `2` (see `parseStatusOutput`) rather than treating this as
/// forward-compatible, because silently rendering `groups: []` for a v1
/// payload that simply predates the field would look identical to "this host
/// really has zero groups" -- the two cases must not be confused.
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Degraded,
    /// No array present yet (or state unknown).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartState {
    Ok,
    Warning,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmartSummary {
    pub state: SmartState,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    /// Detail signals so the UI can show e.g. "1 Pending Sector".
    pub pending_sectors: Option<u64>,
    pub reallocated_sectors: Option<u64>,
    pub uncorrectable_sectors: Option<u64>,
    pub nvme_critical_warning: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiskStatus {
    pub name: String,
    /// Stable `/dev/disk/by-id` name when known. Optional additive field —
    /// schema stays v1; Cockpit ignores unknown keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub size: Option<u64>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub rotational: Option<bool>,
    pub smart: SmartSummary,
    /// mdadm arrays this disk backs (by md name).
    pub arrays: Vec<String>,
    /// True when this disk holds OS mounts (`/`, `/boot`, …).
    #[serde(default)]
    pub system_disk: bool,
    /// System mountpoints observed under this disk (empty if none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_mounts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncSummary {
    pub action: String,
    pub percent: Option<f64>,
    pub finish_min: Option<f64>,
}

/// One member device's live state from `/proc/mdstat` (e.g. `sdd1[3](F)`) --
/// additive alongside `members`/`ArrayStatus::members` (which stays a plain
/// `Vec<String>` for backward compatibility, see `DiskStatus::id`'s doc
/// comment for this project's precedent on additive fields). Without
/// this, a faulty/spare member is indistinguishable from a healthy one in
/// the JSON contract, which is how Cockpit ended up counting a faulty
/// member into a group's slice math.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemberStatus {
    pub name: String,
    pub role: Option<u32>,
    pub faulty: bool,
    pub spare: bool,
    pub write_mostly: bool,
    pub replacement: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArrayStatus {
    pub name: String,
    pub level: Option<String>,
    pub state: String,
    pub read_only: bool,
    pub degraded: bool,
    pub raid_disks: Option<usize>,
    pub active_disks: Option<usize>,
    pub members: Vec<String>,
    /// Per-member faulty/spare/role detail for the same devices `members`
    /// names, in the same order. See [`MemberStatus`].
    pub member_states: Vec<MemberStatus>,
    pub sync: Option<SyncSummary>,
}

/// Outcome of the most recent scrub on a band, decoupled from
/// `shr_state::ScrubOutcome` for the same reason `GroupBandStatus` doesn't
/// re-export `StateBand` wholesale (see that struct's doc comment): the JSON
/// contract's shape must not move just because `state.toml`'s internal
/// representation does.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrubOutcome {
    Completed,
    Cancelled,
    Failed,
}

/// Projection of `shr_state::StateScrubResult` onto the JSON contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScrubSummary {
    pub finished_at: String,
    pub outcome: ScrubOutcome,
    pub error_count: u64,
}

/// One RAID band inside a group, as reported to Cockpit/the CLI. A subset of
/// `shr_state::StateBand` -- deliberately not the whole struct: `part_uuid`s
/// and other on-disk-only bookkeeping have no reader here, and re-exporting
/// `shr-state`'s type directly would couple the JSON contract's shape to
/// `state.toml`'s internal representation (a state.toml field rename would
/// then silently become a Cockpit-breaking change).
/// `Default` exists so construction sites (and the several test fixtures
/// across this workspace) spell out only the fields they actually decide.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GroupBandStatus {
    pub index: u8,
    pub level: String,
    pub md_name: String,
    /// This band's live mdadm array UUID, mirrored straight from
    /// `StateBand::md_uuid` (state.toml) -- already the real `MD_UUID`
    /// `mdadm --detail --export` read back at band-creation/grow time (see
    /// `MdadmExecutor::read_uuid`), so surfacing it here costs zero extra
    /// commands per `status` call, unlike re-running `mdadm --detail` live.
    /// `None` until that best-effort read has succeeded at least once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md_uuid: Option<String>,
    pub usable_bytes: u64,
    /// Mirrors `shr_state::StateBand::resize_pending` -- see that field's doc
    /// comment for what "pending" means (a `--grow` reshape completed but the
    /// LVM/Btrfs layer above it hasn't been resized to match yet).
    pub resize_pending: bool,
    /// Kernel member device names (e.g. `sdb1`) backing this band right now,
    /// sourced from live `mdstat` inventory (matched by `md_name`), not
    /// `state.toml` -- `StateBand` only stores partition UUIDs, which aren't
    /// meaningful to show a human. Empty when this band's `md_name` isn't
    /// (yet, or currently) a live md array -- e.g. right after a crash before
    /// `reconcile` re-assembles it. Never fabricated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Per-member faulty/spare/role detail for `members`, sourced the same
    /// way (live `mdstat`, matched by `md_name`). Empty under the same
    /// conditions `members` is. See [`MemberStatus`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_states: Vec<MemberStatus>,
    /// This band's live resync/check/reshape progress, if any -- sourced the
    /// same way as `members` (matched against live `mdstat` by `md_name`).
    /// `None` covers both "no mdadm array with this name right now" and
    /// "array exists but is idle" -- this report never guesses which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncSummary>,
    /// This band's most recent scrub result, mirrored from
    /// `StateBand::last_scrub`. `None` if no scrub has ever run against
    /// this band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scrub: Option<ScrubSummary>,
    /// This band's speed control: what the array is syncing at right now,
    /// the limits actually in force, the profile that put them there, and
    /// what the last throttle tick decided and why. Additive optional
    /// fields, same precedent as `md_uuid` -- absent rather than null when
    /// unknown, so "nothing is syncing" reads differently from "syncing at
    /// 0 KB/s". Without these there is no way to learn that a sync is
    /// running at its floor, or why, short of reading sysfs by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_speed_kb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_speed_min_kb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_speed_max_kb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_capability_kb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_capability_observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_throttle_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_throttle_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_throttle_speed_kb: Option<u64>,
    /// Mirrors `StateBand::scrub_in_progress`.
    #[serde(default)]
    pub scrub_in_progress: bool,
    /// Which live member `StateBand::pending_member_removal`
    /// still has queued for `mdadm --remove` once its `replace_disk` copy
    /// finishes. Without this, an operator seeing an extra member attached
    /// to a band has no way to tell "my own replace is still finishing, it
    /// will clear itself" from "something unexpected is going on here".
    ///
    /// Measured on a real guest, and worth stating precisely because an
    /// earlier version of this comment got it wrong: for the whole duration
    /// of a `disk replace` copy, `/proc/mdstat` shows the old member as
    /// `loop10p1[0](R)` -- **replacement**, NOT `(F)`/faulty -- and the
    /// array stays `[UUU]`. Keeping the old member healthy and in-sync
    /// throughout is precisely what `mdadm --replace` buys over
    /// `--fail`+`--remove`: redundancy is never surrendered mid-copy. So
    /// `member_states[].faulty` stays `false`, and this field is the only
    /// thing in the report that explains the extra member at all.
    ///
    /// Carries the KERNEL device name (`sdb1`, matching `MemberStatus::name`/
    /// `members`'s own naming, resolved from state.toml's by-partuuid path
    /// against live `lsblk` PARTUUID data by `ops::band_status`) so a
    /// frontend can look up the exact `member_states` row this refers to,
    /// not just learn that *something* is pending somewhere in the band.
    /// Falls back to the raw `/dev/disk/by-partuuid/<uuid>` path
    /// `state.toml` holds when that resolution fails (e.g. the disk has
    /// been physically pulled and lsblk no longer lists its PARTUUID at
    /// all) -- the fact that a removal is pending must never silently
    /// disappear just because per-member correlation couldn't be done.
    /// `None` only when `StateBand::pending_member_removal` itself is
    /// `None` -- never fabricated (see this struct's `members` field for
    /// this same "never fabricated" precedent). Additive: no schema version
    /// bump, following `DiskStatus::id`'s and `md_uuid`'s precedent above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_member_removal: Option<String>,
}

/// One SHR/SHR-2 group recorded in `state.toml`, as reported to Cockpit/the
/// CLI. Sourced from `shr_state::ArrayState` -- see `build_status`'s state
/// parameter -- never invented: a host with no `state.toml` yet reports an
/// empty `groups` list, not fabricated entries.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupStatus {
    pub name: String,
    /// `"shr"` or `"shr2"`, verbatim from `ArrayState::mode`.
    pub mode: String,
    pub layout_version: u32,
    pub mount_point: String,
    pub fs_uuid: Option<String>,
    /// Mirrors `StateFilesystem::{vg_name,lv_name,compression}` verbatim --
    /// always known once a group exists (state.toml requires them).
    pub vg_name: String,
    pub lv_name: String,
    pub compression: String,
    /// Sum of every band's `usable_bytes` -- the group's total usable
    /// capacity, independent of the live inventory in `disks`/`arrays`
    /// above (which describes raw hardware, not this group's logical view).
    pub usable_bytes: u64,
    /// True if ANY band in this group has a deferred LVM/Btrfs resize
    /// pending (see `GroupBandStatus::resize_pending`). A single group-level
    /// flag because that's what a human glancing at the group list needs;
    /// `bands` below still carries the per-band detail for anyone who needs
    /// to know which one.
    pub resize_pending: bool,
    /// Member disk ids (the same by-id strings `StateDisk::id` stores),
    /// independent of `bands[].members` below since a disk can back more
    /// than one band.
    pub disks: Vec<String>,
    pub bands: Vec<GroupBandStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub health: Health,
    pub disks: Vec<DiskStatus>,
    pub arrays: Vec<ArrayStatus>,
    /// Every SHR group `state.toml` records. Empty (never omitted, never
    /// fabricated) when no state file exists yet -- see `build_status`.
    pub groups: Vec<GroupStatus>,
    /// The `state.toml` path THIS invocation actually resolved and loaded
    /// from. `build_status` never sets this itself -- same reason it
    /// takes `state: Option<&StateFile>` instead of loading one itself (see
    /// this module's doc comment on that parameter): loading, and knowing
    /// the path used to load, is the caller's job. `None` only means the
    /// caller never told us one (e.g. a unit-test fixture, or the TUI, which
    /// doesn't surface this field); it must never be guessed or defaulted to
    /// a plausible-looking path. Additive: no schema version bump, following
    /// `DiskStatus::id`'s and `GroupBandStatus::md_uuid`'s precedent for
    /// optional fields added after v2 shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BandReport {
    pub index: u8,
    pub level: String,
    /// Per-member slice size in bytes.
    pub size: u64,
    pub members: Vec<String>,
    pub usable: u64,
    pub raw: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricsReport {
    pub total_usable: u64,
    pub total_raw: u64,
    pub redundancy_overhead: u64,
    pub stranded_bytes: u64,
    pub waste_ratio: f64,
    pub utilization: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanReport {
    pub schema_version: u32,
    pub mode: String,
    pub bands: Vec<BandReport>,
    pub metrics: MetricsReport,
    pub unusable_per_disk: BTreeMap<String, u64>,
    pub warnings: Vec<String>,
}

/// Best-effort Btrfs chunk-allocation figures for one group's filesystem, as
/// the caller happens to have them (e.g. `shr_exec::BtrfsExecutor::usage`,
/// run against the group's mount point). Every field is independently
/// optional -- **nothing here is invented**: a caller with no live usage
/// data passes
/// `FsUsageInput::default()` and `render_fs_df` shows `?` for every one of
/// these, not a fabricated number. This mirrors how `build_status` treats
/// `state: None` as a real, valid "don't know yet" case rather than an error.
///
/// This intentionally does NOT include an "available" figure derived from
/// `usable_bytes` minus something -- that arithmetic belongs to the render
/// layer (see `render_fs_df`), not to what was actually observed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FsUsageInput {
    pub data_used_bytes: Option<u64>,
    pub data_total_bytes: Option<u64>,
    pub metadata_used_bytes: Option<u64>,
    pub metadata_total_bytes: Option<u64>,
    /// Raw space Btrfs has not yet allocated to any chunk -- the figure this
    /// module's docs call more trustworthy than a plain `df`'s "available"
    /// (see `render_fs_df`'s header note).
    pub unallocated_bytes: Option<u64>,
    /// The `statvfs`-style "available" figure a plain `df` would show --
    /// kept alongside `unallocated_bytes` specifically so the render can
    /// juxtapose the two and explain why they can disagree.
    pub statvfs_avail_bytes: Option<u64>,
}

/// One group's row in an `fs df` report: `GroupStatus`'s already-known
/// logical capacity plus whatever live Btrfs usage the caller supplied via
/// [`FsUsageInput`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupDfStatus {
    pub name: String,
    pub mount_point: String,
    /// Sum of this group's bands' `usable_bytes` (post-redundancy, logical)
    /// -- always known, from `state.toml`, independent of whether live
    /// Btrfs usage was supplied.
    pub usable_bytes: u64,
    pub data_used_bytes: Option<u64>,
    pub data_total_bytes: Option<u64>,
    pub metadata_used_bytes: Option<u64>,
    pub metadata_total_bytes: Option<u64>,
    pub unallocated_bytes: Option<u64>,
    pub statvfs_avail_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FsDfReport {
    pub schema_version: u32,
    pub groups: Vec<GroupDfStatus>,
}
