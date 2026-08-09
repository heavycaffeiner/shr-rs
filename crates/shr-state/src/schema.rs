use crate::error::StateError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatePartition {
    pub part_uuid: String,
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub band_index: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDisk {
    pub id: String,
    pub size_bytes: u64,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub added_at: String,
    #[serde(default)]
    pub partitions: Vec<StatePartition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateBand {
    pub index: u8,
    pub level: String,
    pub md_name: String,
    pub md_uuid: Option<String>,
    pub member_partitions: Vec<String>,
    pub usable_bytes: u64,
    /// Set when a `--grow` reshape on this band completed for real but the
    /// LVM PV/LV and Btrfs filesystem could not be resized yet because the
    /// reshape's data movement was still in progress (`mdadm --grow` only
    /// STARTS a reshape -- the underlying block device's reported size
    /// doesn't increase until it finishes, which real disks can take a
    /// long time to do; see `execute_grow`'s Step 8 SM-EXPAND-1 finding
    /// and an earlier review). `OrchestrationEngine::reconcile` clears this
    /// once the reshape is confirmed `idle` and the deferred resize has
    /// actually run. `#[serde(default)]` so a `state.toml` written before
    /// this field existed still loads (defaulting to "nothing pending").
    #[serde(default)]
    pub resize_pending: bool,
    /// The total SMART reallocated-sector count last observed across this
    /// band's member disks, persisted so a periodic throttle tick (
    /// the systemd timer) -- which runs in a brand-new process every time,
    /// with no in-memory state surviving between ticks -- can still compute
    /// a real delta ("has it increased since last time") instead of only
    /// ever seeing an absolute count with nothing to compare against.
    /// `#[serde(default)]` so a `state.toml` written before this field
    /// existed still loads (`None` -- "no prior reading", which
    /// `ReshapeThrottle::tick`'s unknown-signal handling already treats
    /// safely).
    #[serde(default)]
    pub last_smart_reallocated: Option<u64>,
    /// This band's most recent scrub result, if one has ever run.
    /// `#[serde(default)]` for the same older compat reason as the field
    /// above.
    #[serde(default)]
    pub last_scrub: Option<StateScrubResult>,
    /// Set by `OrchestrationEngine::scrub_start`, cleared by
    /// `scrub_status`/`scrub_poll` once this band's `sync_action` returns to
    /// `idle` (mirrors `resize_pending`'s "started here, finished there"
    /// shape). Exists so finishing a scrub can be told apart from "this band
    /// has simply always been idle and no scrub ever ran" -- without this,
    /// polling status on a band that was never scrubbed would read `idle`
    /// and wrongly persist a fabricated "0 errors, just completed" result
    /// (the "unknown/never-observed must never be recorded as a known
    /// good outcome" principle, applied to scrub history instead of
    /// throttle metrics). `#[serde(default)]` for the same older compat
    /// reason as the fields above.
    #[serde(default)]
    pub scrub_in_progress: bool,
    /// Old member device path (`/dev/disk/by-partuuid/<uuid>`) `replace_disk`
    /// could not `--remove` yet because its replacement copy was
    /// still running. `reconcile()`/`check_health()` finish the
    /// removal once the array's `sync_action` goes back to `idle` and
    /// clear this. `#[serde(default)]` so older `state.toml` still
    /// loads (`None` -- nothing pending).
    #[serde(default)]
    pub pending_member_removal: Option<String>,
    /// The reshape speed profile (`expand --priority`) chosen for this
    /// band's currently-running reshape, stored as the same lowercase string
    /// `shr-cli`'s `PriorityArg`/`shr_exec::ReshapePriority` round-trip
    /// through. `expand()`'s handler already knows the operator's
    /// choice and applies it in-process for the reshape's FIRST tick
    /// (`start_reshape_throttle`) -- but the periodic tick
    /// (`OrchestrationEngine::tick_active_reshapes`, driven by a systemd
    /// timer) runs in a brand-new process every fire and has no other way to
    /// recover it. Without this field that tick silently fell back to
    /// `ReshapePriority::Balanced` from the first fire onward, discarding
    /// `background`'s tighter braking or `max`'s unlimited ceiling for the
    /// rest of a multi-hour reshape.
    ///
    /// Per-band, not per-group: a reshape is a per-band kernel operation
    /// (`tick_active_reshapes` iterates bands, checking each one's own
    /// `sync_action`), and different bands -- even different GROUPS -- can
    /// have independent reshapes in flight with different chosen priorities
    /// at once (`expand()` only refuses a SECOND concurrent reshape within
    /// the SAME group; see its an earlier review comment). A single
    /// engine-wide value could not represent that.
    ///
    /// Set alongside `resize_pending` (same "started here, finished there"
    /// shape) whenever `execute_grow` leaves a real reshape running, and
    /// cleared alongside it in `reconcile` once the reshape reaches `idle` --
    /// a stale value surviving past its own reshape would silently govern
    /// the band's NEXT one. `#[serde(default)]` so a older `state.toml`
    /// still loads (`None`, and `tick_active_reshapes` falls back to the
    /// engine's own `self.priority`, i.e. `Balanced` unless a caller opts
    /// in -- identical to today's behavior for such a file).
    #[serde(default)]
    pub reshape_priority: Option<String>,
}

/// The outcome of the most recent scrub (`mdadm --action=check` / `btrfs
/// scrub`) run against one band. `error_count` is the headline signal
/// this project's the design cares about -- "completed" alone is not useful,
/// "found N errors" is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateScrubResult {
    /// RFC3339 timestamp of when this scrub finished.
    pub finished_at: String,
    pub outcome: ScrubOutcome,
    /// mdadm: `mismatch_cnt`. Btrfs: `stats.corrected_errors +
    /// stats.uncorrectable_errors` from `btrfs scrub status`.
    pub error_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrubOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFilesystem {
    pub fs_uuid: Option<String>,
    pub mount_point: String,
    pub vg_name: String,
    pub lv_name: String,
    pub compression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCheckpoint {
    pub step_index: usize,
    pub band_index: Option<u8>,
    pub resumable: bool,
    pub description: String,
}

/// Enough of an in-flight `expand()` request's ORIGINAL `new_disks` to
/// re-derive the remaining plan after a crash, without asking the operator
/// to re-supply the same disks. Captured once at `expand()` start,
/// alongside `expansion.in_progress` -- see `StateExpansion::new_disks`.
/// Deliberately just the fields `shr-orchestrate` needs to reconstruct a
/// planner-usable disk (identity + size/serial/model); `kernel_name` is
/// best-effort (may drift across a reboot) and only feeds a redundant
/// secondary safety check, never the primary by-id one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatePendingDisk {
    pub id: String,
    pub kernel_name: String,
    pub size_bytes: u64,
    pub serial: Option<String>,
    pub model: Option<String>,
}

/// `StateExpansion` (and therefore `ArrayState`/`StateFile`) drops `Eq` here
/// -- `shr_core::ExpansionStep` only derives `PartialEq`, so a `plan` field
/// of that type makes a derived `Eq` impossible. `PartialEq` alone is still
/// enough for every `assert_eq!` in this workspace's test suite.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateExpansion {
    pub in_progress: bool,
    pub checkpoint: Option<StateCheckpoint>,
    /// The disks `expand()` was originally asked to add, captured once at
    /// the start of the call and cleared alongside `checkpoint` once the
    /// expansion finishes (success or clean failure). `#[serde(default)]`
    /// so a `state.toml` written before that fix existed still loads (defaulting
    /// to "nothing pending to resume" -- matches `checkpoint.resumable ==
    /// false` for the same older file, see `StateCheckpoint::resumable`).
    #[serde(default)]
    pub new_disks: Vec<StatePendingDisk>,
    /// The exact `plan_expansion` output computed at `expand()` start,
    /// persisted so a resume can continue executing `plan[checkpoint.
    /// step_index..]` verbatim rather than recomputing a plan from
    /// (possibly only partially applied) current state -- recomputing would
    /// mis-plan a disk that already contributed to ONE band but still has
    /// unused capacity for a LATER step's band (a real shape:
    /// `expand_creates_new_band_for_two_larger_disks`'s two 6TB disks each
    /// feed both a `GrowBand` step and a `CreateBand` step). `#[serde(default)]`
    /// for the same older compat reason as `new_disks`.
    #[serde(default)]
    pub plan: Vec<shr_core::ExpansionStep>,
    /// `plan_expansion`'s `target_layout_version`, persisted alongside `plan`
    /// so a resumed expansion's final `layout_version` bump doesn't need the
    /// original plan recomputed to learn it.
    #[serde(default)]
    pub target_layout_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayState {
    /// Identifies this SHR group among the (possibly many) groups a single
    /// host manages -- see `StateFile`. Required (no `#[serde(default)]`):
    /// a group without a name is a modeling error in the CURRENT schema,
    /// unlike the pre-multi-group `state.toml` on disk, which never had this
    /// field at all and is handled separately by `LegacyArrayStateV1`.
    pub name: String,
    pub mode: String,
    pub created_at: String,
    pub layout_version: u32,
    pub disks: Vec<StateDisk>,
    pub bands: Vec<StateBand>,
    pub filesystem: StateFilesystem,
    pub expansion: StateExpansion,
}

/// The on-disk container for every SHR group a host manages, replacing the
/// old one-array-per-file assumption (Phase 4 multi-group support: the demo
/// target is {SHR, SHR-2} x {uniform, heterogeneous} coexisting on one
/// host). `schema_version` exists so a future format change has something
/// to branch on other than "guess from shape" the way this version had to
/// for the pre-`groups` file (see `StateStore::load` / `LegacyArrayStateV1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    pub schema_version: u32,
    pub groups: Vec<ArrayState>,
    /// Arrays this host has destroyed but whose member superblocks are
    /// still on the disks, because the operator chose not to erase them.
    /// See [`StateRetiredArray`]. `#[serde(default)]` so every
    /// `state.toml` written before this field existed still loads, which is
    /// also why `schema_version` does not move: nothing branches on it, and
    /// an absent list means exactly what the default says (nothing
    /// retired). Worth knowing: an older binary reading such a file would
    /// ignore this key and then drop it on the next write, so a downgrade
    /// silently re-arms the auto-assembly this exists to prevent.
    #[serde(default)]
    pub retired_arrays: Vec<StateRetiredArray>,
}

/// An array that `destroy` tore down WITHOUT `--zero-superblocks`.
///
/// Leaving the superblocks is a legitimate choice -- it is what keeps a
/// mistaken `destroy` recoverable by hand -- but on its own it means the
/// kernel's incremental (udev) assembly finds those members at the next
/// boot and brings the dead array back. Observed on a real guest: a
/// destroyed group reappeared as `/dev/md6` after a reboot, holding a
/// device number and showing up in `shr-rs status` as an array belonging to
/// no group.
///
/// Recording the array's UUID here lets `write_mdadm_conf` emit an
/// `ARRAY <ignore> UUID=...` line for it, which mdadm.conf(5) defines as
/// "any array which matches the rest of the line will never be assembled".
///
/// `disk_ids` is what makes the list self-limiting: once those same disks
/// are handed to a new `create`/`expand`, the old array is gone for good
/// and its entry is pruned, so this never grows without bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRetiredArray {
    /// The array's mdadm UUID (`MD_UUID`), the same value `StateBand::
    /// md_uuid` carries and the only identifier that survives the array
    /// being stopped.
    pub md_uuid: String,
    /// The group this band belonged to, so an operator reading
    /// `mdadm.conf`/`state.toml` can tell what the entry refers to.
    pub group_name: String,
    /// by-id names of the disks whose partitions still carry the
    /// superblock. Used to prune this entry when those disks are reused.
    pub disk_ids: Vec<String>,
    pub retired_at: String,
}

/// Current on-disk schema version. Bumped from the implicit "1" (a bare
/// `ArrayState` at the file's top level, no wrapper) to 2 when the `groups`
/// wrapper was introduced.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// The name a pre-multi-group `state.toml` (which had no concept of a group
/// name at all) is migrated to in memory. Also `shr-cli`'s default for
/// `create --name` when the operator doesn't care to name their first group.
pub const DEFAULT_GROUP_NAME: &str = "default";

impl StateFile {
    pub fn new(groups: Vec<ArrayState>) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            groups,
            retired_arrays: Vec::new(),
        }
    }

    /// Wrap a single migrated-from-legacy group as the whole file's content.
    /// Kept as a named constructor (rather than callers hand-rolling
    /// `StateFile { .. }`) so the "this is what migration produces" decision
    /// lives in one place.
    pub fn migrate_single(group: ArrayState) -> Self {
        Self::new(vec![group])
    }

    pub fn find(&self, name: &str) -> Option<&ArrayState> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// Group names must be unique within a single `state.toml` -- two groups
    /// sharing a name would make `expand --name`/CLI lookups ambiguous and
    /// `shr-rs status` unable to tell them apart.
    pub fn validate_unique_group_names(&self) -> Result<(), StateError> {
        let mut seen = std::collections::HashSet::new();
        for g in &self.groups {
            if !seen.insert(g.name.as_str()) {
                return Err(StateError::DuplicateGroupName(g.name.clone()));
            }
        }
        Ok(())
    }

    /// Runs the placeholder-identifier check (see `ArrayState`'s own method)
    /// across EVERY group -- a regression that only validated the first
    /// group, or the last one loaded, would silently let a placeholder
    /// through for any other group in a multi-group file.
    pub fn validate_no_placeholder_identifiers(&self) -> Result<(), StateError> {
        for g in &self.groups {
            g.validate_no_placeholder_identifiers()?;
        }
        Ok(())
    }
}

/// Shape of `state.toml` before multi-group support existed: what is now
/// `StateFile { groups: vec![ArrayState { .. }], .. }` used to just BE this
/// struct at the file's top level, with no `name` field (there was only ever
/// one array, so nothing needed a name) and no `groups`/`schema_version`
/// wrapper. `StateStore::load` falls back to parsing this shape when parsing
/// the current `StateFile` shape fails, and migrates it to a single group
/// named `DEFAULT_GROUP_NAME` -- see that function's doc comment for why
/// this must never silently drop data.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LegacyArrayStateV1 {
    pub mode: String,
    pub created_at: String,
    pub layout_version: u32,
    pub disks: Vec<StateDisk>,
    pub bands: Vec<StateBand>,
    pub filesystem: StateFilesystem,
    #[serde(default)]
    pub expansion: StateExpansion,
}

impl From<LegacyArrayStateV1> for ArrayState {
    fn from(l: LegacyArrayStateV1) -> Self {
        ArrayState {
            name: DEFAULT_GROUP_NAME.to_string(),
            mode: l.mode,
            created_at: l.created_at,
            layout_version: l.layout_version,
            disks: l.disks,
            bands: l.bands,
            filesystem: l.filesystem,
            expansion: l.expansion,
        }
    }
}

impl ArrayState {
    /// Reject placeholder-shaped identifiers before they are ever persisted.
    ///
    /// A previous implementation hardcoded fake `md_uuid`/`fs_uuid` values instead of
    /// reading real ones from `mdadm --detail --export` / `blkid`. This validation is a
    /// safety net so that if any code path ever regresses to writing a fake identifier,
    /// `StateStore::save()` fails loudly instead of silently persisting a lie to disk.
    ///
    /// `None` is always allowed for either field (it represents "not yet known").
    pub fn validate_no_placeholder_identifiers(&self) -> Result<(), StateError> {
        for band in &self.bands {
            if let Some(v) = &band.md_uuid {
                if is_placeholder_md_uuid(v) {
                    return Err(StateError::PlaceholderIdentifier(format!(
                        "band {} md_uuid looks like a placeholder: {v}",
                        band.index
                    )));
                }
            }
        }

        if let Some(v) = &self.filesystem.fs_uuid {
            if is_placeholder_fs_uuid(v) {
                return Err(StateError::PlaceholderIdentifier(format!(
                    "filesystem fs_uuid looks like a placeholder: {v}"
                )));
            }
        }

        Ok(())
    }
}

/// A `mdadm --detail --export` MD_UUID looks like `12345678:abcdef01:23456789:0abcdef1`
/// (4 groups of 8 hex digits, colon-separated). Two independent checks, found necessary
/// during an earlier review of Phase 4:
///
/// 1. Reject anything that isn't even shaped like an MD_UUID (`""`, `"unknown"`, a
///    truncated read) -- such a value is never valid to persist regardless of content.
/// 2. Reject if 3 or more of the 4 groups are entirely zero, checked *per-group*
///    rather than requiring the zero groups to be in specific positions. The
///    historical bug always zeroed groups 0-2 and left group 3 as `0000000N`, but a
///    position-specific check would miss a future variant that, say, zeroes groups
///    0, 1, and 3 instead. A real mdadm-generated UUID having 3 of 4 groups be
///    literally all-zero has probability on the order of (1/16^8)^3 -- not a value
///    that will ever occur by chance.
fn is_placeholder_md_uuid(v: &str) -> bool {
    let groups: Vec<&str> = v.split(':').collect();
    if groups.len() != 4 || !groups.iter().all(|g| is_hex_group(g, 8)) {
        return true;
    }
    let all_zero_groups = groups.iter().filter(|g| g.chars().all(|c| c == '0')).count();
    all_zero_groups >= 3
}

/// blkid UUIDs are RFC4122-shaped: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
fn is_placeholder_fs_uuid(v: &str) -> bool {
    if !is_uuid_shape(v) {
        return true;
    }
    // The historical bug hardcoded exactly this one literal for every filesystem,
    // regardless of which array it was.
    const LEGACY_LITERAL: &str = "00000000-0000-4000-8000-000000000001";
    if v == LEGACY_LITERAL {
        return true;
    }
    // General "all hex digits are zero" net (a real UUID being all-zero is not a
    // thing that happens).
    v.chars().filter(|c| c.is_ascii_hexdigit()).all(|c| c == '0')
}

fn is_hex_group(g: &str, len: usize) -> bool {
    g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_uuid_shape(v: &str) -> bool {
    v.len() == 36
        && [8, 13, 18, 23].iter().all(|&i| v.as_bytes()[i] == b'-')
        && v.bytes()
            .enumerate()
            .all(|(i, b)| [8, 13, 18, 23].contains(&i) || b.is_ascii_hexdigit())
}
