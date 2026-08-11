use crate::error::OrchestrateError;
use crate::metrics::LiveMetricsSampler;
use crate::notify::NotifyEvent;
use chrono::Utc;
use shr_command::{
    AlwaysRejectConfirmSink, ConfirmRequest, ConfirmSink, Confirmation, NullProgressSink, ProgressSink,
    ProgressUpdate,
};
use shr_core::{
    plan_expansion, plan_initial, DiskId, ExpansionStep, LayoutSnapshot, PlannerInput, RaidLevel,
    RedundancyMode, RedundantBand,
};
use shr_exec::{
    BtrfsExecutor, CapabilityEstimate, CommandRunner, ExecError, LimitScope, LvmExecutor, MdadmExecutor,
    MetricsSampler, NotifyExecutor, PartedExecutor, ReshapeThrottle, SafetyGuard, SyncPriority,
    ThrottleController, ThrottleTick,
};
use shr_inspect::{is_system_mountpoint, parse_mdstat, resolve_disk_path, DiskRef, MdArray, ResolvedDisk};
use shr_state::{
    conf::{is_shr_rs_owned_unit, remove_owned_unit_file, scrub_unit_paths, write_fstab, write_mdadm_conf},
    ArrayState, NotifyPolicy, ScrubOutcome, StateBand, StateCheckpoint, StateDisk, StateExpansion, StateFile,
    StateFilesystem, StatePartition, StatePendingDisk, StateRetiredArray, StateScrubResult, StateStore,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

pub struct CreateRequest {
    /// Identifies this SHR group among the (possibly many) groups this host
    /// manages -- must not collide with any group already recorded in
    /// `state.toml` (multi-group support: the demo target is {SHR, SHR-2} x
    /// {uniform, heterogeneous} coexisting on one host).
    pub name: String,
    pub mode: RedundancyMode,
    /// Disks already resolved to stable identity (by-id `DiskId`, current
    /// kernel name, real size/serial/model) by `shr-inspect`. The engine
    /// never invents identifiers -- see D3.
    pub disks: Vec<ResolvedDisk>,
    pub vg_name: String,
    pub lv_name: String,
    pub mount_point: String,
    pub compression: String,
    pub system_disks: Vec<String>,
}

pub struct ExpandRequest {
    /// Which existing group to expand. `None` is only accepted when
    /// `state.toml` holds EXACTLY one group -- see
    /// `OrchestrationEngine::resolve_group_index`'s doc comment for why that
    /// one case gets an implicit default instead of requiring the caller to
    /// spell out a name that couldn't be ambiguous anyway.
    pub name: Option<String>,
    /// New disks already resolved to stable identity by `shr-inspect`, same
    /// as `CreateRequest::disks` -- see D3.
    pub new_disks: Vec<ResolvedDisk>,
    pub system_disks: Vec<String>,
    /// Bypass the "every band must have a recent, successfully
    /// completed scrub" pre-reshape safety check (the design safety table;
    ///). Defaults to `false` in every constructor this workspace uses
    /// (`Default`-derived structs and test builders alike don't opt in by
    /// accident) -- an operator (or `shr-cli --skip-scrub-check`) must ask
    /// for this explicitly.
    pub skip_scrub_check: bool,
}

/// `OrchestrationEngine::scrub_status`'s result -- aggregated across every
/// band of one group AND its Btrfs filesystem. `error_count` sums mdadm's
/// `mismatch_cnt` (every band) and Btrfs's own error summary; it is NOT
/// necessarily the same as the count just persisted into `last_scrub`,
/// which is per-band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubReport {
    pub group_name: String,
    pub running: bool,
    pub error_count: u64,
}

/// One thing `reconcile()` actually DID against live kernel/LVM/Btrfs state
/// or `state.toml` -- real-guest repro): `shr-rs reconcile` removed a
/// stale, faulty old member (`loop12p1`) from a live array and rewrote
/// `state.toml`, then printed only `Reconcile: nothing pending.` The prior
/// CLI report was built purely from the RETURNED post-state's
/// `resize_pending` flags, which (a) says nothing about a member removal at
/// all, and (b) reads identically whether nothing was pending or a pending
/// resize just got completed by THIS call. Every variant here corresponds
/// to a fact `reconcile()` (or a helper it calls) directly observed and
/// acted on -- never a inferred/approximate description of what MIGHT have
/// happened, matching this project's "no invented data" rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Self-heal: this band's `scrub_in_progress` flag was cleared
    /// because live `sync_action`/`btrfs scrub status` showed the scrub had
    /// actually finished -- the scheduled-scrub systemd timer only ever
    /// calls `fs scrub start`, never `fs scrub status`, so nothing else
    /// observes this on its own.
    ScrubSelfHealed {
        group: String,
        band_index: u8,
        md_name: String,
        error_count: u64,
    },
    /// An earlier review: a deferred `pvresize`/`lvextend`/`btrfs resize max`
    /// left behind by `execute_grow` was just run to completion because the
    /// reshape it was waiting on has gone back to `idle`.
    ResizeCompleted {
        group: String,
        band_index: u8,
        md_name: String,
    },
    /// Self-heal: an old `disk replace`d member's deferred
    /// `mdadm --remove` was just issued and re-verified against
    /// `/proc/mdstat` (membership, not `readlink`) because its copy has
    /// finished and the kernel confirmed it as attached-but-faulty.
    MemberRemoved {
        group: String,
        band_index: u8,
        md_name: String,
        member_path: String,
    },
    /// The host-wide `/proc/sys/dev/raid/speed_limit_max` this project had
    /// overwritten (for a reshape's throttle, or for `fs scrub start
    /// --priority`) was put back to `speed_kb`, the value observed before
    /// the first of those writes, because no md array on the host is running
    /// anything anymore. Host-wide, so deliberately NOT reported per group
    /// or per band -- see `StateFile::saved_speed_limit_max_kb`.
    SpeedLimitRestored { speed_kb: u64 },
}

/// `reconcile()`'s result: the array's state AFTER reconciling, plus
/// exactly what it did to arrive there (`performed` is empty when -- and
/// only when -- this call genuinely found nothing to do). See
/// [`ReconcileAction`] for why a caller cannot safely reconstruct this from
/// `state` alone.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileOutcome {
    pub state: StateFile,
    pub performed: Vec<ReconcileAction>,
}

/// One destructive step's undo, recorded as it succeeds so a later failure
/// can unwind everything already done, in reverse order (D10).
enum UndoAction {
    RemovePartition {
        disk_path: String,
        part_num: u32,
    },
    /// Stopping the array and zeroing its members' superblocks are always
    /// done together, in that order (a member can't be zeroed while still
    /// an active array component) -- bundled into one action so the
    /// journal's push/reverse order doesn't have to encode that ordering
    /// constraint between two separate entries.
    TeardownArray {
        md_name: String,
        member_paths: Vec<String>,
    },
    /// Detach a spare added via `mdadm --add` before a `--grow` consumed it
    /// (D1/D10 expansion). Never valid once the corresponding `grow`
    /// succeeds -- see `MdadmExecutor::remove_member`'s doc comment.
    RemoveSpareMember {
        md_name: String,
        member_path: String,
    },
    RemovePv {
        dev_path: String,
    },
    RemoveVg {
        vg_name: String,
    },
    RemoveLv {
        lv_path: String,
    },
    Unmount {
        mount_point: String,
    },
}

/// The engine's default when a caller doesn't wire up a real progress sink
/// (`OrchestrationEngine::new`, before any `.with_progress_sink` call) --
/// preserves the pre-Stage-0 behavior of reporting nothing.
static DEFAULT_PROGRESS_SINK: NullProgressSink = NullProgressSink;

/// The engine's default when a caller doesn't wire up a real confirm sink --
/// **fail-closed**, not fail-open. Stage 0 first shipped this as
/// `AlwaysConfirmSink` (silently auto-approve), which reproduced "no
/// confirmation ever happens" for every pre-Stage-0 call site for free but
/// meant a NEW, unattended caller that simply forgot to wire a real sink
/// would silently skip confirmation on a destructive operation -- exactly
/// the class of defect this project has repeatedly had to catch (D1 and
/// friends). Flipped to `AlwaysRejectConfirmSink` post-Stage-0: any caller
/// that wants auto-approve now has to say so explicitly via
/// `.with_confirm_sink(&AlwaysConfirmSink)`, the same way `shr-cli` does
/// (see its call site's comment for why that's a safe, deliberate choice
/// there and not a reintroduction of the fail-open default).
static DEFAULT_CONFIRM_SINK: AlwaysRejectConfirmSink = AlwaysRejectConfirmSink;

pub struct OrchestrationEngine<'a> {
    runner: &'a dyn CommandRunner,
    store: Arc<StateStore>,
    mdadm_conf_path: PathBuf,
    fstab_path: PathBuf,
    /// Where `destroy()` looks for (and removes) the group's own
    /// `shr-rs-scrub-<group>` unit pair -- same "tempdir by default in
    /// tests, real `/etc/systemd/system` in production via
    /// `with_unit_dir`" shape as `mdadm_conf_path`/`fstab_path`.
    unit_dir: PathBuf,
    progress: &'a dyn ProgressSink,
    confirm: &'a dyn ConfirmSink,
    priority: SyncPriority,
    /// `None` (the default) means "read live-status checks (degraded,
    /// background activity, scrub-running) through `self.runner`, same as
    /// everything else" -- correct for every real (`SystemRunner`) caller
    /// and every test, which inject one runner and expect it to answer
    /// every read. `Some` exists ONLY for `preview_expand`'s use:
    /// that preview runs the WHOLE `expand()` (mutating commands included)
    /// against a `DryRunRunner`, whose `is_dry_run()` shortcut makes
    /// `MdadmExecutor::sync_action`/`degraded_count` fabricate a fixed
    /// "idle"/"0 degraded" answer instead of ever reading anything real --
    /// so a scrub genuinely running on the real array was invisible to the
    /// preview, which fell through to a stale, misleading validation error
    /// (the "no scrub completed yet") before the real, non-preview
    /// `expand()` call downstream -- which DOES see the real state -- ever
    /// got a chance to run. Pointing ONLY these status reads at a real
    /// `SystemRunner` while the mutating half of the preview stays on
    /// `DryRunRunner` (so it still never touches the real array) fixes
    /// this without weakening dry-run's "never execute anything real"
    /// guarantee.
    status_runner: Option<&'a dyn CommandRunner>,
    /// Which notification channels `notify()`/`check_health()` fire
    /// through. Defaults to `NotifyPolicy::default()` (webhook off until an
    /// operator configures a URL; `systemd_notify` ON) -- see that type's
    /// doc comment for why the free, local channel defaults on (an
    /// opt-in-only notification channel tends to stay silently
    /// off forever).
    notify_policy: NotifyPolicy,
    /// `None` (the default) means "use a real, live `LiveMetricsSampler`
    /// built per-band from `self.runner` and that band's own member disks"
    /// -- `start_sync_throttle`/`tick_active_sync` construct
    /// one on demand, since a single fixed sampler can't know which disks a
    /// given band actually has. `Some` is an explicit override (tests
    /// injecting a fixed danger/idle signal; a future caller with its own
    /// reason to bypass live sampling) that replaces the live sampler
    /// entirely for every band.
    metrics_sampler: Option<&'a dyn MetricsSampler>,
}

impl<'a> OrchestrationEngine<'a> {
    /// Defaults `mdadm.conf`/`fstab` targets to sit next to `store`'s own
    /// path rather than hardcoding `/etc/...` here: this way every test in
    /// this workspace that builds a `StateStore` over a tempdir (there are
    /// dozens) gets fully sandboxed config-file writes for free, with no
    /// risk of a forgotten call site actually touching a real machine's
    /// `/etc/mdadm.conf` during `cargo test`. Production code (`shr-cli`)
    /// MUST call `with_conf_paths` to point at the real `/etc` locations --
    /// see D8.
    pub fn new(runner: &'a dyn CommandRunner, store: Arc<StateStore>) -> Self {
        let conf_dir = store.path().parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let mdadm_conf_path = conf_dir.join("mdadm.conf");
        let fstab_path = conf_dir.join("fstab");
        let unit_dir = conf_dir;
        Self {
            runner,
            store,
            mdadm_conf_path,
            fstab_path,
            unit_dir,
            progress: &DEFAULT_PROGRESS_SINK,
            confirm: &DEFAULT_CONFIRM_SINK,
            priority: SyncPriority::Balanced,
            metrics_sampler: None,
            status_runner: None,
            notify_policy: NotifyPolicy::default(),
        }
    }

    /// Read-only accessor for tests/wiring checks: lets a caller
    /// prove where this engine will actually write `mdadm.conf` without
    /// needing to trigger a real write.
    pub fn mdadm_conf_path(&self) -> &std::path::Path {
        &self.mdadm_conf_path
    }

    /// Read-only accessor for tests/wiring checks: lets a caller
    /// prove where this engine will actually write `fstab` without needing
    /// to trigger a real write.
    pub fn fstab_path(&self) -> &std::path::Path {
        &self.fstab_path
    }

    /// Override where `write_managed_configs` writes -- production code
    /// points this at the real `/etc/mdadm.conf` / `/etc/fstab` (D8); tests
    /// should generally leave the `new()` default (a tempdir path) alone.
    pub fn with_conf_paths(mut self, mdadm_conf: impl Into<PathBuf>, fstab: impl Into<PathBuf>) -> Self {
        self.mdadm_conf_path = mdadm_conf.into();
        self.fstab_path = fstab.into();
        self
    }

    /// Read-only accessor mirroring `mdadm_conf_path`/`fstab_path`:
    /// lets a caller prove where `destroy()` will look for the group's
    /// scrub unit pair without needing a real deletion.
    pub fn unit_dir(&self) -> &std::path::Path {
        &self.unit_dir
    }

    /// Override where `destroy()` looks for the group's own scrub unit
    /// pair to clean up -- production code points this at the real
    /// `/etc/systemd/system` (matching `schedule install`'s own hardcoded
    /// path); tests should generally leave the `new()` default (a tempdir
    /// path) alone.
    pub fn with_unit_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.unit_dir = dir.into();
        self
    }

    /// Wire in a real progress sink (CLI text output, Cockpit's JSON
    /// stream, or a test double). Defaults to a no-op (`NullProgressSink`)
    /// -- a caller that doesn't opt in gets today's behavior: no progress
    /// output at all (Stage 0 DoD).
    pub fn with_progress_sink(mut self, sink: &'a dyn ProgressSink) -> Self {
        self.progress = sink;
        self
    }

    /// Wire in a real confirm sink. Defaults to `AlwaysRejectConfirmSink`
    /// (fail-closed) -- see `DEFAULT_CONFIRM_SINK`'s doc comment. A caller
    /// that has already obtained real, explicit approval before reaching
    /// this call (an interactive CLI operator, a TUI wizard's own confirm
    /// screen, a browser UI's confirm dialog whose result gated the spawn
    /// that runs this code at all) should pass `AlwaysConfirmSink` to say so
    /// -- never rely on the default to mean "yes".
    pub fn with_confirm_sink(mut self, sink: &'a dyn ConfirmSink) -> Self {
        self.confirm = sink;
        self
    }

    /// Point live-status VALIDATION reads (degraded/background-activity/
    /// scrub-running checks) at a different runner than the one that
    /// executes/records this call's own commands -- see `status_runner`'s
    /// doc comment. `preview_expand` is the only production caller:
    /// it runs the whole `expand()` against a `DryRunRunner` (so nothing
    /// real gets mutated) but needs THESE specific reads answered by the
    /// real system so a genuinely running scrub is still visible during the
    /// preview.
    pub fn with_status_runner(mut self, runner: &'a dyn CommandRunner) -> Self {
        self.status_runner = Some(runner);
        self
    }

    /// The runner live-status validation reads should use -- `status_runner`
    /// if one was wired in, otherwise the same runner as everything else
    /// (today's behavior, unchanged for every caller that doesn't opt in).
    fn status_runner(&self) -> &'a dyn CommandRunner {
        self.status_runner.unwrap_or(self.runner)
    }

    /// Wire in the notification policy (webhook URL / `systemd_notify`
    /// on-off), normally loaded from `PolicyStore` (`shr-state::policy`) --
    /// see production call sites in `shr-cli` for where this actually
    /// happens. Defaults to `NotifyPolicy::default()` if never called.
    pub fn with_notify_policy(mut self, policy: NotifyPolicy) -> Self {
        self.notify_policy = policy;
        self
    }

    /// Fire `event` through every channel `self.notify_policy` enables,
    /// discarding (never propagating) delivery failures -- the explicit
    /// requirement: a dead webhook or a `systemd-notify` call outside a
    /// supervised process must NEVER make the underlying scrub/reconcile/
    /// health-check look like it failed. There is nothing more useful a
    /// caller could do with a delivery error here than what this already
    /// does (best-effort, every enabled channel, move on) -- see
    /// `NotifyExecutor`'s doc comment for why the error is still a real
    /// `Result` one layer down instead of swallowed there.
    ///
    /// `systemd-notify --status=...` alone does NOT reach the
    /// operator for any unit this project generates -- see
    /// `NotifyExecutor::systemd_notify`'s doc comment for the measured
    /// reason (no `$NOTIFY_SOCKET` for a plain `Type=oneshot`; even with
    /// `NotifyAccess=all`, systemd clears `StatusText` once a oneshot
    /// exits). This `tracing::warn!` is the channel that actually lands in
    /// `journalctl -u <unit>`: every generated unit runs a subcommand, so
    /// `shr-bin::detect_ui_mode` always resolves it to `UiMode::Cli`, whose
    /// default `EnvFilter` fallback passes WARN+ with no `RUST_LOG` set
    /// (the "quiet by default" only suppressed the per-command INFO
    /// trace). A later fix changed the OTHER branch: `UiMode::Tui`'s fallback is
    /// `"off"` instead (a WARN mid-session corrupts ratatui's alternate
    /// screen -- see `shr-bin::init_tracing`'s doc comment), but no unit
    /// this project generates ever runs in `Tui` mode, so that split does
    /// not change what reaches the journal. Every generated unit's stderr
    /// already goes to the journal under systemd's own default (no
    /// `StandardError=` needed in the unit file). Gated on the SAME
    /// `systemd_notify` flag as the subprocess call above -- this is that
    /// flag's actual local-delivery mechanism now, not a second channel an
    /// operator would need a separate toggle to silence.
    fn notify(&self, event: &NotifyEvent) {
        let notifier = NotifyExecutor::new(self.runner);
        if let Some(url) = &self.notify_policy.webhook_url {
            let _ = notifier.webhook(url, &event.to_json());
        }
        if self.notify_policy.systemd_notify {
            let _ = notifier.systemd_notify(&event.status_line());
            tracing::warn!(target: "shr_rs::notify", "{}", event.status_line());
        }
    }

    /// Select the speed profile (`--priority`) new md syncs started by this
    /// engine use, and the fallback for a band whose own profile isn't
    /// recorded. Defaults to `Balanced`.
    pub fn with_priority(mut self, priority: SyncPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Override adaptive reshape throttling's signal source, bypassing
    /// the engine's own default `LiveMetricsSampler` entirely for
    /// EVERY band -- see `metrics_sampler`'s doc comment. Tests use this to
    /// inject a fixed danger/idle signal without a real kernel to sample
    /// from.
    pub fn with_metrics_sampler(mut self, sampler: &'a dyn MetricsSampler) -> Self {
        self.metrics_sampler = Some(sampler);
        self
    }

    /// Refresh `mdadm.conf`/`fstab` from the current, real state (D8) so the
    /// OS's own boot machinery can reassemble and mount EVERY group with no
    /// shr-rs process running at boot. Regenerates each file's managed block
    /// from scratch every call, from the ENTIRE `StateFile` -- every band of
    /// every group for `mdadm.conf`, every group's filesystem for `fstab` --
    /// never from just the one group a create/expand call happened to
    /// touch. Multi-group correctness trap: if this only ever saw the
    /// group just modified, creating group B would regenerate the managed
    /// block from group B alone, silently deleting group A's `ARRAY` line
    /// and fstab mount -- after a reboot, group A simply wouldn't come
    /// back, with nothing here to say why. Called alongside every non-dry-
    /// run `StateStore::save` that reflects a real physical change,
    /// mirroring an earlier review finding's rule that a real change must be
    /// recorded as soon as it happens, not batched for later.
    fn write_managed_configs(&self, state: &StateFile) -> Result<(), OrchestrateError> {
        write_mdadm_conf(&self.mdadm_conf_path, state)?;
        write_fstab(&self.fstab_path, state)?;
        Ok(())
    }

    /// Re-verify each target disk's core invariants immediately before
    /// crossing into destructive territory, closing the TOCTOU window
    /// between an earlier, separate preflight check (`shr_command::
    /// preflight_create`, which can run an arbitrary amount of time before
    /// this call's own `ConfirmSink` gate returns -- an operator reading a
    /// confirmation screen, a queued Cockpit request) and the first
    /// destructive command this call actually issues. Only ever called from
    /// inside an `if !self.runner.is_dry_run()` block, same as every
    /// `ConfirmSink` gate right above each call site -- a preview touches
    /// nothing real, so there is nothing here for it to re-verify against,
    /// and gating this the same way keeps a bare `preview_create`/
    /// `preview_expand` (no real system to read) from ever reaching it.
    ///
    /// Deliberately does NOT repeat `SafetyGuard::validate_disk_target`
    /// against `system_disks` -- that list is the EXACT SAME value already
    /// checked earlier in this same call, so repeating it proves nothing
    /// new. What actually changes between preflight and now is the real,
    /// live system; this reads THAT directly, through `self.status_runner()`
    /// (same reasoning as every other live-status read in this
    /// engine).
    fn reverify_targets(&self, disks: &[ResolvedDisk]) -> Result<(), OrchestrateError> {
        let runner = self.status_runner();
        for d in disks {
            let by_id_path = resolve_disk_path(&d.id).display().to_string();

            // Still exists: `test -e` on the STABLE by-id path, not the
            // kernel name -- the kernel can reassign a kernel device name to
            // a completely different physical disk if the original one was
            // unplugged and another inserted, which a kernel-name-only
            // check could not tell apart from "still the same disk".
            match runner.run("test", &["-e", &by_id_path]) {
                Ok(_) => {}
                Err(ExecError::NonZeroExit { .. }) => {
                    return Err(OrchestrateError::Validation(format!(
                        "disk `{}` no longer exists at `{by_id_path}` (it did at preflight \
                         time); aborting before touching anything -- re-run preflight",
                        d.id.as_str()
                    )));
                }
                Err(e) => return Err(e.into()),
            }

            // Still not a system disk: ask the LIVE system, not the
            // (necessarily stale-by-now) `system_disks` list already
            // checked earlier in this same call.
            if let Some(mp) = live_system_mountpoint_on(runner, &d.kernel_name)? {
                return Err(OrchestrateError::Validation(format!(
                    "disk `{}` (`{}`) now holds the system mountpoint `{mp}`, which it did not \
                     at preflight time; aborting before touching anything",
                    d.id.as_str(),
                    d.kernel_name
                )));
            }

            // has_content unchanged: only worth checking when preflight saw
            // NONE -- a disk already accepted with `--force-content` has
            // already had its content risk accepted; more of it appearing
            // is not new information this check can act on.
            if !d.has_content {
                let probe = runner.run(
                    "lsblk",
                    &[
                        "--noheadings",
                        "-b",
                        "-o",
                        "PTTYPE,FSTYPE",
                        &format!("/dev/{}", d.kernel_name),
                    ],
                )?;
                if probe.stdout.split_whitespace().next().is_some() {
                    return Err(OrchestrateError::Validation(format!(
                        // No frontend-specific affordance named here.
                        // This message reaches the CLI, the TUI, and Cockpit
                        // alike, and each has its own override control
                        // (`--force-content`, the `o` key, a checkbox) --
                        // naming one of them is wrong in the other two.
                        "disk `{}` (`{}`) now carries a partition table or filesystem signature \
                         it did not have at preflight time; aborting before touching anything -- \
                         re-run preflight, and accept the existing content explicitly, if this \
                         is expected",
                        d.id.as_str(),
                        d.kernel_name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Resolve `md_name`'s `--backup-file` path and clear a STALE one
    /// left behind by a previous attempt on this exact band, so a retried
    /// (post-crash) or later `--grow` never fails outright because mdadm
    /// refuses to reuse an existing backup file.
    ///
    /// Already unique PER BAND (`allocate_md_name` guarantees `md_name` is
    /// unique host-wide, across every group) -- this is never about two
    /// DIFFERENT reshapes colliding, only a leftover file from a PREVIOUS
    /// attempt on this exact band. `/var/lib/shr-rs` (not `/tmp`): this
    /// directory already survives a reboot (it's where `state.toml` itself
    /// lives), which a crash-recovery file needs.
    ///
    /// Only removes the file when this band's `sync_action` reads back
    /// `idle` (through `self.status_runner()`, same reasoning as
    /// every other live-status read here: a preview must see the REAL
    /// answer, not `DryRunRunner`'s fabricated "idle"). `idle` means no
    /// reshape is currently consuming this file, so whatever produced it
    /// has already finished or was abandoned. If a reshape genuinely IS
    /// running (`sync_action == "reshape"`) -- this project's only
    /// grow-in-progress signal for a band -- this refuses to touch the file
    /// at all: deleting a LIVE reshape's crash-recovery data would be a
    /// correctness risk, not cleanup. (In practice this engine never calls
    /// `--grow` again on a band already mid-reshape -- `expand()`'s own
    /// background-activity guard blocks that earlier -- so this branch is
    /// defense in depth, not the expected path.)
    ///
    /// Directory creation and the removal itself go through `self.runner`
    /// (`mkdir -p` / `rm -f`), never raw `std::fs` -- unmockable, and an
    /// unconditional IO error on the Windows dev host `cargo test` runs on.
    fn prepare_backup_file(&self, md_name: &str) -> Result<String, OrchestrateError> {
        let dir = "/var/lib/shr-rs";
        self.runner.run("mkdir", &["-p", dir])?;
        let backup_file = format!("{dir}/backup-{md_name}.bak");

        let status_runner = self.status_runner();
        // Same "don't fabricate a positive answer" rule `MdadmExecutor::
        // sync_action`/`degraded_count` already apply to THEIR own
        // dry-run shortcut: a bare preview with no real system to read
        // (no `with_status_runner` override) must not claim a stale
        // backup file exists just because `DryRunRunner::run` blindly
        // returns success for anything -- that would show a `rm -f`
        // command in the preview that will almost never actually run for
        // real (the "don't show a command that isn't really going to
        // execute" principle, applied here instead of to PARTUUID).
        if status_runner.is_dry_run() {
            return Ok(backup_file);
        }
        match status_runner.run("test", &["-e", &backup_file]) {
            Ok(_) => {
                if MdadmExecutor::new(status_runner).sync_action(md_name)? == "reshape" {
                    return Err(OrchestrateError::Validation(format!(
                        "backup file `{backup_file}` already exists AND {md_name} is currently \
                         reshaping; refusing to touch another in-progress reshape's \
                         crash-recovery data -- wait for it to finish"
                    )));
                }
                self.runner.run("rm", &["-f", &backup_file])?;
            }
            Err(ExecError::NonZeroExit { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(backup_file)
    }

    /// Orchestrate initial array creation
    pub fn create(&self, req: CreateRequest) -> Result<ArrayState, OrchestrateError> {
        // 0. Load every group this host already manages -- an empty
        // `StateFile` if `state.toml` doesn't exist yet (this is the very
        // first group ever created here). Needed up front for the next two
        // checks (multi-group support): a new group's name must not
        // collide with an existing one, and none of its disks may already
        // belong to a DIFFERENT group -- both would otherwise silently
        // corrupt the previously-recorded group's data or make it
        // ambiguous which group a name/disk refers to.
        let mut full_state = self.store.load()?.unwrap_or_else(|| StateFile::new(Vec::new()));
        if full_state.groups.iter().any(|g| g.name == req.name) {
            return Err(OrchestrateError::Validation(format!(
                "a group named `{}` already exists",
                req.name
            )));
        }
        let disks_in_other_groups: HashSet<&str> = full_state
            .groups
            .iter()
            .flat_map(|g| g.disks.iter())
            .map(|d| d.id.as_str())
            .collect();
        for d in &req.disks {
            if disks_in_other_groups.contains(d.id.as_str()) {
                return Err(OrchestrateError::Validation(format!(
                    "disk `{}` already belongs to an existing group; a disk can only be a \
                     member of one group at a time",
                    d.id.as_str()
                )));
            }
        }

        // 1. Reject duplicate disk identity up front (D3): the planner and
        // state schema both assume every disk has a unique stable id.
        let mut seen_ids = HashSet::new();
        for d in &req.disks {
            if !seen_ids.insert(d.id.as_str()) {
                return Err(OrchestrateError::Validation(format!(
                    "duplicate disk id `{}` in create request",
                    d.id.as_str()
                )));
            }
        }

        // 2. Safety validation on all target disks. Check both spellings a
        // disk can be known by: the kernel path used here for validation is
        // NOT necessarily what destructive commands are actually issued
        // against below (that's the stable by-id path, so a stale/mismatched
        // by-id symlink can't silently target a different device than the
        // one just validated).
        for d in &req.disks {
            let kernel_path = format!("/dev/{}", d.kernel_name);
            SafetyGuard::validate_disk_target(&kernel_path, &req.system_disks)?;
            let by_id_path = resolve_disk_path(&d.id).display().to_string();
            SafetyGuard::validate_disk_target(&by_id_path, &req.system_disks)?;
        }

        let parted = PartedExecutor::new(self.runner);
        let mdadm = MdadmExecutor::new(self.runner);
        let lvm = LvmExecutor::new(self.runner);
        let btrfs = BtrfsExecutor::new(self.runner);

        // 3. Prerequisite checks (D11): verify every tool this workflow
        // needs is actually available BEFORE any destructive step. This
        // used to happen last (Btrfs support was only checked at mkfs
        // time, in step 6, by which point partitions and mdadm arrays
        // already existed) -- moved here so a missing tool is caught
        // before anything is touched, not after the most expensive work is
        // already done.
        parted.ensure_supported()?;
        mdadm.ensure_supported()?;
        lvm.ensure_supported()?;
        btrfs.ensure_supported()?;

        // 3.5. Cockpit real-browser finding, backend half): reject a
        // colliding LVM name now, before ANY destructive step runs.
        // `vgcreate` itself doesn't execute until deep inside the
        // destructive sequence below (LVM setup, step 5's "lvm" stage --
        // by which point partitions and mdadm arrays already exist), so
        // letting it be the first thing to notice a duplicate name turns
        // an ordinary validation error into a partial-apply-then-rollback.
        // Reads LIVE LVM state (`vgs`/`lvs` via `self.runner`), never
        // `state.toml`: a VG/LV can exist on the host without shr-rs
        // knowing about it at all (hand-created, another tool, an older
        // install) -- exactly the case the step-0 group-name uniqueness
        // check (a `state.toml`-only check, and a different namespace
        // entirely) cannot catch. Cockpit now passes `--vg-name
        // vg_<group>`/`--lv-name data` explicitly so most collisions never
        // reach here; this is the backstop for everything else (a manual
        // `--vg-name` typo, a VG created outside shr-rs entirely).
        if lvm.vg_exists(&req.vg_name)? {
            return Err(OrchestrateError::Validation(format!(
                "LVM volume group `{}` already exists on this host; choose a different \
                 --vg-name (or remove/rename the existing VG first)",
                req.vg_name
            )));
        }
        if lvm.lv_exists(&req.vg_name, &req.lv_name)? {
            return Err(OrchestrateError::Validation(format!(
                "logical volume `{}/{}` already exists on this host; choose a different \
                 --lv-name",
                req.vg_name, req.lv_name
            )));
        }

        // 4. Prepare domain inputs for shr-core planner using real identity
        // (by-id, real size/serial/model) resolved by shr-inspect -- never
        // engine-invented placeholders.
        let core_disks: Vec<shr_core::Disk> = req.disks.iter().map(ResolvedDisk::to_planner_disk).collect();

        let plan_input = PlannerInput::new(core_disks, req.mode);
        let reserved_head = plan_input.reserved_head;
        let initial_plan = plan_initial(&plan_input).map_err(|e| OrchestrateError::Planner(e.to_string()))?;

        // An earlier review finding: a disk small enough that reserved_head +
        // reserved_tail + band_alignment rounding leaves it with 0 usable
        // bytes never appears in ANY band's membership -- not even as an
        // "unusable tail" planner warning, which only fires for a disk that
        // participates in at least one candidate slice. Left unchecked, the
        // engine would still wipe such a disk's GPT below and record it in
        // state.toml with an empty partition list: destructive for zero
        // benefit, and state.toml would silently misrepresent it as part of
        // the array. Fail before touching anything.
        for d in &req.disks {
            let is_member_of_any_band = initial_plan.bands.iter().any(|b| b.members().contains(&d.id));
            if !is_member_of_any_band {
                return Err(OrchestrateError::Validation(format!(
                    "disk `{}` has no usable capacity in this layout (too small \
                     relative to reserved space/alignment or other disks); remove \
                     it from the request instead of wiping it for no benefit",
                    d.id.as_str()
                )));
            }
        }

        // 4.5. Confirm before crossing into destructive territory.
        // Skipped entirely under dry-run: a dry-run touches nothing
        // real (same rule `rollback`/`host_md_numbers` already follow), so
        // asking "are you sure" about a simulation is meaningless -- and if
        // it weren't skipped, a caller that wires `AlwaysRejectConfirmSink`
        // into its real runs would find `--dry-run` failing for a reason
        // that has nothing to do with dry-run itself. `ConfirmSink` only
        // ever gates REAL destructive execution.
        if !self.runner.is_dry_run() {
            let disk_list: Vec<&str> = req.disks.iter().map(|d| d.id.as_str()).collect();
            let decision = self.confirm.confirm(&ConfirmRequest {
                operation: "create".to_string(),
                summary: format!(
                    "create SHR group `{}` ({} disk(s): {}); this will partition and format them",
                    req.name,
                    req.disks.len(),
                    disk_list.join(", ")
                ),
                irreversible: true,
            });
            if decision == Confirmation::Reject {
                return Err(OrchestrateError::Rejected(format!(
                    "create of group `{}` was rejected via ConfirmSink before touching any disk",
                    req.name
                )));
            }
            // The confirm dialog above is exactly the TOCTOU window --
            // an operator can sit on it for an arbitrary amount of time.
            // Re-check reality right before crossing into destructive
            // territory, not just at preflight time.
            self.reverify_targets(&req.disks)?;
        }

        // 5. Everything from here on is destructive. D10: record an undo
        // action every time a step succeeds, so that if ANY later step
        // fails, everything already done can be unwound in reverse order --
        // otherwise a mid-pipeline failure leaves partitions/md
        // superblocks/PVs/VG/LV behind that corrupt the next attempt. The
        // whole sequence runs as one closure purely so `?` can be used
        // freely inside it while still reaching the rollback logic below on
        // failure (a bare `?` in `create()` itself would return before
        // rollback ever ran).
        let mut journal: Vec<UndoAction> = Vec::new();
        // Seeded from every group ALREADY recorded in state.toml (not just
        // this new group's own bands, which don't exist yet) so a brand new
        // group's band0 can never be allocated the same `/dev/mdN` as some
        // other, already-existing group's band0 -- see `allocate_md_name`'s
        // doc comment. ALSO seeded from whatever `/dev/mdN` numbers the HOST
        // itself already has, whether or not shr-rs put them there -- see
        // `host_md_numbers`'s doc comment for why state.toml alone is not
        // enough.
        let mut used_md = used_md_numbers(&full_state);
        used_md.extend(host_md_numbers(self.runner)?);
        // Every kernel device name this request's OWN new member
        // partitions will resolve to -- populated while partitioning inside
        // the closure below, then consulted both there (to tell "an array
        // made purely of OUR OWN new partitions" apart from "an array that
        // also spans a disk this request never confirmed anything about",
        // see `stop_any_foreign_holder_before_create`'s doc comment) AND
        // after the closure returns, by `rollback` (the `RemovePartition`
        // handling applies the SAME safety rule -- see its call below for
        // why: refusing to stop a foreign-spanning array in the forward
        // path but then unconditionally stopping that exact same array
        // while rolling back the very same failed `create()` would be a
        // direct, in-the-same-operation contradiction of the rule just
        // enforced a moment earlier). Declared here, outside the closure,
        // specifically so it survives past the closure's return either way.
        let mut target_kernel_names: HashSet<String> = HashSet::new();
        let outcome: Result<ArrayState, OrchestrateError> = (|| {
            let mut state_disks = Vec::new();
            let mut created_md_devices = Vec::new();
            let mut state_bands = Vec::new();
            let mut disk_paths: HashMap<String, String> = HashMap::new();

            self.progress.report(ProgressUpdate {
                operation: "create".to_string(),
                stage: "partition".to_string(),
                percent: Some(0.0),
                message: format!("partitioning {} disk(s)", req.disks.len()),
            });

            // Partition disks & create GPT labels -- operate on the stable
            // by-id path, per the project's identity policy (the design
            //), not the unstable /dev/sdX enumeration.
            //
            // D2: a disk only gets a partition for the bands it's actually
            // a redundant member of, never every band unconditionally -- in
            // a heterogeneous configuration, smaller disks are members of
            // fewer bands than larger ones (the design). `band.offset()`
            // is already relative to the start of each disk's usable region
            // (the planner's shared address space starting right after
            // `reserved_head`, common to every disk regardless of which
            // bands it participates in), so `reserved_head + band.offset()`
            // is the correct absolute start on every member disk -- there
            // is no separate per-disk running offset to track (D9: this
            // also stops duplicating the planner's own reserved_head as a
            // second literal that could silently drift from it).
            for d in &req.disks {
                let disk_path = resolve_disk_path(&d.id).display().to_string();
                disk_paths.insert(d.id.as_str().to_string(), disk_path.clone());
                parted.create_gpt(&disk_path)?;

                let mut partitions = Vec::new();

                for band in &initial_plan.bands {
                    if !band.members().contains(&d.id) {
                        continue;
                    }

                    let start_offset = reserved_head + band.offset();
                    let end_offset = start_offset + band.size();
                    // `end_offset` is the exclusive end of this band's range
                    // (as shr-core's planner defines it: size = end - start).
                    // parted treats a `mkpart ... STARTB ENDB` end position
                    // as the LAST byte included in the partition, rounded to
                    // the containing sector -- found running Step 3's
                    // real-VM smoke test, where two bands with touching
                    // boundaries (band N's end exactly equals band N+1's
                    // start) caused parted to refuse the second mkpart with
                    // "closest location we can manage" one sector later,
                    // because the first partition had already consumed the
                    // shared boundary sector. Subtracting one byte makes the
                    // requested end land in the sector immediately before
                    // the next band's start, matching the exclusive-range
                    // math everywhere else in this function.
                    parted.add_partition(&disk_path, start_offset, end_offset - 1)?;
                    let part_num = (band.band_index() + 1) as u32;
                    journal.push(UndoAction::RemovePartition {
                        disk_path: disk_path.clone(),
                        part_num,
                    });
                    parted.set_raid_flag(&disk_path, part_num)?;

                    let part_path = parted.partition_path_for_read(&disk_path, part_num);
                    target_kernel_names
                        .insert(part_path.rsplit('/').next().unwrap_or(&part_path).to_string());
                    let part_uuid = parted.read_partuuid(&part_path)?;

                    partitions.push(StatePartition {
                        part_uuid,
                        offset_bytes: start_offset,
                        size_bytes: band.size(),
                        band_index: band.band_index(),
                    });
                }

                state_disks.push(StateDisk {
                    id: d.id.as_str().to_string(),
                    size_bytes: d.size_bytes,
                    serial: (!d.serial.is_empty()).then(|| d.serial.clone()),
                    model: (!d.model.is_empty()).then(|| d.model.clone()),
                    added_at: Utc::now().to_rfc3339(),
                    partitions,
                });
            }

            // by-partuuid symlinks (used as mdadm member paths just below)
            // are udev-populated, asynchronously relative to the partition
            // table writes just issued above. Wait for udev to catch up
            // before constructing paths that depend on them.
            parted.settle_udev()?;

            self.progress.report(ProgressUpdate {
                operation: "create".to_string(),
                stage: "array".to_string(),
                percent: Some(0.0),
                message: format!("creating {} RAID array(s)", initial_plan.bands.len()),
            });

            // Create mdadm arrays for each band. Matched to partitions by
            // `band.band_index()` directly (an earlier review finding) rather
            // than `enumerate()`'s index -- currently always equal, since
            // `plan_initial` assigns band_index in push order, but that's a
            // planner-internal detail this loop shouldn't have to rely on.
            for band in &initial_plan.bands {
                let md_name = allocate_md_name(&mut used_md);
                let mut member_part_paths = Vec::new();
                let mut member_part_uuids = Vec::new();
                // (disk_path, part_num) per member -- needed to
                // check each member for a live holder array via the same
                // `PartedExecutor::partition_path_for_read` resolution
                // `mdadm --create`'s by-partuuid member paths are built
                // from, just below.
                let mut member_specs: Vec<(String, u32)> = Vec::new();

                for disk in &state_disks {
                    if let Some(part) = disk.partitions.iter().find(|p| p.band_index == band.band_index()) {
                        member_part_uuids.push(part.part_uuid.clone());
                        member_part_paths.push(format!("/dev/disk/by-partuuid/{}", part.part_uuid));
                        if let Some(disk_path) = disk_paths.get(&disk.id) {
                            member_specs.push((disk_path.clone(), (part.band_index + 1) as u32));
                        }
                    }
                }

                let member_refs: Vec<&str> = member_part_paths.iter().map(AsRef::as_ref).collect();
                let level_str = match band.level() {
                    RaidLevel::Raid1 => "raid1",
                    RaidLevel::Raid5 => "raid5",
                    RaidLevel::Raid6 => "raid6",
                };

                // Corrected (see `stop_any_foreign_holder_before_create`'s
                // doc comment for why the first version -- zero first, no
                // stop -- didn't work on real hardware): `destroy` without
                // `--zero-superblocks` (the default) leaves a stale mdadm
                // superblock on a member PARTITION; a later `create()` that
                // re-partitions the SAME disk at the SAME offset hands the
                // new partition that exact stale superblock, and udev
                // incremental assembly resurrects the OLD array on it
                // immediately -- before ANY code in this function runs
                // against it. That live holder has to be stopped before
                // zeroing (zeroing a superblock the kernel has already
                // acted on is a no-op) and before `mdadm --create` (which
                // otherwise hits the exact same EBUSY the old rollback bug
                // did). Authorization for the STOP itself is scoped inside
                // `stop_any_foreign_holder_before_create` (only an array
                // made purely of this request's own new partitions) --
                // zeroing a brand-new partition that never had a
                // superblock, and that isn't held by anything foreign, is a
                // harmless no-op, which is why the `ConfirmSink` gate above
                // (step 4.5) already covers it.
                self.stop_any_foreign_holder_before_create(
                    &parted,
                    &mdadm,
                    &member_specs,
                    &target_kernel_names,
                )?;
                for member in &member_part_paths {
                    mdadm.zero_superblock(member)?;
                }

                mdadm.create_array(&md_name, level_str, &member_refs)?;
                journal.push(UndoAction::TeardownArray {
                    md_name: md_name.clone(),
                    member_paths: member_part_paths.clone(),
                });
                let md_dev_path = format!("/dev/{}", md_name);
                created_md_devices.push(md_dev_path.clone());

                let md_uuid = mdadm.read_uuid(&md_name)?;

                state_bands.push(StateBand {
                    index: band.band_index(),
                    level: level_str.to_string(),
                    md_name: md_name.clone(),
                    md_uuid: Some(md_uuid),
                    member_partitions: member_part_uuids,
                    usable_bytes: band.usable_bytes(),
                    ..Default::default()
                });
            }

            self.progress.report(ProgressUpdate {
                operation: "create".to_string(),
                stage: "lvm".to_string(),
                percent: Some(0.0),
                message: "setting up LVM volumes".to_string(),
            });

            // Setup LVM
            for md_dev in &created_md_devices {
                lvm.pvcreate(md_dev)?;
                journal.push(UndoAction::RemovePv {
                    dev_path: md_dev.clone(),
                });
            }

            let pv_refs: Vec<&str> = created_md_devices.iter().map(AsRef::as_ref).collect();
            lvm.vgcreate(&req.vg_name, &pv_refs)?;
            journal.push(UndoAction::RemoveVg {
                vg_name: req.vg_name.clone(),
            });
            lvm.lvcreate_max(&req.vg_name, &req.lv_name)?;
            let lv_path = format!("/dev/{}/{}", req.vg_name, req.lv_name);
            journal.push(UndoAction::RemoveLv {
                lv_path: lv_path.clone(),
            });

            self.progress.report(ProgressUpdate {
                operation: "create".to_string(),
                stage: "filesystem".to_string(),
                percent: Some(0.0),
                message: format!("creating and mounting Btrfs at {}", req.mount_point),
            });

            // Setup Btrfs & mount
            btrfs.mkfs(&lv_path, Some("SHR_VOLUME"))?;
            // Was a raw `std::fs::create_dir_all` gated on `!is_dry_run()`.
            // Mock runners also report `is_dry_run() == false`, so the guard did
            // not make it test-safe -- every non-dry-run test wrote real
            // directories on the dev host. `DryRunRunner` only records command
            // strings, so routing through the runner needs no guard.
            self.runner.run("mkdir", &["-p", &req.mount_point])?;
            // mount the filesystem's default (top-level,
            // subvolid=5) subvolume first -- `@`/`@snapshots` don't exist
            // yet, so there is nothing else TO mount. `@` becomes the
            // array's actual data; `@snapshots` holds `fs snapshot
            // create`'s read-only snapshots of `@` (the design),
            // kept as a SIBLING of `@` (not nested under it) so a snapshot
            // of `@` never recursively contains earlier snapshots. This is
            // a brand-new filesystem `create()` just formatted, never an
            // existing one being adopted -- no migration path needed.
            btrfs.mount(&lv_path, &req.mount_point, Some(&req.compression), None)?;
            journal.push(UndoAction::Unmount {
                mount_point: req.mount_point.clone(),
            });
            btrfs.create_subvolume(&req.mount_point, "@")?;
            btrfs.create_subvolume(&req.mount_point, "@snapshots")?;
            // Swap to the real, ongoing mount (`subvol=@`) by unmounting
            // and remounting -- Btrfs only honors `subvol=` on a fresh
            // mount, not via `mount -o remount`. The single `Unmount`
            // journal entry pushed above still correctly tears down
            // whichever of the two mounts is active if anything below
            // fails.
            btrfs.unmount(&req.mount_point)?;
            btrfs.mount(&lv_path, &req.mount_point, Some(&req.compression), Some("@"))?;
            let fs_uuid = btrfs.read_uuid(&lv_path)?;

            let mode_str = match req.mode {
                RedundancyMode::Shr => "shr",
                RedundancyMode::Shr2 => "shr2",
            };

            Ok(ArrayState {
                name: req.name.clone(),
                mode: mode_str.to_string(),
                created_at: Utc::now().to_rfc3339(),
                layout_version: 1,
                disks: state_disks,
                bands: state_bands,
                filesystem: StateFilesystem {
                    fs_uuid: Some(fs_uuid),
                    mount_point: req.mount_point.clone(),
                    vg_name: req.vg_name.clone(),
                    lv_name: req.lv_name.clone(),
                    compression: req.compression.clone(),
                },
                expansion: StateExpansion::default(),
            })
        })();

        let state = match outcome {
            Ok(state) => state,
            Err(e) => return Err(self.wrap_with_rollback(&journal, e, Some(&target_kernel_names))),
        };

        // A dry-run is an inspection operation: it must not create or replace
        // persistent array state.  The caller can still render this simulated
        // state in JSON/text, but no daemon will ever treat it as an array.
        // (Deliberately outside the rollback scope above: a `StateStore::save`
        // failure here means the array physically exists but wasn't recorded
        // -- rolling back a working array over a bookkeeping-file error would
        // be worse than the problem it solves.)
        if !self.runner.is_dry_run() {
            prune_retired_arrays_for(&mut full_state, &req.disks);
            full_state.groups.push(state.clone());
            // A fresh redundant array starts resyncing the moment it is
            // created, and that resync used to be governed by no profile at
            // all. Non-fatal: the array exists and is about to be recorded,
            // so a failed sysfs write is worth reporting, not worth failing
            // a successful `create` over.
            let group_idx = full_state.groups.len() - 1;
            for band_pos in 0..full_state.groups[group_idx].bands.len() {
                if let Err(e) = self.govern_running_sync(&mut full_state, group_idx, band_pos) {
                    tracing::warn!(target: "shr_rs::throttle", "band {band_pos} sync limits: {e}");
                }
            }
            self.store.save(&full_state)?;
            self.write_managed_configs(&full_state)?;
        }
        self.progress.report(ProgressUpdate {
            operation: "create".to_string(),
            stage: "done".to_string(),
            percent: Some(100.0),
            message: format!("group `{}` created", state.name),
        });
        Ok(state)
    }

    /// Undo everything recorded in `journal`, in reverse order. A no-op
    /// under dry-run: nothing physical was ever created, so there is
    /// nothing to undo, and issuing "undo" commands would misrepresent a
    /// simulation as having touched real state. Returns a human-readable
    /// description of every step that failed to undo (empty if rollback
    /// fully succeeded) -- the caller is responsible for not losing the
    /// original error that triggered the rollback.
    ///
    /// `safe_stop_targets`: same "only stop an array made purely of our own
    /// new partitions" rule `stop_any_foreign_holder_before_create` enforces
    /// in the forward path -- `Some(set)` means the caller knows
    /// exactly which new kernel device names are its own, so
    /// `RemovePartition`'s holder-stop below refuses (and records a failure
    /// instead of touching) an array with a member outside that set, rather
    /// than stopping it unconditionally. Necessary, not just symmetrical:
    /// a forward-path check that already refuses a foreign-spanning holder
    /// as a hard validation error, before ANY array for this attempt
    /// exists, must not have `rollback` then stop that SAME array
    /// unconditionally while unwinding the partitions already carved for
    /// this same failed attempt -- that would both refuse and perform the
    /// identical unsafe stop, just one step apart.
    ///
    /// `execute_grow`/`execute_create_band` also pass `Some(set)` now
    /// (previously `None`, unconditional). For `execute_create_band` this
    /// is the exact same shape as `create()` -- a whole new band/array, so
    /// `target_kernel_names` is that array's complete new membership. For
    /// `execute_grow`, `target_kernel_names` holds ONLY the newly-added
    /// member(s), never the target band's pre-existing members -- so if a
    /// `RemoveSpareMember` undo failed to detach a new member before this
    /// `RemovePartition` runs, the live holder found here is the real,
    /// pre-existing band array, and its OTHER (pre-existing) members are
    /// correctly outside `target_kernel_names`, producing the same refusal
    /// rather than `mdadm --stop`ping a working array with real data still
    /// on it. That refusal is strictly safer than the old unconditional-stop
    /// behavior it replaces, not a behavior this rollback path relied on.
    /// `None` remains available for any future caller that has no
    /// meaningful "own partitions" set to scope the stop to.
    fn rollback(&self, journal: &[UndoAction], safe_stop_targets: Option<&HashSet<String>>) -> Vec<String> {
        if self.runner.is_dry_run() {
            return Vec::new();
        }

        let parted = PartedExecutor::new(self.runner);
        let mdadm = MdadmExecutor::new(self.runner);
        let lvm = LvmExecutor::new(self.runner);
        let btrfs = BtrfsExecutor::new(self.runner);
        let mut failures = Vec::new();

        for action in journal.iter().rev() {
            match action {
                UndoAction::Unmount { mount_point } => {
                    if let Err(e) = btrfs.unmount(mount_point) {
                        failures.push(format!("unmount {mount_point}: {e}"));
                    }
                }
                UndoAction::RemoveLv { lv_path } => {
                    if let Err(e) = lvm.lvremove(lv_path) {
                        failures.push(format!("lvremove {lv_path}: {e}"));
                    }
                }
                UndoAction::RemoveVg { vg_name } => {
                    if let Err(e) = lvm.vgremove(vg_name) {
                        failures.push(format!("vgremove {vg_name}: {e}"));
                    }
                }
                UndoAction::RemovePv { dev_path } => {
                    if let Err(e) = lvm.pvremove(dev_path) {
                        failures.push(format!("pvremove {dev_path}: {e}"));
                    }
                }
                UndoAction::TeardownArray {
                    md_name,
                    member_paths,
                } => {
                    if let Err(e) = mdadm.stop_array(md_name) {
                        failures.push(format!("stop {md_name}: {e}"));
                    }
                    for member in member_paths {
                        if let Err(e) = mdadm.zero_superblock(member) {
                            failures.push(format!("zero-superblock {member}: {e}"));
                        }
                    }
                }
                UndoAction::RemoveSpareMember { md_name, member_path } => {
                    if let Err(e) = mdadm.remove_member(md_name, member_path) {
                        failures.push(format!("remove spare {member_path} from {md_name}: {e}"));
                    }
                }
                UndoAction::RemovePartition { disk_path, part_num } => {
                    // The more important half of that fix: a
                    // partition being rolled back may have been
                    // auto-assembled into an mdadm array this create()
                    // attempt never made itself (e.g. udev incremental
                    // assembly resurrecting a residual superblock left by
                    // an earlier `destroy` without `--zero-superblocks` --
                    // seen on a real guest).
                    // `parted rm` refuses to touch a partition an active
                    // array is holding ("unable to inform the kernel of
                    // the change ... probably because it/they are in
                    // use"), so before removing it, check the kernel's
                    // LIVE membership view for any array holding it --
                    // regardless of whether it's in THIS journal (the
                    // `TeardownArray` branch above already covers that
                    // case) or a foreign array this run never created --
                    // and stop it first.
                    match self.holder_md_array(&parted, disk_path, *part_num) {
                        Ok(Some(holder)) => {
                            let foreign_members: Option<Vec<&str>> = safe_stop_targets.map(|targets| {
                                holder
                                    .members
                                    .iter()
                                    .map(|m| m.name.as_str())
                                    .filter(|name| !targets.contains(*name))
                                    .collect()
                            });
                            match foreign_members {
                                Some(foreign) if !foreign.is_empty() => {
                                    // Same refusal `stop_any_foreign_holder_before_create`
                                    // would have made in the forward path -- do NOT stop
                                    // an array that spans a disk this `create()` attempt
                                    // never confirmed anything about, even while rolling
                                    // back. `parted rm` below is left to fail on its own
                                    // (and get logged as its own failure) since the
                                    // partition genuinely can't be freed without touching
                                    // that foreign array.
                                    failures.push(format!(
                                        "will not stop array `{}` (auto-assembled on \
                                         partition {part_num} of {disk_path}): it also has \
                                         member(s) {foreign:?} outside this create \
                                         request's own disks -- inspect it manually \
                                         (`mdadm --detail /dev/{}`)",
                                        holder.name, holder.name
                                    ));
                                }
                                _ => {
                                    if let Err(e) = mdadm.stop_array(&holder.name) {
                                        failures.push(format!(
                                            "stop {} (auto-assembled on partition {part_num} \
                                             of {disk_path}) before removing that partition: {e}",
                                            holder.name
                                        ));
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => failures.push(format!(
                            "check for an array holding partition {part_num} on {disk_path}: {e}"
                        )),
                    }
                    if let Err(e) = parted.remove_partition(disk_path, *part_num) {
                        failures.push(format!("remove partition {part_num} on {disk_path}: {e}"));
                    }
                }
            }
        }

        failures
    }

    /// Whether SOME mdadm array currently lists `disk_path`'s partition
    /// `part_num` as a member, per live `/proc/mdstat` -- and if so, that
    /// array's full record (name AND members, e.g. so a caller can check
    /// whether every one of ITS members is something the caller already
    /// has authorization to touch). Shared by two callers: `rollback`'s
    /// `RemovePartition` handling and `create`'s own
    /// pre-`mdadm --create` holder check -- both need to find an
    /// array a partition was unexpectedly auto-assembled into; a
    /// foreign/auto-assembled array that never went through THIS
    /// operation's own bookkeeping can still be holding the partition, and
    /// both `parted rm` (rollback) and `mdadm --create` (forward path)
    /// refuse to touch it until whatever's holding it is stopped.
    ///
    /// Resolves the same way `mdadm --create`'s member paths were built
    /// (via `PartedExecutor::partition_path_for_read`, the canonical
    /// kernel path) so the name searched for here is the SAME kernel
    /// device name `/proc/mdstat` itself reports members by -- not the
    /// by-id path `disk_path` is expressed in.
    fn holder_md_array(
        &self,
        parted: &PartedExecutor,
        disk_path: &str,
        part_num: u32,
    ) -> Result<Option<MdArray>, ExecError> {
        let kernel_path = parted.partition_path_for_read(disk_path, part_num);
        let kernel_name = kernel_path.rsplit('/').next().unwrap_or(&kernel_path);
        let mdstat_output = self.runner.run("cat", &["/proc/mdstat"])?;
        let mdstat = parse_mdstat(&mdstat_output.stdout);
        Ok(mdstat
            .arrays
            .into_iter()
            .find(|a| a.members.iter().any(|m| m.name == kernel_name)))
    }

    /// Corrected: the first version of this fix zeroed each new
    /// member partition's superblock unconditionally before `mdadm
    /// --create` and stopped there. Real-guest re-verification showed that
    /// doesn't work -- by the time `zero_superblock` runs, udev incremental
    /// assembly has ALREADY resurrected the old array on the partition
    /// (this happens the instant `parted mkpart` finishes, immediately
    /// after `settle_udev`, well before this function is even called), so
    /// `--zero-superblock` is itself operating on a busy device and
    /// silently fails to clear anything; `mdadm --create` then hits the
    /// exact same EBUSY it was meant to prevent. Zeroing a superblock the
    /// kernel has already acted on is a no-op -- the holder has to be
    /// stopped FIRST, THEN zeroed, THEN (re-)created on top of.
    ///
    /// Deliberately narrower than "stop any array holding a disk we were
    /// told to use": only ever stops an array whose ENTIRE membership is a
    /// subset of `target_kernel_names` (every new member partition THIS
    /// `create()` request itself is creating, across every band/disk, built
    /// by the caller while partitioning). The disks named in a `create`
    /// request are confirmed targets -- but an array that ALSO has a member
    /// outside that set spans a disk this request never confirmed anything
    /// about (e.g. an operator reusing only some of a previously-`destroy`d
    /// group's disks, where udev resurrects the OLD, larger array using a
    /// mix of newly re-created and still-untouched partitions). Stopping
    /// that would be destructive to something the operator didn't
    /// authorize just by authorizing a create on a different, unrelated
    /// disk set -- so that case is a hard `Validation` error naming the
    /// holder and its foreign members, not something silently stopped.
    ///
    /// Also verifies the stop actually took effect by re-reading live
    /// `/proc/mdstat` afterward, rather than trusting `mdadm --stop`'s exit
    /// code (the lesson, again: a command returning 0 is not proof the
    /// kernel state changed).
    ///
    /// Also called (reused verbatim, not reimplemented) by
    /// `execute_grow` right before `mdadm --add` and by
    /// `execute_create_band` right before its own `mdadm --create` -- the
    /// exact same udev-resurrection race a partition can hit whether it's
    /// part of a brand-new `create()` or a later `expand()` step. Only
    /// `create()` and `execute_create_band` build `target_kernel_names` as
    /// a brand-new array's full membership; `execute_grow`'s set is
    /// narrower (only the new member(s) being added to an EXISTING array),
    /// which is still correct here since a resurrected foreign superblock
    /// can only ever land on a partition this call itself just carved.
    fn stop_any_foreign_holder_before_create(
        &self,
        parted: &PartedExecutor,
        mdadm: &MdadmExecutor,
        member_specs: &[(String, u32)],
        target_kernel_names: &HashSet<String>,
    ) -> Result<(), OrchestrateError> {
        for (disk_path, part_num) in member_specs {
            let Some(holder) = self.holder_md_array(parted, disk_path, *part_num)? else {
                continue;
            };
            let foreign_members: Vec<&str> = holder
                .members
                .iter()
                .map(|m| m.name.as_str())
                .filter(|name| !target_kernel_names.contains(*name))
                .collect();
            if !foreign_members.is_empty() {
                return Err(OrchestrateError::Validation(format!(
                    "partition {part_num} on {disk_path} is already part of live array `{}`, \
                     which also has member(s) {foreign_members:?} outside this create \
                     request's own disks; refusing to stop an array that spans disks this \
                     request never confirmed -- inspect it manually (`mdadm --detail \
                     /dev/{}`) and stop/reassign it before retrying",
                    holder.name, holder.name
                )));
            }
            mdadm.stop_array(&holder.name)?;
            // Don't trust the exit code -- re-read live /proc/mdstat
            // and confirm the array is actually gone before proceeding to
            // zero and (re-)create on top of the same partitions.
            if self.holder_md_array(parted, disk_path, *part_num)?.is_some() {
                return Err(OrchestrateError::Validation(format!(
                    "stopped array `{}` (mdadm --stop exited successfully) but /proc/mdstat \
                     still shows a holder on partition {part_num} of {disk_path} -- refusing \
                     to proceed onto a partition the kernel still shows as in use",
                    holder.name
                )));
            }
        }
        Ok(())
    }

    /// Wrap `e` with the result of rolling back `journal`: the bare error if
    /// rollback fully succeeded, or `OrchestrateError::Rollback` if it
    /// didn't -- either way the original error is never lost.
    /// `safe_stop_targets` is forwarded verbatim to `rollback` -- see its
    /// doc comment.
    fn wrap_with_rollback(
        &self,
        journal: &[UndoAction],
        e: OrchestrateError,
        safe_stop_targets: Option<&HashSet<String>>,
    ) -> OrchestrateError {
        let failures = self.rollback(journal, safe_stop_targets);
        if failures.is_empty() {
            e
        } else {
            OrchestrateError::Rollback {
                source: Box::new(e),
                failures,
            }
        }
    }

    /// Orchestrate expansion by adding new disks (D1). Computes a real
    /// `shr_core::plan_expansion` (D12 -- the previous implementation never
    /// called it) and executes its steps for real; the previous
    /// implementation only incremented `layout_version` and reported
    /// success without touching a single disk -- the exact false-success
    /// this project's smoke-test principles exist to catch. Steps run
    /// strictly sequentially, never concurrently (the design: no
    /// simultaneous reshapes).
    /// Complete any deferred LVM/Btrfs resize left behind by `execute_grow`
    /// when a `--grow` reshape was still running at the time
    /// (`resize_pending` must be a persisted record that's actually
    /// completable, not a silently-dropped gap). For every band with
    /// `resize_pending == true`, checks whether its reshape has since
    /// finished (`sync_action == "idle"`) and if so, runs the deferred
    /// `pvresize`/`lvextend`/`btrfs resize max` now and clears the flag.
    /// A safe no-op when there's no active array, nothing pending, or a
    /// pending band's reshape is still running -- meant to be called both
    /// opportunistically (every `expand()` does, before touching anything
    /// new) and explicitly (`shr-rs reconcile`).
    ///
    /// Multi-group support: iterates every band of EVERY group, not just
    /// whichever one a caller happens to be thinking about -- a deferred
    /// resize left behind on group A must still get finished by a plain
    /// `shr-rs reconcile` (or an `expand()` targeting group B) even though
    /// neither of those names group A.
    pub fn reconcile(&self) -> Result<Option<ReconcileOutcome>, OrchestrateError> {
        let Some(mut state) = self.store.load()? else {
            return Ok(None);
        };
        let mut changed = false;
        // Everything this call actually did, handed back alongside
        // `state` so a caller (`shr-cli`'s `reconcile` handler) can report
        // it -- see `ReconcileAction`'s doc comment for the real-guest
        // repro this exists to fix.
        let mut performed: Vec<ReconcileAction> = Vec::new();

        // Self-heal `scrub_in_progress` for EVERY group/band against
        // real kernel state -- NOT gated behind the resize-pending
        // shortcut just below, since a scheduled scrub finishing has
        // nothing to do with any deferred reshape. The systemd timer this
        // project generates only ever calls `fs scrub start`; nothing calls `fs
        // scrub status` afterward to observe a scrub that finished on its
        // own, so without this the flag stays stuck `true` forever after
        // the first scheduled run. `expand()` already opportunistically
        // calls `reconcile()` before touching anything new, so this also
        // self-heals the flag right before the "is a scrub currently
        // running" check reads it.
        //
        // Only probes a group whose stored state actually claims a scrub
        // is in flight (`scrub_in_progress` set on at least one band) --
        // there is nothing to reconcile for a group that was never
        // scrubbed, and probing anyway would needlessly read every group's
        // live sync_action/mismatch_cnt/`btrfs scrub status` on EVERY
        // `reconcile()`/opportunistic-`expand()` call, including the many
        // validation paths that return before doing anything and are
        // meant to touch nothing at all.
        for group_idx in 0..state.groups.len() {
            if !state.groups[group_idx].bands.iter().any(|b| b.scrub_in_progress) {
                continue;
            }
            let (_, _, mut scrub_actions) = self.reconcile_group_scrub(&mut state, group_idx)?;
            changed |= !scrub_actions.is_empty();
            performed.append(&mut scrub_actions);
        }

        // Finish any deferred `--remove` of an old `--replace`d
        // member left attached because its copy was still running when
        // `replace_disk` returned. Same shape as the scrub self-heal above:
        // `reconcile_pending_member_removals` itself is a true no-op (zero
        // commands) for a group with nothing recorded pending.
        for group_idx in 0..state.groups.len() {
            let (removal_changed, mut removal_actions) =
                self.reconcile_pending_member_removals(&mut state, group_idx)?;
            changed |= removal_changed;
            performed.append(&mut removal_actions);
        }

        if state
            .groups
            .iter()
            .any(|g| g.bands.iter().any(|b| b.resize_pending))
        {
            let mdadm = MdadmExecutor::new(self.runner);
            let lvm = LvmExecutor::new(self.runner);
            let btrfs = BtrfsExecutor::new(self.runner);

            for group in &mut state.groups {
                for i in 0..group.bands.len() {
                    if !group.bands[i].resize_pending {
                        continue;
                    }
                    let md_name = group.bands[i].md_name.clone();
                    if mdadm.sync_action(&md_name)? != "idle" {
                        continue;
                    }
                    // No `ConfirmSink` gate here (unlike `create`/`expand`):
                    // `reconcile` never starts a NEW destructive action, it only
                    // finishes bookkeeping (pvresize/lvextend/resize_max, all
                    // capacity-increasing, none of them destructive) for a
                    // reshape that was already approved and already physically
                    // committed by a prior `expand()` call -- see
                    // `execute_grow`'s `resize_pending` doc comment.
                    self.progress.report(ProgressUpdate {
                        operation: "reconcile".to_string(),
                        stage: "resize".to_string(),
                        percent: None,
                        message: format!(
                            "growing the storage onto the new space for band {} ({md_name})",
                            group.bands[i].index
                        ),
                    });
                    let md_dev_path = format!("/dev/{md_name}");
                    lvm.pvresize(&md_dev_path)?;
                    lvm.lvextend_max(&group.filesystem.vg_name, &group.filesystem.lv_name)?;
                    btrfs.resize_max(&group.filesystem.mount_point)?;
                    group.bands[i].resize_pending = false;
                    changed = true;
                    // This is a COMPLETED resize, not a pending one --
                    // it must never be reported (or, worse, silently
                    // omitted) the same way as "nothing to do here".
                    performed.push(ReconcileAction::ResizeCompleted {
                        group: group.name.clone(),
                        band_index: group.bands[i].index,
                        md_name,
                    });
                }
            }
        }

        // Hand back the per-array limits of every band whose sync has
        // finished. `tick_active_sync` does this too, but nothing guarantees
        // a host ever ran `schedule install`, and a floor left behind
        // silently governs every later operation on that array. Skipped
        // under dry-run for the same reason as the host-wide restore below.
        if !self.runner.is_dry_run() {
            for group_idx in 0..state.groups.len() {
                for band_pos in 0..state.groups[group_idx].bands.len() {
                    if state.groups[group_idx].bands[band_pos].sync_priority.is_none() {
                        continue;
                    }
                    let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
                    let Some(action) = Self::live_sync_action(self.status_runner(), &md_name)? else {
                        continue;
                    };
                    if action != "idle" {
                        continue;
                    }
                    changed |= self.clear_band_limits(&mut state, group_idx, band_pos)?;
                }
            }
        }

        // Last, deliberately: the self-heals above are what clear the flags
        // for a scrub or reshape that has actually finished, and this reads
        // the same live `sync_action` they just acted on. A true no-op (zero
        // commands) whenever this project has not borrowed the host-wide
        // speed limit, which is the normal case.
        if let Some(speed_kb) = self.restore_speed_limit_if_idle(&mut state)? {
            changed = true;
            performed.push(ReconcileAction::SpeedLimitRestored { speed_kb });
        }

        if changed && !self.runner.is_dry_run() {
            self.store.save(&state)?;
        }
        Ok(Some(ReconcileOutcome { state, performed }))
    }

    /// Read live scrub status for one group and self-heal any
    /// band whose `scrub_in_progress` bit is stale (that band's real
    /// `sync_action`/`btrfs scrub status` says nothing is running anymore,
    /// but the persisted bit still says `true`) by recording the same
    /// "just finished" result `scrub_status()` always has. Reads go through
    /// `self.status_runner()`, not `self.runner` directly -- see
    /// `status_runner`'s doc comment: `expand()`'s "is a scrub
    /// currently running" check and `reconcile()`'s self-heal both need
    /// this to answer truthfully even when `self.runner` is a
    /// `DryRunRunner` (a `preview_expand` call). Returns
    /// `(running, error_count, actions)`; persisting is the CALLER's job
    /// (gated on `!actions.is_empty()`), since `scrub_status`/`reconcile`
    /// gate that on different things (a single group vs. every group) and
    /// have their own `is_dry_run()`-guarded save call already. `actions`
    /// is always either empty or a single `ScrubSelfHealed` per healed band
    /// -- `scrub_status()` only needs `is_empty()`; `reconcile()`
    /// forwards the contents to its caller so a self-heal it performed is
    /// actually reported, not silently folded into "nothing pending".
    fn reconcile_group_scrub(
        &self,
        state: &mut StateFile,
        group_idx: usize,
    ) -> Result<(bool, u64, Vec<ReconcileAction>), OrchestrateError> {
        let mdadm = MdadmExecutor::new(self.status_runner());
        let btrfs = BtrfsExecutor::new(self.status_runner());

        let band_count = state.groups[group_idx].bands.len();
        let mut mdadm_running = false;
        let mut mdadm_error_total: u64 = 0;
        for i in 0..band_count {
            let md_name = state.groups[group_idx].bands[i].md_name.clone();
            if mdadm.sync_action(&md_name)? != "idle" {
                mdadm_running = true;
            }
            mdadm_error_total += mdadm.scrub_error_count(&md_name)?;
        }
        let btrfs_status = btrfs.scrub_status(&state.groups[group_idx].filesystem.mount_point)?;
        let running = mdadm_running || btrfs_status.running;
        let error_count = mdadm_error_total + btrfs_status.error_count;

        let mut actions = Vec::new();
        if !running {
            let now = Utc::now().to_rfc3339();
            let group_name = state.groups[group_idx].name.clone();
            for i in 0..band_count {
                if !state.groups[group_idx].bands[i].scrub_in_progress {
                    continue;
                }
                let per_band_errors = mdadm.scrub_error_count(&state.groups[group_idx].bands[i].md_name)?;
                let band_index = state.groups[group_idx].bands[i].index;
                let md_name = state.groups[group_idx].bands[i].md_name.clone();
                state.groups[group_idx].bands[i].last_scrub = Some(StateScrubResult {
                    finished_at: now.clone(),
                    outcome: ScrubOutcome::Completed,
                    error_count: per_band_errors,
                });
                state.groups[group_idx].bands[i].scrub_in_progress = false;
                // Report the self-heal itself, not just its result --
                // the previous shape (a bare `changed` bool) told a caller
                // THAT state.toml was rewritten but not what actually
                // happened, the same blind spot the member-removal/resize
                // paths had.
                actions.push(ReconcileAction::ScrubSelfHealed {
                    group: group_name.clone(),
                    band_index,
                    md_name,
                    error_count: per_band_errors,
                });
                // Fire the moment a scrub this project started is
                // OBSERVED to have finished with errors -- reachable from
                // every caller of this helper: `scrub_status()` (`fs scrub
                // status`), `reconcile()`'s opportunistic call (inside
                // every `expand()`), and `check_health()`'s periodic poll.
                if per_band_errors > 0 {
                    self.notify(&NotifyEvent::ScrubErrorsFound {
                        group: group_name.clone(),
                        band_index,
                        error_count: per_band_errors,
                    });
                }
            }
        }
        Ok((running, error_count, actions))
    }

    /// Finish any deferred `--remove` of an old `--replace`d member left
    /// attached because its copy was still running when `replace_disk`
    /// returned -- `StateBand::pending_member_removal` records
    /// exactly which device path is left to clean up. real-guest
    /// repro: nothing ever finished this cleanup afterward, so the stale,
    /// faulty old member stayed attached to the array forever, `state.toml`
    /// and the live kernel diverged, and the disk could never be reused --
    /// same class of "command finishes, kernel state changes later" gap
    /// already fixed for `scrub_in_progress`, fixed here the same way: self-heal
    /// against real kernel state, shared by `reconcile()` (opportunistic/
    /// explicit) and `check_health()` (the periodic timer -- the ONLY
    /// thing that guarantees a real, multi-hour replace copy eventually
    /// gets cleaned up if nobody runs `expand`/`reconcile` again).
    ///
    /// A true no-op (constructs no executor, issues no command) when this
    /// group has nothing recorded pending -- same cost-guard shape as
    /// `reconcile_group_scrub`'s "never probe a group that was never
    /// scrubbed" rule, so a healthy host's `reconcile()`/`check_health()`
    /// stays cheap.
    /// Whether `member_path` (a by-partuuid path) is STILL a member of
    /// `md_name`'s array, per `/proc/mdstat` -- the only correct source of
    /// truth for array membership.
    ///
    /// Real-guest repro: `mdadm --remove` detaches a member from the
    /// array but never deletes its partition, so `MdadmExecutor::
    /// resolve_member_kernel_name`'s `readlink -e` keeps resolving long
    /// after a genuinely successful removal -- it answers "does this
    /// partition still exist on disk", not "is it still attached to the
    /// array". Using its `Some`-ness alone as a post-removal check (the
    /// pre-fix bug) made every real removal look like a failure, which
    /// then never cleared `pending_member_removal`, which then made
    /// `reconcile()` and the 15-minute health-check timer repeat the same
    /// false error forever.
    ///
    /// A dangling by-partuuid symlink (kernel name resolution fails
    /// entirely) is also treated as "not a member": a physically removed
    /// disk obviously cannot still be attached.
    fn member_still_in_array(
        &self,
        mdadm: &MdadmExecutor,
        md_name: &str,
        member_path: &str,
    ) -> Result<bool, OrchestrateError> {
        let Some(kernel_name) = mdadm.resolve_member_kernel_name(member_path)? else {
            return Ok(false);
        };
        let mdstat_output = self.runner.run("cat", &["/proc/mdstat"])?;
        let mdstat = parse_mdstat(&mdstat_output.stdout);
        Ok(mdstat
            .arrays
            .iter()
            .find(|a| a.name == md_name)
            .is_some_and(|a| a.members.iter().any(|m| m.name == kernel_name)))
    }

    /// Returns `(state_mutated, actions)`: `state_mutated` gates the
    /// caller's persistence (set whenever `pending_member_removal` was
    /// cleared, for ANY reason), while `actions` carries only the
    /// subset worth telling an operator about -- a REAL `mdadm --remove`
    /// this call issued. The two are deliberately not the same signal: if
    /// the by-partuuid symlink is already gone (someone else beat this call
    /// to it), stale bookkeeping still needs clearing/persisting, but this
    /// call did not itself remove anything and must not claim it did.
    fn reconcile_pending_member_removals(
        &self,
        state: &mut StateFile,
        group_idx: usize,
    ) -> Result<(bool, Vec<ReconcileAction>), OrchestrateError> {
        if !state.groups[group_idx]
            .bands
            .iter()
            .any(|b| b.pending_member_removal.is_some())
        {
            return Ok((false, Vec::new()));
        }
        let mdadm = MdadmExecutor::new(self.runner);
        let mut changed = false;
        let mut actions = Vec::new();
        let band_count = state.groups[group_idx].bands.len();
        for i in 0..band_count {
            let Some(old_member_path) = state.groups[group_idx].bands[i].pending_member_removal.clone()
            else {
                continue;
            };
            let md_name = state.groups[group_idx].bands[i].md_name.clone();
            let band_index = state.groups[group_idx].bands[i].index;
            if mdadm.sync_action(&md_name)? != "idle" {
                continue; // the copy is still running -- stays pending
            }

            // Confirm the kernel still actually shows this device
            // attached AND faulty before touching it -- an operator (or an
            // earlier partial run of this same cleanup) may already have
            // removed it by hand, in which case there is nothing left to
            // do but clear the stale bookkeeping; a device that's attached
            // but NOT faulty is left alone entirely (never remove a live,
            // healthy member) and stays pending for a later attempt.
            if let Some(kernel_name) = mdadm.resolve_member_kernel_name(&old_member_path)? {
                let mdstat_output = self.runner.run("cat", &["/proc/mdstat"])?;
                let mdstat = parse_mdstat(&mdstat_output.stdout);
                let is_faulty = mdstat
                    .arrays
                    .iter()
                    .find(|a| a.name == md_name)
                    .is_some_and(|a| a.members.iter().any(|m| m.name == kernel_name && m.faulty));
                if !is_faulty {
                    continue;
                }

                self.progress.report(ProgressUpdate {
                    operation: "reconcile".to_string(),
                    stage: "remove-stale-member".to_string(),
                    percent: None,
                    message: format!(
                        "band {band_index} ({md_name}): its replacement has finished syncing -- \
                         removing the old disk `{old_member_path}` ({kernel_name})"
                    ),
                });
                mdadm.remove_member(&md_name, &old_member_path)?;
                // Trust the kernel, not the exit code (its own rule) --
                // and here "trust the kernel" means array MEMBERSHIP
                // (`/proc/mdstat`), not partition existence (`readlink`).
                if self.member_still_in_array(&mdadm, &md_name, &old_member_path)? {
                    return Err(OrchestrateError::Validation(format!(
                        "band {band_index} ({md_name}): `mdadm --remove {old_member_path}` exited \
                         successfully but the kernel still shows it attached -- inspect `mdadm \
                         --detail {md_name}` by hand"
                    )));
                }
                // A real, verified removal -- the only case this
                // helper reports as an action (see the function doc
                // comment for why the `None` branch below does not).
                actions.push(ReconcileAction::MemberRemoved {
                    group: state.groups[group_idx].name.clone(),
                    band_index,
                    md_name: md_name.clone(),
                    member_path: old_member_path.clone(),
                });
            }
            // `None`: the by-partuuid symlink is already gone -- an
            // operator (or a prior run of this cleanup) already removed it.
            // Nothing to report as an action THIS call performed, but the
            // stale bookkeeping below still needs clearing/persisting.

            state.groups[group_idx].bands[i].pending_member_removal = None;
            changed = true;
        }
        Ok((changed, actions))
    }

    /// Poll every group for the three notification triggers (a scrub
    /// finding errors, a band being degraded, worsening SMART health) and
    /// fire through every channel `self.notify_policy` enables. Meant to
    /// be called periodically by its own systemd timer
    /// (`internal health-check-tick`, mirroring the throttle-tick timer's
    /// established pattern) -- deliberately a SEPARATE, always-
    /// does-real-reads entrypoint, not folded into `reconcile()`: several
    /// `expand()` validation paths rely on `reconcile()` staying a true
    /// no-op ("must not touch anything") when nothing is pending, and this
    /// method's reads are NOT conditional the way `reconcile()`'s are.
    ///
    /// "Degraded" fires on EVERY call this observes `degraded_count() > 0`
    /// for a band, not just the first transition -- deliberately: this
    /// project has no cross-process place to remember "already notified
    /// for this outage" that a schema change wouldn't require touching
    /// call sites this worker does not own (`shr-command`'s render/tests
    /// are out of scope for this change), and a periodic REMINDER while a
    /// band remains degraded is itself a defensible, common alerting
    /// pattern (most monitoring systems re-fire an unresolved critical
    /// alert), not silence. `systemd-notify --status=...` in particular is
    /// idempotent to repeat (it just keeps the unit's status line current).
    ///
    /// Real-guest repro: with a group's array not assembled at all
    /// (`state.toml` intact, e.g. a reboot came back without its member
    /// devices), `degraded_count`'s `cat /sys/block/<md>/md/degraded` has no
    /// file to read and fails. The old code let that `?` propagate, which
    /// aborted THIS WHOLE TICK -- not just the missing band, every LATER
    /// group's health check too -- in exactly the situation (total loss of
    /// a band) that most needs an alert, on the one path meant to notice
    /// when nobody is watching. Fixed at the call site below: a failed read
    /// notifies `ArrayMissing` (a missing array is a MORE severe state than
    /// `Degraded`, not a lesser one worth staying quiet about) and the loop
    /// keeps going, never `?`-propagating out of this function.
    ///
    /// That `ArrayMissing` notify only fires for the SPECIFIC shape the real
    /// guest produced -- `NonZeroExit` with `"No such file or directory"` in
    /// `cat`'s stderr. Any other read failure (`degraded_count`'s own
    /// `Prerequisite` when the array IS assembled but its sysfs contents
    /// didn't parse, or a `NonZeroExit` for some other reason, e.g.
    /// permission denied) is NOT treated as a missing array -- doing so
    /// would assert a false, specific cause about a machine that is not in
    /// that state. The sweep still continues either way (that is the actual
    /// earlier fix); only the choice of WHICH notification to send, if any,
    /// depends on the failure shape. The residual case notifies nothing
    /// (asserting no cause is safer than asserting a wrong one) but is not
    /// silent: `SystemRunner::run` already logs every command it runs,
    /// success or failure, before `degraded_count` ever wraps the result
    /// into this `Err` -- see the per-band match arm below.
    pub fn check_health(&self) -> Result<(), OrchestrateError> {
        let Some(mut state) = self.store.load()? else {
            return Ok(());
        };
        let mut changed = false;
        let mdadm = MdadmExecutor::new(self.runner);

        for group_idx in 0..state.groups.len() {
            let group_name = state.groups[group_idx].name.clone();

            // Scrub errors -- reuses the self-heal/observe path, scoped
            // the same way (only groups with a scrub genuinely started
            // here); firing itself already happens inside
            // `reconcile_group_scrub`.
            //
            // Sibling: `reconcile_group_scrub` reads every band's live
            // `sync_action`/`mismatch_cnt` with `?` internally -- same
            // vanished-array failure mode as `degraded_count`, reachable
            // here on any band in this group, not just the one with
            // `scrub_in_progress` set. NOT `?`'d here for the same reason:
            // one group's unreadable band must not abort the rest of this
            // tick. No separate notify needed on `Err` -- the per-band loop
            // below still reaches this group and fires `ArrayMissing` for
            // whichever band actually vanished; `reconcile()`/`scrub_status`
            // (operator-invoked, unlike this timer) still see the real
            // error when run directly.
            if state.groups[group_idx].bands.iter().any(|b| b.scrub_in_progress) {
                if let Ok((_, _, scrub_actions)) = self.reconcile_group_scrub(&mut state, group_idx) {
                    changed |= !scrub_actions.is_empty();
                }
            }

            // This periodic timer is what actually guarantees a
            // deferred `replace`-member removal gets finished for a real,
            // multi-hour copy even if the operator never runs `expand`/
            // `reconcile` again -- see `reconcile_pending_member_removals`'s
            // doc comment. This entrypoint has no `ReconcileAction`
            // consumer of its own (no CLI command surfaces a "what did the
            // periodic health check do" report) -- only the mutation bit is
            // needed here.
            //
            // Sibling, same rationale as `reconcile_group_scrub` just
            // above: its internal `sync_action(&md_name)?` can fail the
            // same way if this band's array vanished. `Err` here means
            // "could not finish this cleanup this tick" -- not fatal to the
            // tick itself.
            if let Ok((removal_changed, _removal_actions)) =
                self.reconcile_pending_member_removals(&mut state, group_idx)
            {
                changed |= removal_changed;
            }

            for i in 0..state.groups[group_idx].bands.len() {
                let md_name = state.groups[group_idx].bands[i].md_name.clone();
                let band_index = state.groups[group_idx].bands[i].index;

                // NOT `?` -- see this function's doc comment. A read
                // failure here (in practice: the array isn't assembled, so
                // the sysfs file this reads doesn't exist) must notify and
                // move on, not abort every remaining band/group this tick
                // would otherwise have checked.
                match mdadm.degraded_count(&md_name) {
                    Ok(count) => {
                        if count > 0 {
                            self.notify(&NotifyEvent::Degraded {
                                group: group_name.clone(),
                                band_index,
                            });
                        }
                    }
                    // Real guest: `cat` on a not-assembled array's sysfs
                    // path fails with exactly this stderr, exit 1. Match
                    // that shape specifically, not `Err(_)` -- `ArrayMissing`
                    // asserts a concrete cause ("not assembled"), and this is
                    // the only failure that shape actually is.
                    Err(ExecError::NonZeroExit { ref stderr, .. })
                        if stderr.contains("No such file or directory") =>
                    {
                        self.notify(&NotifyEvent::ArrayMissing {
                            group: group_name.clone(),
                            band_index,
                        });
                    }
                    // Any OTHER failure -- `degraded_count`'s own
                    // `Prerequisite` when `cat` succeeds but its stdout
                    // doesn't parse as a number (array IS assembled), or a
                    // `NonZeroExit` for a reason other than "no such file"
                    // (e.g. permission denied) -- is not evidence the array
                    // is missing. Notifying `ArrayMissing` here would be a
                    // false claim about a live machine's state (the
                    // mirror-image mistake: asserting a cause the evidence
                    // doesn't support). Still must not abort the tick (the
                    // actual defect) -- just skip this band's degraded
                    // check for this pass without asserting a cause. Not
                    // silent: `SystemRunner::run` already logs every
                    // command it executes, including this failing `cat`,
                    // with its exit code and args, before `degraded_count`
                    // ever wraps it into this `Err` (the audit trail,
                    // `crates/shr-exec/src/cmd.rs`'s `log_executed_command`
                    // -- visible via `RUST_LOG`/`journalctl` for the
                    // `internal health-check-tick` unit).
                    Err(_) => {}
                }

                // SMART worsening: reuses the SAME `last_smart_reallocated`
                // field/delta comparison the reshape throttle's emergency
                // brake already uses -- this is just a second,
                // independent caller of it outside an active reshape, so a
                // worsening SMART signal is caught even when nothing is
                // currently expanding.
                let member_disks = Self::band_member_disk_paths(&state, group_idx, band_index);
                let previous = state.groups[group_idx].bands[i].last_smart_reallocated;
                let sampler = LiveMetricsSampler::new(self.runner, member_disks, previous);
                if let Some(metrics) = sampler.sample() {
                    if let Some(delta) = metrics.smart_delta_reallocated {
                        if delta > 0 {
                            self.notify(&NotifyEvent::SmartWorsened {
                                group: group_name.clone(),
                                band_index,
                                reallocated_delta: delta,
                            });
                        }
                    }
                    if let Some(total) = sampler.last_smart_total() {
                        if state.groups[group_idx].bands[i].last_smart_reallocated != Some(total) {
                            state.groups[group_idx].bands[i].last_smart_reallocated = Some(total);
                            changed = true;
                        }
                    }
                }
            }
        }

        if changed && !self.runner.is_dry_run() {
            self.store.save(&state)?;
        }
        Ok(())
    }

    /// Start a scrub across every band of one group AND its Btrfs
    /// filesystem. `fd-lock` scope note: unlike `create`/`expand`,
    /// this is meant to be called with the CLI's exclusive `state.toml` lock
    /// held only around THIS call, not for the scrub's whole (potentially
    /// many-hour) duration -- a scrub is a long-running background kernel
    /// activity, not a multi-step transaction this process drives, so there
    /// is nothing here that a concurrent `create`/`expand` racing on
    /// `state.toml`'s tmp-write path would corrupt once the sysfs/`btrfs
    /// scrub start` commands have been issued. `scrub_status`/`scrub_cancel`
    /// likewise only need the lock for their own brief read/write, never for
    /// "as long as the scrub is running".
    ///
    /// `speed` sets the profile every band of this group runs its `check`
    /// under, from the same three `expand --priority` uses (`SyncPriority`
    /// is reused rather than cloned into a near-identical `ScrubPriority`:
    /// it is exactly the same question asked about a different kernel sync
    /// activity).
    ///
    /// `None` means "touch no kernel parameter at all", which is the default
    /// and the behavior every caller had before this existed -- whatever cap
    /// is already in place governs the scrub. That is a real choice the
    /// Cockpit dialog offers, so it stays.
    ///
    /// Both limits are written, not the ceiling alone: the kernel reduces
    /// the sync rate toward `sync_speed_min` whenever non-sync IO touches
    /// the members, so a scrub started with `--priority max` against a
    /// 1 MB/s floor was throttled anyway.
    pub fn scrub_start(
        &self,
        name: Option<&str>,
        speed: Option<SyncPriority>,
    ) -> Result<(), OrchestrateError> {
        let mut state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, name)?;
        let group_name = state.groups[group_idx].name.clone();
        let mount_point = state.groups[group_idx].filesystem.mount_point.clone();
        let md_names: Vec<String> = state.groups[group_idx]
            .bands
            .iter()
            .map(|b| b.md_name.clone())
            .collect();
        let mdadm = MdadmExecutor::new(self.runner);

        // Degraded defense completion -- a scrub verifies parity
        // against every member; a missing member leaves nothing to verify
        // against and only adds read load to an already-reduced array.
        //
        // (measured on real guest: weekly `fs scrub start` timer fired
        // while `mdadm --stop`ped for maintenance): `degraded_count` reading
        // `/sys/block/<md>/md/degraded` on an unassembled array fails as
        // `cat: ...: No such file or directory`, which used to propagate
        // verbatim via `?` -- a raw plumbing leak that never says the array
        // isn't assembled, unlike this guard's own message right below.
        // `Self::absent_array_error` narrows to exactly that ENOENT shape so
        // `degraded_count`'s OTHER failure mode (a parse error when `cat`
        // itself succeeded) still propagates unchanged -- that one is NOT
        // "array not assembled" and must not be relabeled as such.
        for band in &state.groups[group_idx].bands {
            let degraded = mdadm.degraded_count(&band.md_name).map_err(|e| {
                Self::absent_array_error(&e, &group_name, band.index, &band.md_name)
                    .unwrap_or(OrchestrateError::Exec(e))
            })?;
            if degraded > 0 {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) is degraded; scrub is blocked until it is healthy",
                    band.index, band.md_name
                )));
            }
        }

        // (scrub blocked while expanding). The other direction --
        // `expand` blocked while a scrub is running -- needs no new code:
        // `expand()`'s existing "band has background activity" guard
        // already rejects `sync_action == "check"` (a running scrub) the
        // same as `"reshape"`/`"resync"`, since it checks for anything
        // other than `"idle"`, not reshape specifically.
        if state.groups[group_idx].expansion.in_progress {
            return Err(OrchestrateError::Validation(format!(
                "group `{group_name}` has an expansion in progress; scrub is blocked until it finishes"
            )));
        }
        // Same ENOENT-leak fix as the degraded guard above --
        // `sync_action` reading an unassembled array's
        // `/sys/block/<md>/md/sync_action` fails the same way (measured:
        // `cat: ...: No such file or directory`).
        for band in &state.groups[group_idx].bands {
            let activity = mdadm.sync_action(&band.md_name).map_err(|e| {
                Self::absent_array_error(&e, &group_name, band.index, &band.md_name)
                    .unwrap_or(OrchestrateError::Exec(e))
            })?;
            if activity != "idle" {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) has background activity in progress (sync_action={activity}); \
                     scrub is blocked until it finishes",
                    band.index, band.md_name
                )));
            }
        }

        // Before the `check` threads start, so they run under the requested
        // limits from their first stripe rather than being re-capped a
        // moment later. Per band, because the limits are per array: two
        // groups scrubbing at different profiles at once each keep their
        // own, which the host-wide parameter could not express.
        if let Some(priority) = speed {
            for band_pos in 0..state.groups[group_idx].bands.len() {
                self.start_sync_throttle(&mut state, group_idx, band_pos, priority)?;
            }
            // Persisted before the checks start, not with the rest of the
            // bookkeeping below: the limits are already written at this
            // point, and a failure from here on would otherwise leave them
            // in force with nothing recording that this project put them
            // there -- so nothing would ever hand them back.
            if !self.runner.is_dry_run() {
                self.store.save(&state)?;
            }
        }

        for md_name in &md_names {
            mdadm.scrub_start(md_name)?;
        }
        BtrfsExecutor::new(self.runner).scrub_start(&mount_point)?;

        // Mark every band as having a scrub started HERE so `scrub_status`
        // can tell "this band just finished a real scrub" apart from "this
        // band has simply always been idle" once `sync_action` reads back
        // `idle` -- see `StateBand::scrub_in_progress`'s doc comment.
        for band in &mut state.groups[group_idx].bands {
            band.scrub_in_progress = true;
        }
        if !self.runner.is_dry_run() {
            self.store.save(&state)?;
        }

        self.progress.report(ProgressUpdate {
            operation: "scrub".to_string(),
            stage: "start".to_string(),
            percent: Some(0.0),
            message: format!("scrub started for group `{group_name}`"),
        });
        Ok(())
    }

    /// (measured on real guest: `mdadm --stop`ped array, weekly `fs
    /// scrub start` timer): turns `degraded_count`/`sync_action`'s ENOENT
    /// failure (`cat: /sys/block/<md>/md/<file>: No such file or
    /// directory`) into a message naming the actual condition -- the array
    /// isn't assembled -- instead of leaking that raw `cat` plumbing to the
    /// operator (or the timer's failure mail) the way the un-narrowed `?`
    /// used to. Returns `None` for anything else so the caller falls back to
    /// propagating `e` unchanged: `degraded_count` can also fail with
    /// `ExecError::Prerequisite` when `cat` SUCCEEDED but stdout didn't
    /// parse (array present, sysfs content just malformed) -- that is a
    /// different, genuine failure and must not be relabeled "not
    /// assembled". Gated on program `cat` AND the ENOENT stderr shape only,
    /// same precision as `is_missing_array_device`'s `mount`/"special
    /// device" pairing -- not exit code alone (`cat` reuses exit 1 for
    /// other read failures too, e.g. permission denied).
    fn absent_array_error(
        err: &ExecError,
        group_name: &str,
        band_index: u8,
        md_name: &str,
    ) -> Option<OrchestrateError> {
        match err {
            ExecError::NonZeroExit { program, stderr, .. }
                if program == "cat" && stderr.contains("No such file or directory") =>
            {
                Some(OrchestrateError::Validation(format!(
                    "group `{group_name}` band {band_index} ({md_name}): array is not assembled; \
                     reassemble it (`mdadm --assemble`) before scrubbing"
                )))
            }
            _ => None,
        }
    }

    /// Cancel a running (or already-finished) scrub. `mdadm`/`btrfs` each
    /// report their own error when there is nothing left on their side to
    /// cancel -- measured on real mdadm:
    /// Btrfs's half of a scrub routinely finishes long before mdadm's
    /// per-band `check` half does, so `btrfs scrub cancel` says "not
    /// running" as ORDINARY successful completion of that half, not caller
    /// misuse -- only this function knows it started two things with
    /// different lifetimes, so this is where that gets decided, not inside
    /// `BtrfsExecutor::scrub_cancel` (whose "surface it as-is" posture is
    /// correct for a caller cancelling exactly one thing). The same
    /// reasoning applies symmetrically to mdadm's own `idle` write. Neither
    /// tolerance hides a genuine failure (permission denied, device gone,
    /// `btrfs` missing) -- those are collected into `failures` and reported.
    /// No degraded/mutual-exclusion gate: cancelling must always be
    /// reachable, including on a degraded array, since refusing to cancel
    /// would leave an operator with no way to stop a scrub they need to stop.
    pub fn scrub_cancel(&self, name: Option<&str>) -> Result<(), OrchestrateError> {
        let mut state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, name)?;
        let mdadm = MdadmExecutor::new(self.runner);
        let btrfs = BtrfsExecutor::new(self.runner);
        let mount_point = state.groups[group_idx].filesystem.mount_point.clone();

        let mut failures: Vec<String> = Vec::new();
        for band in &state.groups[group_idx].bands {
            if let Err(e) = mdadm.scrub_cancel(&band.md_name) {
                // Rule 2 (trust kernel state, not an exit code): a write
                // that reports failure while `sync_action` already reads
                // back `idle` reached the desired end state regardless.
                match mdadm.sync_action(&band.md_name) {
                    Ok(action) if action == "idle" => {}
                    _ => failures.push(format!(
                        "band {} ({}) mdadm cancel: {e}",
                        band.index, band.md_name
                    )),
                }
            }
        }
        if let Err(e) = btrfs.scrub_cancel(&mount_point) {
            // Measured text: "ERROR: scrub cancel failed
            // on <mount>: not running". Exit code 2 alone is not
            // distinctive enough -- other btrfs failures share it -- so key
            // off the message btrfs-progs actually emits for this condition.
            let already_stopped =
                matches!(&e, ExecError::NonZeroExit { stderr, .. } if stderr.contains("not running"));
            if !already_stopped {
                failures.push(format!("btrfs cancel on {mount_point}: {e}"));
            }
        }

        // Persist what is ACTUALLY true now, regardless of which cancels
        // above reported success vs. a tolerated/genuine failure (the
        // original bug: the mdadm cancel above may have already succeeded even
        // though the function used to bail out on the btrfs error before
        // ever getting here), and regardless of whether THIS read-back
        // itself can be trusted (a bare `?` here reintroduced the same
        // early-exit one step later -- a read-back failure must not skip
        // `store.save` or swallow `failures` collected above). An unreadable
        // signal is never treated as "safe" (the rule): fall back to
        // `true` on a read error, matching the mdadm-cancel tolerance loop
        // above (`Ok(action) if action == "idle" => {}`, anything else --
        // including `Err` -- does not assume success), and record the read
        // failure itself into `failures` so it reaches the operator instead
        // of being dropped.
        let btrfs_running = match btrfs.scrub_status(&mount_point) {
            Ok(status) => status.running,
            Err(e) => {
                failures.push(format!("btrfs scrub status read-back on {mount_point}: {e}"));
                true
            }
        };
        for band in &mut state.groups[group_idx].bands {
            let mdadm_running = match mdadm.sync_action(&band.md_name) {
                Ok(action) => action != "idle",
                Err(e) => {
                    failures.push(format!(
                        "band {} ({}) sync_action read-back: {e}",
                        band.index, band.md_name
                    ));
                    true
                }
            };
            band.scrub_in_progress = mdadm_running || btrfs_running;
        }
        if !self.runner.is_dry_run() {
            self.store.save(&state)?;
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(OrchestrateError::Validation(format!(
                "scrub cancel did not fully stop everything: {}",
                failures.join("; ")
            )))
        }
    }

    /// Check on a scrub's progress and, if this group's scrub
    /// (started via `scrub_start`, tracked by `scrub_in_progress`) has just
    /// finished, persist the result -- every band's `sync_action` back
    /// to `idle` AND Btrfs no longer running. Never persists a result for a
    /// band that was never marked `scrub_in_progress` in the first place
    /// (see that field's doc comment): calling this on a group that has
    /// simply never been scrubbed must not fabricate a "0 errors, just
    /// completed" history entry.
    pub fn scrub_status(&self, name: Option<&str>) -> Result<ScrubReport, OrchestrateError> {
        let mut state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, name)?;
        let (running, error_count, actions) = self.reconcile_group_scrub(&mut state, group_idx)?;
        if !actions.is_empty() && !self.runner.is_dry_run() {
            self.store.save(&state)?;
        }
        Ok(ScrubReport {
            group_name: state.groups[group_idx].name.clone(),
            running,
            error_count,
        })
    }

    /// Recompress every file under a group's Btrfs filesystem via
    /// `btrfs filesystem defragment` -- Btrfs only ever applies a NEW
    /// compression setting to newly-written data, so existing extents need
    /// this to actually pick up a changed `create --compression`/policy.
    pub fn recompress(&self, name: Option<&str>, compression: &str) -> Result<(), OrchestrateError> {
        let mut state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, name)?;
        let mdadm = MdadmExecutor::new(self.runner);

        // Degraded defense completion -- recompress is maintenance,
        // not recovery; it adds read/write load an already-reduced array
        // doesn't need.
        for band in &state.groups[group_idx].bands {
            if mdadm.degraded_count(&band.md_name)? > 0 {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) is degraded; recompress is blocked until it is healthy",
                    band.index, band.md_name
                )));
            }
        }

        let btrfs = BtrfsExecutor::new(self.runner);
        let mount_point = state.groups[group_idx].filesystem.mount_point.clone();

        // Order matters. Btrfs only applies the mount option's
        // compression LEVEL to extents it rewrites at that moment -- the
        // `defragment -c` flag itself cannot carry a level (see
        // `BtrfsExecutor::recompress`'s doc comment for the real
        // btrfs-progs v6.12 rejection this fixes). So the new compression
        // must be live as a mount option BEFORE `defragment` runs, or the
        // rewritten extents pick up whatever level the OLD mount option
        // still had. Persist state.toml/fstab first (config-of-record),
        // then remount (the point where the real filesystem starts
        // honoring it), then defragment (the actual rewrite).
        let previous_compression = state.groups[group_idx].filesystem.compression.clone();
        state.groups[group_idx].filesystem.compression = compression.to_string();

        if !self.runner.is_dry_run() {
            if let Err(e) = self.store.save(&state) {
                // Nothing external has changed yet (no fstab write, no
                // remount) -- there is nothing to roll back, just report.
                return Err(e.into());
            }
            if let Err(e) = write_fstab(&self.fstab_path, &state) {
                // state.toml now disagrees with fstab/the live mount, and
                // neither has actually changed yet: revert state.toml so
                // "what's on disk" and "what state.toml claims" both still
                // say the OLD compression, matching the untouched live
                // mount.
                state.groups[group_idx].filesystem.compression = previous_compression;
                let _ = self.store.save(&state);
                return Err(e.into());
            }
        }

        // Same "executor calls run unconditionally, only state/fstab
        // bookkeeping is dry-run-gated" pattern `create()`'s `btrfs.mount`
        // call uses -- a `DryRunRunner` still needs to record this command.
        if let Err(e) = btrfs.remount_compress(&mount_point, compression) {
            // Remount itself never partially applies (mount either swaps
            // the option atomically or the call fails outright), so the
            // live filesystem is still on the OLD compression. Revert
            // state.toml/fstab to match it -- same reasoning as the
            // fstab-write-failure branch above.
            if !self.runner.is_dry_run() {
                state.groups[group_idx].filesystem.compression = previous_compression;
                let _ = self.store.save(&state);
                let _ = write_fstab(&self.fstab_path, &state);
            }
            return Err(e.into());
        }

        // Past this point the remount already committed the new
        // compression as the config-of-record AND as what the live
        // filesystem will use for every new/rewritten extent from now on --
        // a `defragment` failure here doesn't leave state.toml/fstab lying
        // about the live mount, it just means some already-existing
        // extents didn't get rewritten yet. Not rolled back.
        btrfs.recompress(&mount_point, compression)?;
        Ok(())
    }

    /// Create a read-only snapshot of a group's data at
    /// `@snapshots/<snapshot_name>` (the design's `fs snapshot
    /// create`). Every group's Btrfs filesystem carries the `@`/
    /// `@snapshots` layout since `create()` -- no legacy/single-root
    /// fallback needed.
    ///
    /// `@snapshots` is a SIBLING of `@`, not nested under it (`create()`'s
    /// layout) -- deliberately, so a snapshot of `@` never recursively
    /// contains earlier snapshots. That means it is NOT reachable from the
    /// group's real, ongoing `subvol=@` mount at all: reaching it needs the
    /// filesystem's default (top-level) subvolume mounted separately. This
    /// mounts that EPHEMERALLY, only for the duration of this one snapshot,
    /// at a scratch path under `/run` -- so a normal `fs snapshot create`
    /// never permanently changes the group's mount topology or fstab (no
    /// new managed block, no new mount to keep alive across reboots).
    pub fn snapshot_create(&self, name: Option<&str>, snapshot_name: &str) -> Result<(), OrchestrateError> {
        if snapshot_name.is_empty() || snapshot_name.contains('/') {
            return Err(OrchestrateError::Validation(format!(
                "invalid snapshot name `{snapshot_name}` -- must be non-empty and must not contain `/`"
            )));
        }
        // Reserve `AUTO_SNAPSHOT_PREFIX` exclusively for `snapshot_
        // auto_run`'s own automated snapshots -- see that constant's doc
        // comment for why pruning depends on this namespace split being
        // absolute, not a heuristic. Rejected here, at creation time, so
        // the split can never be violated by accident later. Only THIS
        // operator-facing entrypoint enforces it -- `snapshot_auto_run`
        // calls `create_snapshot_now` directly, bypassing this check,
        // because using the reserved prefix is exactly what it is
        // supposed to do.
        if snapshot_name.starts_with(AUTO_SNAPSHOT_PREFIX) {
            return Err(OrchestrateError::Validation(format!(
                "snapshot name `{snapshot_name}` starts with the reserved `{AUTO_SNAPSHOT_PREFIX}` \
                 prefix (reserved for shr-rs's own scheduled snapshot automation) -- choose a \
                 different name"
            )));
        }
        self.create_snapshot_now(name, snapshot_name)
    }

    /// The actual mount/snapshot/unmount sequence, shared by `snapshot_
    /// create` (after it validates the name and rejects the reserved
    /// prefix) and `snapshot_auto_run` (which deliberately uses that same
    /// reserved prefix for its own automated snapshots -- see `snapshot_
    /// create`'s doc comment for why calling this directly, instead of
    /// through `snapshot_create`, is correct here and not a bypass).
    fn create_snapshot_now(&self, name: Option<&str>, snapshot_name: &str) -> Result<(), OrchestrateError> {
        let state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, name)?;

        let filesystem = &state.groups[group_idx].filesystem;
        let dev_path = format!("/dev/{}/{}", filesystem.vg_name, filesystem.lv_name);
        let btrfs = BtrfsExecutor::new(self.runner);
        let scratch = format!("/run/shr-rs/snapshot-mount-{}", state.groups[group_idx].name);

        // Route through `self.runner`, not raw `std::fs` -- see `create()`
        // for why the `!is_dry_run()` guard this replaced was not test-safe.
        self.runner.run("mkdir", &["-p", &scratch])?;
        btrfs.mount(&dev_path, &scratch, Some(&filesystem.compression), None)?;

        let group_name = state.groups[group_idx].name.clone();
        let snapshot_result = (|| -> Result<(), OrchestrateError> {
            // A name that is already taken must be rejected here, by name.
            // `btrfs subvolume snapshot -r <src> <dest>` treats an EXISTING
            // `dest` as the parent directory to create the new subvolume
            // inside, and every snapshot here is read-only, so btrfs
            // otherwise reports `Could not create subvolume: Read-only file
            // system` -- true, but naming neither the cause nor the
            // operator's own input (seen on a real guest through Cockpit).
            // It cannot be checked before the mount above: `@snapshots` is
            // a sibling of `@` and is unreachable from the group's own
            // `subvol=@` mount.
            let existing = btrfs.list_snapshot_names(&format!("{scratch}/@snapshots"))?;
            if existing
                .iter()
                .any(|existing_name| existing_name == snapshot_name)
            {
                return Err(OrchestrateError::Validation(format!(
                    "snapshot `{snapshot_name}` already exists in group `{group_name}` -- choose a \
                     different name, or delete the existing one first"
                )));
            }
            btrfs.create_snapshot(
                &format!("{scratch}/@"),
                &format!("{scratch}/@snapshots/{snapshot_name}"),
            )?;
            Ok(())
        })();
        // Always attempt to unmount the scratch mount, even if the
        // snapshot itself failed -- leaving it mounted on error would be a
        // silent resource leak an operator has to discover by hand. The
        // snapshot's own error takes priority when both fail; the unmount
        // failure alone still surfaces if the snapshot succeeded.
        let unmount_result = btrfs.unmount(&scratch);
        snapshot_result?;
        unmount_result?;
        Ok(())
    }

    /// Run scheduled snapshot automation across EVERY group --
    /// create one new automated snapshot per group, then prune that
    /// group's own older automated snapshots down to `keep`. Invoked by
    /// `shr-rs internal snapshot-auto-tick`, the periodic entrypoint
    /// `shr-rs-snapshot-auto.timer` (`schedule install`, only when
    /// `policy.toml`'s `[snapshot].enabled` is `true`) fires on
    /// `[snapshot].schedule`.
    ///
    /// `AUTO_SNAPSHOT_PREFIX` is how "ours" is identified for pruning:
    /// every automated snapshot's name starts with it, `snapshot_create`
    /// refuses to let an operator manually create one under it (see that
    /// function's own check), so the two namespaces can never collide --
    /// a snapshot bearing this prefix is provably ours, and nothing else
    /// ever is. This is the same reasoning as `is_shr_rs_owned_unit`'s
    /// marker for systemd units: pruning must never guess, only
    /// recognize what it can prove.
    ///
    /// A host with no active array at all is a silent no-op (matches
    /// `check_health`'s "nothing to do" shape for the same periodic-timer
    /// class of caller), not an error -- a policy that's `enabled` before
    /// the first `create()` ever runs must not make the timer fail loudly
    /// every tick until one exists.
    ///
    /// The same "nothing to do here yet" reasoning extends to a
    /// SINGLE group whose `state.toml` entry has outlived its array (real-
    /// guest repro: unplanned power-cycle, array not yet reassembled) --
    /// `is_missing_array_device` below is the ONLY failure this tolerates;
    /// that group is recorded as skipped in the returned summary (never
    /// silently dropped -- a skipped BACKUP must be visible to whoever
    /// reads `shr-rs internal snapshot-auto-tick`'s output, not just
    /// invisible success), and every OTHER group still gets its snapshot --
    /// one group's missing LV must not cost the rest of the host its
    /// backups for the tick. Any OTHER failure shape still aborts the
    /// whole tick, same as before this fix.
    pub fn snapshot_auto_run(&self, keep: u32) -> Result<Vec<String>, OrchestrateError> {
        let Some(state) = self.store.load()? else {
            return Ok(Vec::new());
        };
        let group_names: Vec<String> = state.groups.iter().map(|g| g.name.clone()).collect();

        let mut summary = Vec::new();
        for group_name in group_names {
            let auto_name = format!("{AUTO_SNAPSHOT_PREFIX}{}", Utc::now().format("%Y%m%dT%H%M%SZ"));

            if let Err(err) = self.create_snapshot_now(Some(&group_name), &auto_name) {
                if !Self::is_missing_array_device(&err) {
                    return Err(err);
                }
                summary.push(format!(
                    "group `{group_name}`: SKIPPED (array/LV not present -- {err})"
                ));
                continue;
            }

            let pruned = match self.prune_group_snapshots(&group_name, keep) {
                Ok(pruned) => pruned,
                // Same tolerance as above, for the same reason -- unlikely
                // (the mount just above succeeded against this exact
                // device) but not impossible if the array vanishes mid-tick.
                Err(err) if Self::is_missing_array_device(&err) => {
                    summary.push(format!(
                        "group `{group_name}`: created `{auto_name}`, but pruning was SKIPPED \
                         (array/LV not present -- {err})"
                    ));
                    continue;
                }
                Err(err) => return Err(err),
            };
            summary.push(format!(
                "group `{group_name}`: created `{auto_name}`, pruned {pruned} old auto-snapshot(s)"
            ));
        }
        Ok(summary)
    }

    /// True iff `err` is
    /// `mount` failing with EXACTLY the "the device node itself doesn't
    /// exist" shape -- stderr contains BOTH `"special device"` AND `"does
    /// not exist"`, e.g. `mount: /tmp/mp1: special device
    /// /dev/shr_vg/nosuchlv does not exist.` (exit 32).
    ///
    /// Deliberately does NOT match on `"does not exist"` alone: real mount
    /// also emits `mount: <point>: mount point does not exist.` (also exit
    /// 32) when the SCRATCH DIRECTORY itself is missing/unusable --
    /// `create_snapshot_now`/`prune_group_snapshots` both `mkdir -p` that
    /// directory immediately before this `mount` call, so a stale
    /// non-directory at that path, a read-only `/run`, or tmpfs pressure
    /// produces exactly that message. That is a genuine, actionable
    /// backup failure, not "array not assembled yet" -- treating it as the
    /// latter would silently reclassify a real failure as an expected
    /// no-op in the one function whose entire job is taking backups. Only
    /// the `"special device ... does not exist"` pair is unambiguous
    /// enough to tolerate; do not loosen this back to a single substring
    /// (see `mount_point_missing_is_not_treated_as_absent_array`).
    ///
    /// Not gated on exit code 32 alone: mount(8) reuses 32 for many
    /// unrelated failure classes (bad fs, busy, wrong options, ...), none
    /// of which this tolerance is meant to cover.
    fn is_missing_array_device(err: &OrchestrateError) -> bool {
        matches!(
            err,
            OrchestrateError::Exec(ExecError::NonZeroExit { program, stderr, .. })
                if program == "mount" && stderr.contains("special device") && stderr.contains("does not exist")
        )
    }

    /// Delete `group_name`'s own oldest `AUTO_SNAPSHOT_PREFIX`-named
    /// snapshots beyond the newest `keep` -- NEVER a snapshot `fs snapshot
    /// create` made by hand, regardless of its name (the prefix check is
    /// the only thing that matters here). The auto-generated name embeds a
    /// UTC timestamp in a lexicographically sortable form
    /// (`YYYYMMDDTHHMMSSZ`), so a plain string sort is already oldest-
    /// first -- no need to parse each name back into a real timestamp.
    fn prune_group_snapshots(&self, group_name: &str, keep: u32) -> Result<usize, OrchestrateError> {
        let state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, Some(group_name))?;
        let filesystem = &state.groups[group_idx].filesystem;
        let dev_path = format!("/dev/{}/{}", filesystem.vg_name, filesystem.lv_name);
        let btrfs = BtrfsExecutor::new(self.runner);
        let scratch = format!("/run/shr-rs/snapshot-mount-{group_name}");

        // Same fix as `create_snapshot_now` above.
        self.runner.run("mkdir", &["-p", &scratch])?;
        btrfs.mount(&dev_path, &scratch, Some(&filesystem.compression), None)?;
        let snapshots_dir = format!("{scratch}/@snapshots");

        let prune_result = btrfs.list_snapshot_names(&snapshots_dir).and_then(|names| {
            let mut ours: Vec<String> = names
                .into_iter()
                .filter(|n| n.starts_with(AUTO_SNAPSHOT_PREFIX))
                .collect();
            ours.sort(); // oldest first -- see this method's doc comment
            let excess = ours.len().saturating_sub(keep as usize);
            let mut deleted = 0usize;
            for name in &ours[..excess] {
                btrfs.delete_subvolume(&format!("{snapshots_dir}/{name}"))?;
                deleted += 1;
            }
            Ok(deleted)
        });

        // Same "always attempt the unmount, snapshot/prune error wins if
        // both fail" shape as `snapshot_create` above.
        let unmount_result = btrfs.unmount(&scratch);
        let deleted = prune_result?;
        unmount_result?;
        Ok(deleted)
    }

    /// Replace one member disk of a group with another. the design
    /// safety table: the replacement must be an EQUAL-OR-LARGER disk --
    /// unlike `expand`, this doesn't reshape/re-plan anything, it just
    /// re-partitions the new disk to the OLD disk's exact existing
    /// geometry (every band it was a member of) and hands each band's
    /// mdadm array a live `--replace ... --with ...`, which keeps the band
    /// at full redundancy throughout the copy where mdadm is able to.
    ///
    /// `old_disk_id` must currently be a real member of the target group;
    /// `new_disk` must not already belong to ANY group and must not be a
    /// system disk (same D3/D4-style checks `create`/`expand` run).
    ///
    /// `old_disk_id` accepts by-id (`state.toml`'s recorded `id`) or
    /// serial (`state.toml`'s recorded `serial`, fragment match) with NO live
    /// system call -- this is the primary path, because the disk being
    /// replaced is very often already physically failed/removed, and a live
    /// probe can never see it. A kernel name (`sdc`, `/dev/sdc`) only works
    /// as a fallback: it's resolved by `readlink -e`-ing each member's
    /// `/dev/disk/by-id/<id>` symlink until one points at the requested
    /// kernel name, which by construction requires the disk to still be
    /// physically present and enumerable -- it can never help find an
    /// already-gone disk, which is why it isn't the primary match.
    pub fn replace_disk(
        &self,
        name: Option<&str>,
        old_disk_id: &str,
        new_disk: &ResolvedDisk,
        system_disks: &[String],
    ) -> Result<ArrayState, OrchestrateError> {
        let mut state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&state, name)?;

        let old_disk = Self::find_disk_by_reference(&state.groups[group_idx].disks, old_disk_id)
            .or_else(|| {
                find_disk_by_live_kernel_name(self.runner, &state.groups[group_idx].disks, old_disk_id)
            })
            .cloned()
            .ok_or_else(|| {
                OrchestrateError::Validation(format!(
                    "disk `{old_disk_id}` is not a member of group `{}` -- --old accepts the \
                     disk's by-id name or serial (matched against recorded state, so this also \
                     works once the disk has physically failed) or its current kernel name (only \
                     while still visible to the system); `shr-rs disk list` only shows LIVE disks \
                     and omits an already-failed one, so run `shr-rs groups --json` instead to see \
                     every recorded member's `id`/`serial`, failed or not",
                    state.groups[group_idx].name
                ))
            })?;

        // Compare against `old_disk.id` (the resolved by-id), not the
        // raw `old_disk_id` reference -- the latter may be a serial or
        // kernel-name alias, which would never equal `new_disk.id` (always
        // a by-id) even when they name the same physical disk.
        if new_disk.id.as_str() == old_disk.id {
            return Err(OrchestrateError::Validation(
                "replacement disk must be different from the disk being replaced".to_string(),
            ));
        }
        let disks_in_any_group: HashSet<&str> = state
            .groups
            .iter()
            .flat_map(|g| g.disks.iter())
            .map(|d| d.id.as_str())
            .collect();
        if disks_in_any_group.contains(new_disk.id.as_str()) {
            return Err(OrchestrateError::Validation(format!(
                "disk `{}` already belongs to an existing group; a disk can only be a member of \
                 one group at a time",
                new_disk.id.as_str()
            )));
        }
        let kernel_path = format!("/dev/{}", new_disk.kernel_name);
        SafetyGuard::validate_disk_target(&kernel_path, system_disks)?;
        let by_id_path = resolve_disk_path(&new_disk.id).display().to_string();
        SafetyGuard::validate_disk_target(&by_id_path, system_disks)?;

        // The design safety table: replace only accepts an equal-or-larger disk.
        if new_disk.size_bytes < old_disk.size_bytes {
            return Err(OrchestrateError::Validation(format!(
                "replacement disk `{}` ({} bytes) is smaller than `{old_disk_id}` ({} bytes); \
                 replace requires an equal-or-larger disk",
                new_disk.id.as_str(),
                new_disk.size_bytes,
                old_disk.size_bytes
            )));
        }

        let mdadm = MdadmExecutor::new(self.runner);
        // Replace is
        // often exactly how an operator responds to `old_disk` having
        // already failed, so a blanket "any degraded band blocks replace"
        // guard would block the very recovery it exists for. Per band
        // `old_disk` is a member of: block ONLY if a member OTHER than
        // `old_disk` is also down -- that leaves the array with no
        // redundancy margin left for a further failure during the
        // (potentially many-hour) rebuild/copy, which IS still worth
        // blocking regardless of whether `old_disk` itself is fine or also
        // down. Also records, per band, whether `old_disk`'s own member is
        // healthy -- the partition-copy loop below needs that to choose
        // `--replace` (old alive, keeps redundancy during the copy) vs
        // `--add` (old already gone, this is a rebuild, not a live swap).
        let mut old_member_healthy_by_band: HashMap<u8, bool> = HashMap::new();
        for partition in &old_disk.partitions {
            let band = state.groups[group_idx]
                .bands
                .iter()
                .find(|b| b.index == partition.band_index)
                .ok_or_else(|| {
                    OrchestrateError::Validation(format!(
                        "band {} referenced by `{old_disk_id}`'s partition table no longer exists",
                        partition.band_index
                    ))
                })?;
            let degraded = mdadm.degraded_count(&band.md_name)?;
            // Nothing in the band is down at all -- `old_disk` must be the
            // healthy case. Skip resolving its kernel name/reading
            // `/proc/mdstat` entirely: this also makes the whole check a
            // no-op under dry-run, where `degraded_count` always reads 0.
            let old_is_healthy = if degraded == 0 {
                true
            } else {
                let old_member_path = format!("/dev/disk/by-partuuid/{}", partition.part_uuid);
                match mdadm.resolve_member_kernel_name(&old_member_path)? {
                    None => false, // disk physically gone: dangling by-partuuid symlink
                    Some(kernel_name) => {
                        let mdstat_output = self.runner.run("cat", &["/proc/mdstat"])?;
                        let mdstat = parse_mdstat(&mdstat_output.stdout);
                        mdstat
                            .arrays
                            .iter()
                            .find(|a| a.name == band.md_name)
                            .and_then(|a| a.members.iter().find(|m| m.name == kernel_name))
                            .map(|m| !m.faulty)
                            .unwrap_or(false) // absent from the member list at all: also gone
                    }
                }
            };
            let other_failures = if old_is_healthy {
                degraded
            } else {
                degraded.saturating_sub(1)
            };
            if other_failures > 0 {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) has a failed member OTHER than `{old_disk_id}`; replace is \
                     blocked until it is healthy -- proceeding would leave the array with no \
                     redundancy margin left for a further failure during the rebuild",
                    band.index, band.md_name
                )));
            }
            old_member_healthy_by_band.insert(band.index, old_is_healthy);
        }

        if !self.runner.is_dry_run() {
            let decision = self.confirm.confirm(&ConfirmRequest {
                operation: "replace".to_string(),
                summary: format!(
                    "replace disk `{old_disk_id}` with `{}` in group `{}`; this will partition \
                     the new disk and rebuild every band `{old_disk_id}` was a member of",
                    new_disk.id.as_str(),
                    state.groups[group_idx].name
                ),
                irreversible: true,
            });
            if decision == Confirmation::Reject {
                return Err(OrchestrateError::Rejected(format!(
                    "replace of `{old_disk_id}` was rejected via ConfirmSink before touching any disk"
                )));
            }
            // See create()'s identical call for the rationale.
            self.reverify_targets(std::slice::from_ref(new_disk))?;
        }

        let parted = PartedExecutor::new(self.runner);
        parted.ensure_supported()?;
        mdadm.ensure_supported()?;

        let new_disk_path = resolve_disk_path(&new_disk.id).display().to_string();
        parted.create_gpt(&new_disk_path)?;

        // Per-band notes on an old member `mdadm --replace` left
        // attached (either because cleanup couldn't run yet or because it
        // ran and failed) -- turned into a hard error at the end of this
        // function, AFTER state.toml/mdadm.conf/fstab are saved: the
        // physical replacement already happened for real by that point, so
        // recorded state must stay correct even when this call still fails
        // loudly for the operator.
        let mut stuck_removals: Vec<String> = Vec::new();
        let mut new_partitions = Vec::new();
        // Old_part_uuid -> new_part_uuid, one pair per partition this
        // disk had -- used below to rewrite every band's `member_partitions`
        // in place (see the loop after this one for why that's required).
        let mut part_uuid_map: Vec<(String, String)> = Vec::new();
        for partition in &old_disk.partitions {
            let band_pos = state.groups[group_idx]
                .bands
                .iter()
                .position(|b| b.index == partition.band_index)
                .ok_or_else(|| {
                    OrchestrateError::Validation(format!(
                        "band {} referenced by `{old_disk_id}`'s partition table no longer exists",
                        partition.band_index
                    ))
                })?;
            let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
            let part_num = (partition.band_index + 1) as u32;
            let end = partition.offset_bytes + partition.size_bytes - 1;

            parted.add_partition(&new_disk_path, partition.offset_bytes, end)?;
            parted.set_raid_flag(&new_disk_path, part_num)?;
            let new_part_path = parted.partition_path_for_read(&new_disk_path, part_num);
            let new_part_uuid = parted.read_partuuid(&new_part_path)?;
            parted.settle_udev()?;

            let old_member_path = format!("/dev/disk/by-partuuid/{}", partition.part_uuid);
            let new_member_path = format!("/dev/disk/by-partuuid/{new_part_uuid}");
            // Point of no return: mdadm now owns the copy/rebuild from old to new.
            let old_is_healthy = *old_member_healthy_by_band
                .get(&partition.band_index)
                .unwrap_or(&true);
            if old_is_healthy {
                // Mdadm's `--replace old --with new`
                // requires `new` to ALREADY be a member (spare) of the
                // array before it can be designated the preferred
                // replacement target -- issuing `--with` against a device
                // mdadm has never seen fails outright ("not found in <md>
                // so cannot make it preferred replacement"), exit code 1.
                // `--add` first, the exact same primitive the
                // already-failed-old branch below already uses for its own
                // rebuild.
                mdadm.add_member(&md_name, &new_member_path)?;
                if let Err(e) = mdadm.replace_member(&md_name, &old_member_path, &new_member_path) {
                    // mdadm marks `old_member_path` `want_replacement`
                    // BEFORE it even validates `new_member_path`, so a
                    // failure here can leave that flag set with no clean
                    // sysfs-level way for this engine to verify it's safe
                    // to clear blind. Best-effort undo only the spare THIS
                    // call just added (`add_member` above); surface BOTH
                    // outcomes so the operator knows exactly what's left to
                    // inspect by hand, instead of silently assuming either
                    // side succeeded.
                    let undo = mdadm.remove_member(&md_name, &new_member_path);
                    return Err(OrchestrateError::Validation(format!(
                        "mdadm --replace {old_member_path} --with {new_member_path} on {md_name} \
                         failed: {e}; `{old_member_path}` may still be marked `want_replacement` \
                         in the array's internal state -- inspect `mdadm --detail {md_name}` and \
                         `/sys/block/{md_name}/md/dev-*/state` by hand before retrying.{}",
                        match undo {
                            Ok(()) => String::new(),
                            Err(undo_err) => format!(
                                " Additionally, removing the spare `{new_member_path}` this call \
                                 added also failed: {undo_err}; it may still be attached to the \
                                 array as an unused spare."
                            ),
                        }
                    )));
                }

                // `--replace` only STARTS the background copy -- the
                // kernel marks `old_member_path` faulty and stops using it
                // once the copy finishes, but never detaches it; that always
                // needs a separate `--remove`, or the old member stays a
                // live (if faulty) part of the array forever, degraded.md
                // never fires (kernel still reports it in-sync), and the
                // disk can't be reused elsewhere. Only safe to issue that
                // `--remove` once `sync_action` is ALREADY back to `idle`
                // right now -- if it isn't, the copy this call just started
                // is still running and `old_member_path` is its live
                // source; removing it now would be wrong. This makes
                // cleanup synchronous for the common repro (tiny disks,
                // copy finishes before this command returns) and deferred
                // -- reported, not silently dropped -- for a real disk
                // whose copy is still running when this command returns.
                if mdadm.sync_action(&md_name)? == "idle" {
                    let removed = match mdadm.remove_member(&md_name, &old_member_path) {
                        Ok(()) => true,
                        Err(e) => {
                            stuck_removals.push(format!(
                                "band {} ({md_name}): old member `{old_member_path}` is still \
                                 attached after its replace copy finished, and `mdadm --remove` \
                                 failed: {e} -- inspect `mdadm --detail {md_name}` and remove it \
                                 by hand",
                                partition.band_index
                            ));
                            false
                        }
                    };
                    // Trust the kernel, not the exit code: verify it's
                    // actually gone before treating this as done -- via
                    // array MEMBERSHIP, not `readlink` alone (a
                    // removed member's partition still exists, so its
                    // by-partuuid symlink keeps resolving).
                    if removed && self.member_still_in_array(&mdadm, &md_name, &old_member_path)? {
                        stuck_removals.push(format!(
                            "band {} ({md_name}): `mdadm --remove {old_member_path}` exited \
                             successfully but the kernel still shows it attached to {md_name} -- \
                             inspect `mdadm --detail {md_name}` by hand before reusing the disk",
                            partition.band_index
                        ));
                    }
                } else {
                    // Record exactly which device is left to clean up
                    // so `reconcile()`/`check_health()` can finish this
                    // later without an operator having to remember (the
                    // prior earlier fix only ever reported this in a
                    // `ProgressSink` message nothing consumed -- see
                    // `reconcile_pending_member_removals`'s doc comment).
                    state.groups[group_idx].bands[band_pos].pending_member_removal =
                        Some(old_member_path.clone());
                    self.progress.report(ProgressUpdate {
                        operation: "replace".to_string(),
                        stage: "cleanup-pending".to_string(),
                        percent: None,
                        message: format!(
                            "band {} ({md_name}): the copy onto the new disk is still running, so \
                             the old disk `{old_member_path}` stays attached for now. It is \
                             removed automatically once the copy finishes -- by the scheduled \
                             health check, or immediately with `shr-rs reconcile`",
                            partition.band_index
                        ),
                    });
                }
            } else {
                // `--replace` requires `old` to be a live member --
                // it already isn't, so this is a rebuild, not a live swap.
                // `old` may still occupy a slot marked faulty (as opposed
                // to already fully removed), which mdadm requires cleared
                // via `--remove` before a spare can rebuild into it;
                // `--remove` on a device already gone entirely is a
                // harmless no-op mdadm accepts, so this is safe to run
                // unconditionally rather than branching on which sub-case
                // it is.
                let _ = mdadm.remove_member(&md_name, &old_member_path);
                mdadm.add_member(&md_name, &new_member_path)?;
            }

            part_uuid_map.push((partition.part_uuid.clone(), new_part_uuid.clone()));
            new_partitions.push(StatePartition {
                part_uuid: new_part_uuid,
                offset_bytes: partition.offset_bytes,
                size_bytes: partition.size_bytes,
                band_index: partition.band_index,
            });
        }

        // Commit the new disk's identity immediately (an earlier review finding
        // rationale, same as `execute_grow`): the first `--replace` above
        // already crossed the point of no return.
        // Match on `old_disk.id` (the resolved by-id, always a real
        // `StateDisk.id`), not the raw `old_disk_id` reference -- a
        // serial/kernel-name alias would never be found here.
        let disk_pos = state.groups[group_idx]
            .disks
            .iter()
            .position(|d| d.id == old_disk.id)
            .expect("checked above");
        state.groups[group_idx].disks[disk_pos] = StateDisk {
            id: new_disk.id.as_str().to_string(),
            size_bytes: new_disk.size_bytes,
            serial: (!new_disk.serial.is_empty()).then(|| new_disk.serial.clone()),
            model: (!new_disk.model.is_empty()).then(|| new_disk.model.clone()),
            added_at: Utc::now().to_rfc3339(),
            partitions: new_partitions,
        };
        // The DISK's own `partitions` list is now
        // correct, but every BAND's `member_partitions` list still names the
        // OLD part_uuid -- which no longer exists on ANY disk once the
        // physical replacement above went through. `status` never notices
        // (it reads the live kernel, and the array genuinely is healthy),
        // but anything that rebuilds the logical layout from state.toml
        // (`snapshot_from_state`, used by `expand`) resolves each
        // `member_partitions` entry back to its owning disk and fails
        // outright the moment it can't find one. Rewrite IN PLACE, not
        // push/sort/dedup -- member order mirrors mdadm's device order.
        for band in state.groups[group_idx].bands.iter_mut() {
            for uuid in band.member_partitions.iter_mut() {
                if let Some((_, new_uuid)) = part_uuid_map.iter().find(|(old, _)| old == uuid) {
                    *uuid = new_uuid.clone();
                }
            }
        }
        if !self.runner.is_dry_run() {
            // `mdadm --replace` copies onto the new member as a `recover`,
            // which used to run under no profile at all. Non-fatal for the
            // same reason as `create`'s: the replacement itself succeeded.
            for band_pos in 0..state.groups[group_idx].bands.len() {
                // Membership changed, so a capability learned on the old
                // one describes a different array.
                Self::discard_capability_estimate(&mut state.groups[group_idx].bands[band_pos]);
                if let Err(e) = self.govern_running_sync(&mut state, group_idx, band_pos) {
                    tracing::warn!(target: "shr_rs::throttle", "band {band_pos} sync limits: {e}");
                }
            }
            self.store.save(&state)?;
            self.write_managed_configs(&state)?;
        }
        // Recorded state is correct at this point regardless -- this
        // still fails loudly (state.toml already saved above) when the
        // kernel disagrees, per this project's "trust actual kernel state
        // over recorded state" rule: reporting plain success here would
        // repeat the exact defect this fix exists for.
        if !stuck_removals.is_empty() {
            return Err(OrchestrateError::Validation(format!(
                "disk `{old_disk_id}` was replaced with `{}` and the shr-rs configuration, \
                 mdadm.conf and fstab already reflect it, but the OLD disk could not be \
                 confirmed removed from the live array: {}",
                new_disk.id.as_str(),
                stuck_removals.join("; ")
            )));
        }
        Ok(state.groups[group_idx].clone())
    }

    /// Tear down one group entirely: unmount its filesystem, remove
    /// its LV/VG/PVs, stop every band's mdadm array, optionally zero each
    /// member's superblock, then strip its entries from `mdadm.conf`/
    /// `fstab`/`state.toml` -- the reverse of `create()`. Manual teardown
    /// (an operator running `mdadm --stop`/`wipefs` by hand) leaves exactly
    /// those `mdadm.conf`/`fstab`/`state.toml` entries behind as orphans;
    /// this is what actually removes them.
    ///
    /// Multi-group correctness (same trap `write_managed_configs`'s doc
    /// comment describes): only `name`'s entries are dropped from the
    /// in-memory `StateFile` before `write_managed_configs` regenerates
    /// BOTH files from the resulting (smaller) `StateFile` -- since those
    /// writers always flatten across EVERY remaining group, every OTHER
    /// group's `ARRAY`/fstab line survives untouched by construction, not
    /// because this function special-cases them.
    ///
    /// `zero_superblocks`: also wipes each member partition's mdadm
    /// superblock after stopping its array, so a stopped-but-still-tagged
    /// partition can never be auto-reassembled by a later `mdadm --assemble
    /// --scan` (e.g. at the next boot) into an array `state.toml` no longer
    /// knows about. Optional (the design leaves this to the operator/caller
    /// -- `shr-cli` decides the default): a partition still readable as
    /// "used to be part of an mdadm array" is not itself a safety problem,
    /// only a residue an operator may want to leave alone if they intend to
    /// re-inspect the disk before reusing it.
    ///
    /// Every executor call below is collected into `failures` rather than
    /// aborting on the first one (mirrors `rollback`'s own "keep going,
    /// report everything that didn't work" shape) -- SO THAT one band's
    /// `mdadm --stop` failing doesn't stop later bands from also being
    /// attempted. If ANYTHING failed, `state.toml`/`mdadm.conf`/`fstab` are
    /// left COMPLETELY UNCHANGED (still listing this group): a partial
    /// teardown that still dropped the bookkeeping would create exactly the
    /// kind of live-array/no-record orphan this function exists to prevent,
    /// just in the opposite direction.
    pub fn destroy(&self, name: Option<&str>, zero_superblocks: bool) -> Result<(), OrchestrateError> {
        let mut full_state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&full_state, name)?;
        let group_name = full_state.groups[group_idx].name.clone();

        if full_state.groups[group_idx].expansion.in_progress {
            return Err(OrchestrateError::Validation(format!(
                "group `{group_name}` has an expansion in progress; resolve it (resume it or \
                 inspect manually) before destroying the group"
            )));
        }

        // Refuse while any band has background activity, same reasoning as
        // `scrub_start`/`expand`'s equivalent guards -- reading through
        // `self.status_runner()` so a `preview_destroy` given a real
        // status runner sees the REAL answer, not `DryRunRunner`'s
        // fabricated "idle".
        let status_mdadm = MdadmExecutor::new(self.status_runner());
        for band in &full_state.groups[group_idx].bands {
            let activity = status_mdadm.sync_action(&band.md_name)?;
            if activity != "idle" {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) has background activity in progress (sync_action={activity}); \
                     destroy is blocked until it finishes",
                    band.index, band.md_name
                )));
            }
        }

        if !self.runner.is_dry_run() {
            let decision = self.confirm.confirm(&ConfirmRequest {
                operation: "destroy".to_string(),
                summary: format!(
                    "destroy group `{group_name}` ({} band(s), mount `{}`): unmounts the \
                     filesystem, removes its LV/VG/PVs, stops every mdadm array, and \
                     permanently deletes its data",
                    full_state.groups[group_idx].bands.len(),
                    full_state.groups[group_idx].filesystem.mount_point
                ),
                irreversible: true,
            });
            if decision == Confirmation::Reject {
                return Err(OrchestrateError::Rejected(format!(
                    "destroy of group `{group_name}` was rejected via ConfirmSink before \
                     touching anything"
                )));
            }
        }

        let btrfs = BtrfsExecutor::new(self.runner);
        let lvm = LvmExecutor::new(self.runner);
        let mdadm = MdadmExecutor::new(self.runner);
        let mut failures: Vec<String> = Vec::new();

        self.progress.report(ProgressUpdate {
            operation: "destroy".to_string(),
            stage: "unmount".to_string(),
            percent: Some(0.0),
            message: format!(
                "unmounting `{}`",
                full_state.groups[group_idx].filesystem.mount_point
            ),
        });
        if let Err(e) = btrfs.unmount(&full_state.groups[group_idx].filesystem.mount_point) {
            failures.push(format!(
                "unmount {}: {e}",
                full_state.groups[group_idx].filesystem.mount_point
            ));
        }

        let vg_name = full_state.groups[group_idx].filesystem.vg_name.clone();
        let lv_name = full_state.groups[group_idx].filesystem.lv_name.clone();
        let lv_path = format!("/dev/{vg_name}/{lv_name}");
        if let Err(e) = lvm.lvremove(&lv_path) {
            failures.push(format!("lvremove {lv_path}: {e}"));
        }
        if let Err(e) = lvm.vgremove(&vg_name) {
            failures.push(format!("vgremove {vg_name}: {e}"));
        }

        self.progress.report(ProgressUpdate {
            operation: "destroy".to_string(),
            stage: "arrays".to_string(),
            percent: Some(50.0),
            message: format!(
                "stopping {} RAID array(s)",
                full_state.groups[group_idx].bands.len()
            ),
        });
        for band in &full_state.groups[group_idx].bands {
            let md_dev_path = format!("/dev/{}", band.md_name);
            if let Err(e) = lvm.pvremove(&md_dev_path) {
                failures.push(format!("pvremove {md_dev_path}: {e}"));
            }
            if let Err(e) = mdadm.stop_array(&band.md_name) {
                failures.push(format!("stop {}: {e}", band.md_name));
            }
            if zero_superblocks {
                let member_paths: Vec<String> = full_state.groups[group_idx]
                    .disks
                    .iter()
                    .flat_map(|d| d.partitions.iter())
                    .filter(|p| p.band_index == band.index)
                    .map(|p| format!("/dev/disk/by-partuuid/{}", p.part_uuid))
                    .collect();
                for member in &member_paths {
                    if let Err(e) = mdadm.zero_superblock(member) {
                        failures.push(format!("zero-superblock {member}: {e}"));
                    }
                }
            }
        }

        if !failures.is_empty() {
            return Err(OrchestrateError::Validation(format!(
                "destroy of group `{group_name}` did not fully complete: {}; the shr-rs \
                 configuration, mdadm.conf and fstab were left UNCHANGED (still listing this \
                 group) so nothing is treated as gone until it actually is -- inspect `mdadm \
                 --detail`/`lsblk`/`mount` and retry",
                failures.join("; ")
            )));
        }

        // Leaving the superblocks in place is a legitimate choice -- it is
        // what keeps a mistaken `destroy` recoverable by hand -- but on its
        // own it also means the kernel's incremental assembly finds those
        // members at the next boot and brings the dead array back (real
        // guest: a destroyed group returned as `/dev/md6`, owning a device
        // number, belonging to no group). Recording it here is what lets
        // `write_mdadm_conf` emit an `ARRAY <ignore>` line so the metadata
        // survives for recovery while nothing assembles it unasked. With
        // `zero_superblocks` there is no metadata left to ignore.
        if !zero_superblocks {
            let disk_ids: Vec<String> = full_state.groups[group_idx]
                .disks
                .iter()
                .map(|d| d.id.clone())
                .collect();
            let retired_at = Utc::now().to_rfc3339();
            for band in &full_state.groups[group_idx].bands {
                // A band whose `md_uuid` was never read back has nothing
                // that could match an mdadm.conf line, so there is nothing
                // to suppress -- and emitting `UUID=` with no value would
                // be a line mdadm cannot act on.
                if let Some(md_uuid) = &band.md_uuid {
                    full_state.retired_arrays.push(StateRetiredArray {
                        md_uuid: md_uuid.clone(),
                        group_name: group_name.clone(),
                        disk_ids: disk_ids.clone(),
                        retired_at: retired_at.clone(),
                    });
                }
            }
        }

        full_state.groups.remove(group_idx);
        if !self.runner.is_dry_run() {
            self.store.save(&full_state)?;
            self.write_managed_configs(&full_state)?;
            self.remove_group_scrub_unit(&group_name)?;
        }

        self.progress.report(ProgressUpdate {
            operation: "destroy".to_string(),
            stage: "done".to_string(),
            percent: Some(100.0),
            message: format!("group `{group_name}` destroyed"),
        });
        Ok(())
    }

    /// Real-guest repro: `destroy()` removed `group_name` from
    /// state.toml/mdadm.conf/fstab but left its `schedule install`-created
    /// `shr-rs-scrub-<group_name>` timer/service enabled forever, firing
    /// `fs scrub start --name <group_name>` against a group that no longer
    /// exists -- the exact orphan class an earlier fix already addressed for mdadm.conf/
    /// fstab, reproduced in systemd because nothing there tracked which
    /// units shr-rs itself owned. Fixed the same way an earlier fix addressed the config
    /// files: regenerate/remove based on the group actually being gone,
    /// scoped to ONLY this group's own unit pair -- `scrub_unit_paths`
    /// derives the exact same filenames `write_scrub_timer_units` writes,
    /// so this can never guess wrong and touch another group's units.
    ///
    /// `is_shr_rs_owned_unit` gates every deletion: an operator's own
    /// hand-written unit that happens to share this group's sanitized name
    /// is left completely alone, never even considered.
    ///
    /// Called AFTER state.toml/mdadm.conf/fstab are already saved above --
    /// the group is genuinely gone from shr-rs's own bookkeeping either
    /// way by this point, so a failure here is reported LOUDLY (an error,
    /// not a silently swallowed best-effort) but never rolls anything back
    /// (there is nothing left to roll back to, same reasoning `replace_
    /// disk`'s `stuck_removals`/an earlier fix already established for "the real
    /// change already happened, only cleanup failed").
    fn remove_group_scrub_unit(&self, group_name: &str) -> Result<(), OrchestrateError> {
        let (service_path, timer_path) = scrub_unit_paths(&self.unit_dir, group_name);
        let mut failures: Vec<String> = Vec::new();
        let mut any_removed = false;

        // Disable the TIMER (the thing `schedule install` actually
        // `systemctl enable --now`d) before deleting either file -- the
        // `.service` is never itself enabled, only invoked BY the timer.
        if timer_path.exists() && is_shr_rs_owned_unit(&timer_path) {
            if let Some(unit_name) = timer_path.file_name().and_then(|n| n.to_str()) {
                if let Err(e) = self.runner.run("systemctl", &["disable", "--now", unit_name]) {
                    failures.push(format!("systemctl disable --now {unit_name}: {e}"));
                }
            }
        }

        for path in [&timer_path, &service_path] {
            match remove_owned_unit_file(path) {
                Ok(true) => any_removed = true,
                Ok(false) => {} // never existed, or not shr-rs-owned -- correctly left alone
                Err(e) => failures.push(format!("removing {}: {e}", path.display())),
            }
        }

        if any_removed {
            if let Err(e) = self.runner.run("systemctl", &["daemon-reload"]) {
                failures.push(format!("systemctl daemon-reload: {e}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(OrchestrateError::Validation(format!(
                "group `{group_name}` was destroyed and the shr-rs configuration, mdadm.conf and \
                 fstab are already correct, but cleaning up its scheduled error-check systemd \
                 unit failed: {}; remove \
                 `{}`/`{}` by hand (`systemctl disable --now <name>`, delete the files, then \
                 `systemctl daemon-reload`)",
                failures.join("; "),
                service_path.display(),
                timer_path.display()
            )))
        }
    }

    /// Which group `name` (from `ExpandRequest`) refers to. `None` is only
    /// accepted when `state` holds EXACTLY one group: with several groups
    /// present, silently picking "the first one" would let an operator
    /// expand the wrong group by accident just by omitting `--name`, which
    /// is a far worse failure mode than requiring them to be explicit.
    fn resolve_group_index(state: &StateFile, name: Option<&str>) -> Result<usize, OrchestrateError> {
        match name {
            Some(n) => state
                .groups
                .iter()
                .position(|g| g.name == n)
                .ok_or_else(|| OrchestrateError::Validation(format!("no group named `{n}` exists"))),
            None => match state.groups.len() {
                0 => Err(OrchestrateError::NoActiveArray),
                1 => Ok(0),
                _ => Err(OrchestrateError::Validation(
                    "multiple groups exist; --name is required to select which one to expand".to_string(),
                )),
            },
        }
    }

    /// Match `reference` against a group's recorded disks by-id (exact)
    /// or serial (fragment, case-insensitive) -- entirely from `state.toml`,
    /// no live system call, so this works even for a disk that's physically
    /// gone. An ambiguous serial fragment (matches more than one disk) is
    /// treated as no match rather than guessing.
    fn find_disk_by_reference<'b>(disks: &'b [StateDisk], reference: &str) -> Option<&'b StateDisk> {
        if let Some(d) = disks.iter().find(|d| d.id == reference) {
            return Some(d);
        }
        let wanted = reference.to_lowercase();
        let mut hits = disks.iter().filter(|d| {
            d.serial
                .as_deref()
                .is_some_and(|s| s.to_lowercase().contains(&wanted))
        });
        let first = hits.next()?;
        if hits.next().is_some() {
            return None;
        }
        Some(first)
    }

    pub fn expand(&self, req: ExpandRequest) -> Result<ArrayState, OrchestrateError> {
        // Opportunistically finish any resize a PRIOR expand() had to defer
        // (an earlier review) before considering this new request -- keeps
        // "capacity never actually increases" from compounding indefinitely
        // across repeated expansions. Reconciles EVERY group, not just the
        // one this call targets.
        self.reconcile()?;

        let mut full_state = self.store.load()?.ok_or(OrchestrateError::NoActiveArray)?;
        let group_idx = Self::resolve_group_index(&full_state, req.name.as_deref())?;

        if full_state.groups[group_idx].expansion.in_progress {
            // A checkpoint whose `resumable` bit is set carries a
            // persisted `plan`/`new_disks` snapshot (captured when THIS
            // expansion originally started, below) that can be replayed
            // from `checkpoint.step_index` without asking the operator to
            // re-supply the same disks or re-deriving a plan against
            // possibly-already-partially-applied state (see
            // `StateExpansion::plan`'s doc comment for why recomputing
            // would mis-plan a disk that already contributed to one band
            // but still has capacity for a later step). `req` is ignored
            // entirely on this path -- the persisted plan is authoritative,
            // not whatever the caller happened to pass this time.
            let expansion = full_state.groups[group_idx].expansion.clone();
            let resumable =
                expansion.checkpoint.as_ref().is_some_and(|c| c.resumable) && !expansion.plan.is_empty();
            if resumable {
                return self.resume_expand(full_state, group_idx);
            }

            // An earlier review finding: this flag is now only left set
            // when a step actually crossed its point of no return (a real
            // mdadm reshape started, or a PV joined the shared VG) -- a
            // step that failed cleanly before that point clears it again
            // automatically. So reaching this branch means a real,
            // physical change is in an unfinished state with no persisted
            // plan to resume from (a older `state.toml`); name exactly
            // where to look.
            let checkpoint_desc = full_state.groups[group_idx]
                .expansion
                .checkpoint
                .as_ref()
                .map(|c| c.description.as_str())
                .unwrap_or("(no checkpoint recorded)");
            return Err(OrchestrateError::Validation(format!(
                "an expansion of group `{}` is already in progress ({checkpoint_desc}) with no \
                 resumable plan recorded; a real change was made to the array and this \
                 configuration predates automatic resume support. Inspect the array's real state (`mdadm \
                 --detail`, `/proc/mdstat`, `lsblk`), and once you've confirmed it's healthy, \
                 clear that group's `expansion.in_progress` (and `expansion.checkpoint`) in \
                 /var/lib/shr-rs/state.toml by hand before retrying",
                full_state.groups[group_idx].name
            )));
        }

        // Reject duplicate new-disk identity up front, same as create()'s D3 check.
        let mut seen_ids = HashSet::new();
        for d in &req.new_disks {
            if !seen_ids.insert(d.id.as_str()) {
                return Err(OrchestrateError::Validation(format!(
                    "duplicate disk id `{}` in expand request",
                    d.id.as_str()
                )));
            }
        }

        // Reject a disk that already belongs to ANY group (including one
        // other than the group being expanded) -- same reasoning as
        // create()'s equivalent check: a disk can only ever be a member of
        // one group.
        let disks_in_any_group: HashSet<&str> = full_state
            .groups
            .iter()
            .flat_map(|g| g.disks.iter())
            .map(|d| d.id.as_str())
            .collect();
        for d in &req.new_disks {
            if disks_in_any_group.contains(d.id.as_str()) {
                return Err(OrchestrateError::Validation(format!(
                    "disk `{}` already belongs to an existing group; a disk can only be a \
                     member of one group at a time",
                    d.id.as_str()
                )));
            }
        }

        // Safety validation, same pattern as create() (D4).
        for d in &req.new_disks {
            let kernel_path = format!("/dev/{}", d.kernel_name);
            SafetyGuard::validate_disk_target(&kernel_path, &req.system_disks)?;
            let by_id_path = resolve_disk_path(&d.id).display().to_string();
            SafetyGuard::validate_disk_target(&by_id_path, &req.system_disks)?;
        }

        let parted = PartedExecutor::new(self.runner);
        let mdadm = MdadmExecutor::new(self.runner);
        let lvm = LvmExecutor::new(self.runner);
        let btrfs = BtrfsExecutor::new(self.runner);

        // Prerequisite checks (D11), same as create().
        parted.ensure_supported()?;
        mdadm.ensure_supported()?;
        lvm.ensure_supported()?;
        btrfs.ensure_supported()?;

        // The design: never run a destructive expansion step against a
        // degraded array -- a second failure mid-reshape risks data loss.
        // Scoped to the TARGET group's own bands: an unrelated group being
        // degraded has no bearing on whether it's safe to expand this one
        // (they share no spindles -- create()/expand() both refuse to let
        // any disk belong to more than one group).
        //
        // Reads through `self.status_runner()`, NOT the `mdadm` above built
        // from `self.runner`: `preview_expand` runs this whole
        // function against a `DryRunRunner`, whose `is_dry_run()` shortcut
        // makes `degraded_count`/`sync_action` fabricate a fixed
        // "0 degraded"/"idle" answer instead of ever reading anything real.
        // That blinded this check (and the background-activity one right
        // below) during preview, letting a real, currently-running scrub
        // fall through both all the way to the misleading staleness message --
        // BEFORE the real, non-preview `expand()` call downstream (which
        // reads the real system through `self.runner` there) ever got a
        // chance to catch it correctly.
        let status_mdadm = MdadmExecutor::new(self.status_runner());
        for band in &full_state.groups[group_idx].bands {
            if status_mdadm.degraded_count(&band.md_name)? > 0 {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) is degraded; expand is blocked until it is healthy",
                    band.index, band.md_name
                )));
            }
        }

        // An earlier review finding: `execute_grow` correctly detects a reshape
        // already in flight on the band IT is growing and defers that
        // band's own resize (Step 8 SM-EXPAND-1), but nothing previously
        // stopped a DIFFERENT band from starting `--grow` while one band
        // was already reshaping -- two reshapes competing for the same
        // underlying spindles, which the design assumes never
        // happens. Refuse the whole expand() up front if any band OF THE
        // TARGET GROUP has ANY background activity (initial resync
        // included, not just reshape) -- simple and safe, even though it's
        // occasionally more conservative than strictly necessary (e.g.
        // blocking a brand-new, unrelated CreateBand while another band
        // merely finishes its post-create resync). Scoped to this group
        // for the same reason as the degraded check just above. Same
        // `status_runner` reasoning as that check.
        for band in &full_state.groups[group_idx].bands {
            let activity = status_mdadm.sync_action(&band.md_name)?;
            if activity != "idle" {
                return Err(OrchestrateError::Validation(format!(
                    "band {} ({}) has background activity in progress (sync_action={activity}); \
                     expand is blocked until it finishes -- starting a second reshape/resync on \
                     another band at the same time is not supported",
                    band.index, band.md_name
                )));
            }
        }

        // A running Btrfs scrub is filesystem-level,
        // not an md-level resync, so it never shows up in the sync_action
        // check just above -- `mdadm`'s sync_action stays `idle` the whole
        // time a scrub is running. Without this check, that let a running
        // scrub fall through all the way to the freshness check below,
        // where `scrub_staleness` sees no COMPLETED scrub yet and reports
        // "has never been checked ... run `fs scrub start`" --
        // actively misleading the operator into thinking nothing is
        // running when one already is.
        //
        // Reuses `scrub_status()` rather than reading the possibly-stale
        // `scrub_in_progress` bit directly: that bit is only ever cleared
        // by a `scrub_status`/`fs scrub status` call actually observing the
        // scrub has finished, so a scrub that finished without anyone
        // polling it would otherwise look "still running" here forever.
        // `scrub_status()` asks the kernel (`btrfs scrub status`/mdadm
        // `sync_action`) directly and reconciles the stored bit if it's
        // out of date, so this check reports the REAL current state.
        //
        // Checked unconditionally (unlike the freshness gate below, this
        // isn't gated by `skip_scrub_check`): that flag means "I accept
        // expanding without a fresh completed scrub", not "let two
        // operations touch this array's data at once".
        let scrub_report = self.scrub_status(req.name.as_deref())?;
        if scrub_report.running {
            return Err(OrchestrateError::Validation(format!(
                "group `{}` currently has a scrub running; wait for it to finish (check `fs \
                 scrub status`) before expanding -- a scrub and a reshape must not run against \
                 the same array at the same time",
                scrub_report.group_name
            )));
        }

        // The the design safety table's scrub-before-reshape requirement
        // (decision) -- a reshape recomputes/redistributes parity from
        // every member's current data, so silent corruption already present
        // on a disk gets baked into the new layout instead of being caught.
        // Require a scrub that both COMPLETED (not cancelled/failed) and is
        // recent enough on every band of the TARGET group, unless the
        // caller explicitly accepts the risk. Scoped to this group for the
        // same reason as the degraded/sync_action checks just above --
        // never every group's history.
        if !req.skip_scrub_check {
            for band in &full_state.groups[group_idx].bands {
                if let Some(staleness) = scrub_staleness(band) {
                    // The cause-specific half comes from `describe()`; the
                    // advice after it is identical for every cause, and both
                    // other UIs key off the `--skip-scrub-check` substring in
                    // it to tell this recoverable refusal from the ones no
                    // override can fix (`shr-tui`'s `is_scrub_check_warning`,
                    // the Cockpit plugin's `isScrubCheckWarning`). Any new
                    // variant must keep that tail.
                    return Err(OrchestrateError::Validation(format!(
                        "band {} ({}) {}; run `shr-rs fs scrub start` first, or pass \
                         --skip-scrub-check to expand anyway (expanding rebuilds the redundancy \
                         from each disk's CURRENT data, so undetected corruption gets baked into \
                         the new layout instead of being caught)",
                        band.index,
                        band.md_name,
                        staleness.describe()
                    )));
                }
            }
        }

        let snapshot = snapshot_from_state(&full_state.groups[group_idx])?;
        let new_core_disks: Vec<shr_core::Disk> =
            req.new_disks.iter().map(ResolvedDisk::to_planner_disk).collect();
        let plan = plan_expansion(&snapshot, &new_core_disks)
            .map_err(|e| OrchestrateError::Planner(e.to_string()))?;

        // An earlier review finding: mirrors create()'s earlier-review guard.
        // A requested disk that appears in no step at all (too small to
        // extend any existing band or seed a new one) would otherwise be
        // silently ignored while the CLI still reports "expanded
        // successfully" -- the same false-success shape D1 was about, just
        // narrower (no disks touched, no version bump, but still a claimed
        // success for a request that did nothing).
        for d in &req.new_disks {
            let used = plan.steps.iter().any(|s| match s {
                ExpansionStep::GrowBand { add_members, .. } => add_members.contains(&d.id),
                ExpansionStep::LevelUp { add_members, .. } => add_members.contains(&d.id),
                ExpansionStep::CreateBand { band } => band.members().contains(&d.id),
                ExpansionStep::MarkUnusable { .. } => false,
            });
            if !used {
                return Err(OrchestrateError::Validation(format!(
                    "disk `{}` would not be used by this expansion (too small relative to \
                     reserved space/alignment or other disks); remove it from the request",
                    d.id.as_str()
                )));
            }
        }

        let disks_by_id: HashMap<&str, &ResolvedDisk> =
            req.new_disks.iter().map(|d| (d.id.as_str(), d)).collect();
        // Pre-existing array members already have a GPT table from the
        // original create() -- never call create_gpt on them again, that
        // would wipe their existing partitions. Only a disk touched for the
        // first time in this expand() call (always one from req.new_disks,
        // possibly across more than one step -- see execute_grow/
        // execute_create_band) needs one.
        let mut gpt_initialized: HashSet<String> = full_state.groups[group_idx]
            .disks
            .iter()
            .map(|d| d.id.clone())
            .collect();
        // Seeded from every band in every group -- see `create()`'s
        // identical use of `used_md_numbers` for why this must be
        // host-wide, not scoped to the group being expanded: a brand new
        // band this expansion creates (`ExpansionStep::CreateBand`) must
        // never collide with a DIFFERENT group's `/dev/mdN`. ALSO seeded
        // from the host's real `/dev/mdN` numbers -- see
        // `host_md_numbers`'s doc comment.
        let mut used_md = used_md_numbers(&full_state);
        used_md.extend(host_md_numbers(self.runner)?);

        // Confirm before crossing into destructive territory -- same
        // placement rationale as create()'s equivalent gate just above the
        // "everything from here on is destructive" boundary, and likewise
        // skipped entirely under dry-run (a simulation touches nothing
        // real, so there is nothing to confirm).
        if !self.runner.is_dry_run() {
            let disk_list: Vec<&str> = req.new_disks.iter().map(|d| d.id.as_str()).collect();
            let decision = self.confirm.confirm(&ConfirmRequest {
                operation: "expand".to_string(),
                summary: format!(
                    "expand group `{}` by adding {} disk(s): {} ({} step(s) planned)",
                    full_state.groups[group_idx].name,
                    req.new_disks.len(),
                    disk_list.join(", "),
                    plan.steps.len()
                ),
                irreversible: true,
            });
            if decision == Confirmation::Reject {
                return Err(OrchestrateError::Rejected(format!(
                    "expand of group `{}` was rejected via ConfirmSink before touching any disk",
                    full_state.groups[group_idx].name
                )));
            }
            // See create()'s identical call for the rationale.
            self.reverify_targets(&req.new_disks)?;
        }

        // Capture exactly what this expansion needs to resume after a
        // crash -- the disks it was asked to add and the plan computed for
        // them -- alongside the checkpoint every step below advances.
        // `resumable: true` from here on (unlike the older `false`
        // literal this replaced): a plan now IS persisted, so a crash after
        // this point has something to replay.
        prune_retired_arrays_for(&mut full_state, &req.new_disks);
        full_state.groups[group_idx].expansion.in_progress = true;
        full_state.groups[group_idx].expansion.new_disks =
            req.new_disks.iter().map(pending_disk_from_resolved).collect();
        full_state.groups[group_idx].expansion.plan = plan.steps.clone();
        full_state.groups[group_idx].expansion.target_layout_version = plan.target_layout_version as u32;
        full_state.groups[group_idx].expansion.checkpoint = Some(StateCheckpoint {
            step_index: 0,
            band_index: None,
            resumable: true,
            description: format!("expansion starting: {} step(s) planned", plan.steps.len()),
        });
        if !self.runner.is_dry_run() {
            self.store.save(&full_state)?;
        }

        self.execute_plan_steps(
            &mut full_state,
            group_idx,
            &plan.steps,
            0,
            &disks_by_id,
            &mut gpt_initialized,
            &mut used_md,
        )?;

        self.finish_expansion(&mut full_state, group_idx, plan.target_layout_version)?;
        Ok(full_state.groups[group_idx].clone())
    }

    /// Resume: called instead of the normal plan/confirm/execute
    /// sequence above when `expand()` finds an `in_progress` expansion with
    /// a resumable, persisted plan. Never re-confirms (the operator already
    /// approved this expansion when it originally started; a resume is not
    /// a NEW destructive action being newly authorized -- same reasoning as
    /// `reconcile`'s own "no ConfirmSink gate" doc comment) and never
    /// re-validates degraded/sync_action/system-disk guards (those guard
    /// STARTING a new expansion safely; this continues one already
    /// physically underway). `req` from the caller's `expand()` invocation
    /// is never consulted here -- see the call site's doc comment.
    fn resume_expand(
        &self,
        mut full_state: StateFile,
        group_idx: usize,
    ) -> Result<ArrayState, OrchestrateError> {
        let expansion = full_state.groups[group_idx].expansion.clone();
        let plan_steps = expansion.plan.clone();
        let start_index = expansion.checkpoint.as_ref().map(|c| c.step_index).unwrap_or(0);
        let target_layout_version = expansion.target_layout_version as u64;

        let parted = PartedExecutor::new(self.runner);
        let mdadm = MdadmExecutor::new(self.runner);
        let lvm = LvmExecutor::new(self.runner);
        let btrfs = BtrfsExecutor::new(self.runner);
        parted.ensure_supported()?;
        mdadm.ensure_supported()?;
        lvm.ensure_supported()?;
        btrfs.ensure_supported()?;

        // Reconstruct the disks the ORIGINAL expand() call resolved, from
        // the persisted snapshot -- never from a fresh caller-supplied
        // list, which could have drifted (or been omitted) across the
        // crash. `resolve_disk_path` (used deep inside execute_grow/
        // execute_create_band) keys off `id` alone, so a `kernel_name` that
        // drifted across a reboot doesn't affect where commands actually
        // target.
        let pending: Vec<ResolvedDisk> = expansion
            .new_disks
            .iter()
            .map(resolved_disk_from_pending)
            .collect();
        let disks_by_id: HashMap<&str, &ResolvedDisk> = pending.iter().map(|d| (d.id.as_str(), d)).collect();
        let mut gpt_initialized: HashSet<String> = full_state.groups[group_idx]
            .disks
            .iter()
            .map(|d| d.id.clone())
            .collect();
        let mut used_md = used_md_numbers(&full_state);
        used_md.extend(host_md_numbers(self.runner)?);

        self.progress.report(ProgressUpdate {
            operation: "expand".to_string(),
            stage: "resume".to_string(),
            percent: None,
            message: format!(
                "resuming group `{}` expansion at step {} of {}",
                full_state.groups[group_idx].name,
                start_index + 1,
                plan_steps.len()
            ),
        });

        self.execute_plan_steps(
            &mut full_state,
            group_idx,
            &plan_steps,
            start_index,
            &disks_by_id,
            &mut gpt_initialized,
            &mut used_md,
        )?;

        self.finish_expansion(&mut full_state, group_idx, target_layout_version)?;
        Ok(full_state.groups[group_idx].clone())
    }

    /// Run `plan_steps[start_index..]` in order, persisting a checkpoint
    /// before and immediately after every step (an earlier review finding/F2) --
    /// shared by a plan's first, normal run (`start_index == 0`, from
    /// `expand()`) and a crash resume (`start_index == checkpoint.
    /// step_index`, from `resume_expand`) so the two paths can never drift
    /// apart. On failure, leaves `expansion.in_progress`/`checkpoint` (and
    /// therefore `plan`/`new_disks`, untouched here) exactly as the failing
    /// step's own point-of-no-return status dictates -- see the loop body's
    /// comment -- so a later `expand()` call either resumes from the same
    /// checkpoint again or is free to start fresh.
    #[allow(clippy::too_many_arguments)]
    fn execute_plan_steps(
        &self,
        full_state: &mut StateFile,
        group_idx: usize,
        plan_steps: &[ExpansionStep],
        start_index: usize,
        disks_by_id: &HashMap<&str, &ResolvedDisk>,
        gpt_initialized: &mut HashSet<String>,
        used_md: &mut HashSet<u32>,
    ) -> Result<(), OrchestrateError> {
        let parted = PartedExecutor::new(self.runner);
        let mdadm = MdadmExecutor::new(self.runner);
        let lvm = LvmExecutor::new(self.runner);
        let btrfs = BtrfsExecutor::new(self.runner);

        for (step_index, step) in plan_steps.iter().enumerate().skip(start_index) {
            full_state.groups[group_idx].expansion.checkpoint = Some(StateCheckpoint {
                step_index,
                band_index: step_band_index(step),
                resumable: true,
                description: describe_step(step),
            });
            if !self.runner.is_dry_run() {
                self.store.save(full_state)?;
            }

            self.progress.report(ProgressUpdate {
                operation: "expand".to_string(),
                stage: format!("step-{}-of-{}", step_index + 1, plan_steps.len()),
                percent: Some((step_index as f64 / plan_steps.len().max(1) as f64) * 100.0),
                message: describe_step(step),
            });

            // An earlier review finding: whether this step ever crossed
            // its point of no return (a real, physical change was made and
            // already persisted inside execute_grow/execute_create_band --
            // see F1). Used below to decide whether it's safe to release
            // the in-progress lock on failure: if nothing physical
            // survived this step's own internal rollback, a transient
            // failure (a flaky `parted`/`udevadm` call, say) must not
            // permanently lock the user out of ever expanding again.
            let mut crossed_ponr = false;
            let step_result: Result<(), OrchestrateError> = match step {
                ExpansionStep::MarkUnusable { .. } => {
                    // No destructive command, and the current schema has no
                    // field to record stranded capacity in yet (documented
                    // gap) -- purely informational.
                    Ok(())
                }
                ExpansionStep::GrowBand {
                    band_index,
                    add_members,
                } => self.execute_grow(
                    &parted,
                    &mdadm,
                    &lvm,
                    &btrfs,
                    full_state,
                    group_idx,
                    gpt_initialized,
                    disks_by_id,
                    *band_index,
                    None,
                    add_members,
                    &mut crossed_ponr,
                ),
                ExpansionStep::LevelUp {
                    band_index,
                    to,
                    add_members,
                    ..
                } => self.execute_grow(
                    &parted,
                    &mdadm,
                    &lvm,
                    &btrfs,
                    full_state,
                    group_idx,
                    gpt_initialized,
                    disks_by_id,
                    *band_index,
                    Some(*to),
                    add_members,
                    &mut crossed_ponr,
                ),
                ExpansionStep::CreateBand { band } => self.execute_create_band(
                    &parted,
                    &mdadm,
                    &lvm,
                    &btrfs,
                    full_state,
                    group_idx,
                    gpt_initialized,
                    disks_by_id,
                    band,
                    shr_core::DEFAULT_RESERVED_HEAD,
                    used_md,
                    &mut crossed_ponr,
                ),
            };

            if let Err(e) = step_result {
                if !crossed_ponr {
                    // Nothing physical survived this step's attempt (its
                    // own internal rollback already ran, if one was
                    // needed) -- safe to release the lock so a transient
                    // failure doesn't permanently block future expansions.
                    full_state.groups[group_idx].expansion.in_progress = false;
                    full_state.groups[group_idx].expansion.checkpoint = None;
                    full_state.groups[group_idx].expansion.plan = Vec::new();
                    full_state.groups[group_idx].expansion.new_disks = Vec::new();
                    full_state.groups[group_idx].expansion.target_layout_version = 0;
                }
                // else: a real physical change already happened and was
                // persisted inside the step (F1) -- leave in_progress=true
                // and the plan/new_disks/checkpoint in place so a later
                // `expand()` call resumes exactly here.
                if !self.runner.is_dry_run() {
                    self.store.save(full_state)?;
                }
                return Err(e);
            }

            // Persist this step's real result immediately -- if a LATER
            // step fails, this one's already-completed physical change
            // (the array really did grow) must not be lost from state.toml.
            if !self.runner.is_dry_run() {
                self.store.save(full_state)?;
            }
        }

        Ok(())
    }

    /// Common tail of a plan finishing successfully, whether via
    /// `expand()`'s first run or `resume_expand`'s continuation: bump
    /// `layout_version` to what the plan targeted and clear every
    /// in-progress/resume field (an earlier review rule: a finished
    /// expansion must leave nothing behind for a later `expand()` call to
    /// mistake for a still-pending one).
    fn finish_expansion(
        &self,
        full_state: &mut StateFile,
        group_idx: usize,
        target_layout_version: u64,
    ) -> Result<(), OrchestrateError> {
        full_state.groups[group_idx].layout_version = target_layout_version as u32;
        full_state.groups[group_idx].expansion.in_progress = false;
        full_state.groups[group_idx].expansion.checkpoint = None;
        full_state.groups[group_idx].expansion.plan = Vec::new();
        full_state.groups[group_idx].expansion.new_disks = Vec::new();
        full_state.groups[group_idx].expansion.target_layout_version = 0;

        if !self.runner.is_dry_run() {
            self.store.save(full_state)?;
            self.write_managed_configs(full_state)?;
        }
        self.progress.report(ProgressUpdate {
            operation: "expand".to_string(),
            stage: "done".to_string(),
            percent: Some(100.0),
            message: format!("group `{}` expanded", full_state.groups[group_idx].name),
        });
        Ok(())
    }

    /// This band's member disk device paths (by-id, resolved the same way
    /// every destructive command targets a disk in this engine) -- what a
    /// `LiveMetricsSampler` asks `smartctl` about for temperature/
    /// reallocated-sector signals. SMART is a whole-disk concept, so this is
    /// deliberately every disk that has ANY partition tagged with
    /// `band_index`, not the band's mdadm member partitions themselves.
    fn band_member_disk_paths(state: &StateFile, group_idx: usize, band_index: u8) -> Vec<String> {
        state.groups[group_idx]
            .disks
            .iter()
            .filter(|d| d.partitions.iter().any(|p| p.band_index == band_index))
            .map(|d| {
                resolve_disk_path(&DiskId::from(d.id.clone()))
                    .display()
                    .to_string()
            })
            .collect()
    }

    /// Run one `ReshapeThrottle::tick()` for `band_index` and return its
    /// decision -- `self.metrics_sampler` (an explicit test/caller override)
    /// if one is wired, otherwise a `LiveMetricsSampler` built from
    /// this band's own member disks and its persisted
    /// `last_smart_reallocated` baseline. When the live path is taken, also
    /// writes the new absolute SMART total back into
    /// `state.groups[group_idx].bands[band_pos].last_smart_reallocated` so
    /// the NEXT tick -- a different process entirely, for the periodic
    /// timer path -- can compute a real delta again instead of comparing against
    /// nothing.
    ///
    /// `priority` decides both the decision thresholds and, in the caller,
    /// the speed band -- taken as an explicit parameter rather than read
    /// from `self.priority`, because the periodic tick is a brand-new
    /// process that must use the BAND's own persisted profile, not whatever
    /// that process's builder happened to default to.
    fn tick_throttle_decision(
        &self,
        state: &mut StateFile,
        group_idx: usize,
        band_pos: usize,
        priority: SyncPriority,
    ) -> ThrottleTick {
        if let Some(sampler) = self.metrics_sampler {
            return ReshapeThrottle::new(priority, sampler).tick();
        }

        let band_index = state.groups[group_idx].bands[band_pos].index;
        let member_disks = Self::band_member_disk_paths(state, group_idx, band_index);
        let previous_total = state.groups[group_idx].bands[band_pos].last_smart_reallocated;
        let live = LiveMetricsSampler::new(self.runner, member_disks, previous_total);
        let tick = ReshapeThrottle::new(priority, &live).tick();
        state.groups[group_idx].bands[band_pos].last_smart_reallocated = live.last_smart_total();
        tick
    }

    /// Where this band's speed limits get written, probed once and recorded
    /// so later ticks don't re-probe. The per-array attributes are preferred
    /// wherever they exist: they make a per-band profile mean something, let
    /// two groups sync at different profiles at once, and make teardown a
    /// write of `system` rather than a restore of a remembered number.
    fn band_limit_scope(&self, state: &mut StateFile, group_idx: usize, band_pos: usize) -> LimitScope {
        if let Some(per_array) = state.groups[group_idx].bands[band_pos].sync_limits_per_array {
            return if per_array {
                LimitScope::PerArray
            } else {
                LimitScope::HostWide
            };
        }
        let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
        let scope = shr_exec::probe_limit_scope(self.runner, &md_name);
        // Not recorded under dry-run: `probe_limit_scope` cannot read
        // anything real there, so its answer says nothing about this host.
        if !self.runner.is_dry_run() {
            state.groups[group_idx].bands[band_pos].sync_limits_per_array =
                Some(scope == LimitScope::PerArray);
        }
        scope
    }

    /// Set a just-started sync's kernel limits for `priority`, then apply ONE
    /// throttle decision on top -- so a danger signal already present at the
    /// moment it starts (an elevated SMART reallocated count, a hot disk)
    /// brakes it immediately instead of blindly writing the profile's
    /// ceiling and only reacting later.
    ///
    /// Deliberately a single tick, not a loop that polls until the operation
    /// finishes: a real reshape can run for hours (see `execute_grow`'s
    /// `resize_pending` doc comment), and the caller must return promptly.
    /// Ongoing monitoring is `tick_active_sync`'s job, driven by a periodic
    /// systemd timer.
    ///
    /// Persists `priority` on the band, which is both what the periodic tick
    /// reads back and the marker that this project wrote these limits at all
    /// (so `clear_band_limits` knows what to hand back later).
    fn start_sync_throttle(
        &self,
        state: &mut StateFile,
        group_idx: usize,
        band_pos: usize,
        priority: SyncPriority,
    ) -> Result<(), OrchestrateError> {
        let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
        let scope = self.band_limit_scope(state, group_idx, band_pos);
        // BEFORE the first write, never after -- the write below destroys
        // the value being saved. Only the host-wide fallback needs it; the
        // per-array path clears itself exactly.
        if scope == LimitScope::HostWide {
            self.remember_speed_limit_max(state);
        }
        let capability_kb = state.groups[group_idx].bands[band_pos].sync_capability_kb;
        let mut ctrl = ThrottleController::new(self.runner, &md_name, priority, capability_kb, scope);
        ctrl.apply_initial()?;
        let tick = self.tick_throttle_decision(state, group_idx, band_pos, priority);
        let speed_kb = ctrl.apply(tick.decision)?;
        let band = &mut state.groups[group_idx].bands[band_pos];
        band.sync_priority = Some(priority.as_str().to_string());
        Self::record_throttle(band, &tick, speed_kb);
        Ok(())
    }

    /// One throttle tick for a band whose md sync is currently running:
    /// fold this tick's `sync_speed` into the capability estimate, re-derive
    /// the profile's limits from it, then decide and apply.
    ///
    /// The estimate is updated BEFORE the decision so a band whose capability
    /// has just been learned stops running under the bootstrap constants at
    /// the first tick that yields an observation, rather than one tick later.
    fn tick_sync_throttle(
        &self,
        state: &mut StateFile,
        group_idx: usize,
        band_pos: usize,
        priority: SyncPriority,
    ) -> Result<(), OrchestrateError> {
        let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
        let scope = self.band_limit_scope(state, group_idx, band_pos);
        let before = CapabilityEstimate::new(
            state.groups[group_idx].bands[band_pos].sync_capability_kb,
            state.groups[group_idx].bands[band_pos].sync_capability_uncapped_ticks,
        );
        let mut ctrl = ThrottleController::resume(self.runner, &md_name, priority, before.kb, scope);

        let observed = shr_exec::read_sync_speed_kb(self.runner, &md_name);
        let updated = before.observe(observed, ctrl.current_speed_kb());
        if updated.kb != before.kb {
            state.groups[group_idx].bands[band_pos].sync_capability_kb = updated.kb;
            state.groups[group_idx].bands[band_pos].sync_capability_observed_at =
                Some(Utc::now().to_rfc3339());
            ctrl.set_capability(updated.kb);
        }
        state.groups[group_idx].bands[band_pos].sync_capability_uncapped_ticks = updated.uncapped_ticks;

        let tick = self.tick_throttle_decision(state, group_idx, band_pos, priority);
        let speed_kb = ctrl.apply(tick.decision)?;
        let band = &mut state.groups[group_idx].bands[band_pos];
        // Recorded even when the profile came from the fallback rather than
        // from the operator, so the band's limits are cleared once its sync
        // ends the same way an `expand`'s are.
        band.sync_priority = Some(priority.as_str().to_string());
        Self::record_throttle(band, &tick, speed_kb);
        Ok(())
    }

    /// Hand a band's speed limits back once its sync has finished, and
    /// forget the profile that governed it -- a stale value surviving past
    /// its own operation would silently govern the band's NEXT one.
    /// Returns whether anything changed.
    ///
    /// A no-op for a band this project never wrote limits for, and for the
    /// host-wide fallback, where there is nothing per-array to clear and
    /// `restore_speed_limit_if_idle` owns putting the operator's own value
    /// back.
    fn clear_band_limits(
        &self,
        state: &mut StateFile,
        group_idx: usize,
        band_pos: usize,
    ) -> Result<bool, OrchestrateError> {
        let Some(priority) = state.groups[group_idx].bands[band_pos]
            .sync_priority
            .as_deref()
            .and_then(SyncPriority::parse)
        else {
            return Ok(false);
        };
        let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
        let scope = self.band_limit_scope(state, group_idx, band_pos);
        ThrottleController::new(self.runner, &md_name, priority, None, scope).clear()?;
        state.groups[group_idx].bands[band_pos].sync_priority = None;
        Ok(true)
    }

    /// Apply this engine's profile to a band that has just started an md
    /// sync of its own accord -- the resync after `create`, the recovery
    /// after `replace_disk` -- but only if one is actually running, so a
    /// band that finished instantly is not left with a floor nothing will
    /// clear until the next tick.
    fn govern_running_sync(
        &self,
        state: &mut StateFile,
        group_idx: usize,
        band_pos: usize,
    ) -> Result<(), OrchestrateError> {
        let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
        match Self::live_sync_action(self.runner, &md_name)? {
            Some(action) if action != "idle" => {
                self.start_sync_throttle(state, group_idx, band_pos, self.priority)
            }
            _ => Ok(()),
        }
    }

    fn record_throttle(band: &mut StateBand, tick: &ThrottleTick, speed_kb: u64) {
        band.last_throttle_decision = Some(tick.decision.as_str().to_string());
        band.last_throttle_reason = Some(tick.reason.clone());
        band.last_throttle_speed_kb = Some(speed_kb);
    }

    /// Forget what this band was measured doing, because adding or removing
    /// a member changes what the array can do -- a capability learned on the
    /// old membership would derive every limit from a number that no longer
    /// describes this array.
    fn discard_capability_estimate(band: &mut StateBand) {
        band.sync_capability_kb = None;
        band.sync_capability_observed_at = None;
        band.sync_capability_uncapped_ticks = 0;
    }

    /// Record what `/proc/sys/dev/raid/speed_limit_max` read BEFORE this
    /// project overwrites it, so `restore_speed_limit_if_idle` can put the
    /// operator's own value back afterward. Call this immediately before the
    /// first write of an operation, never after.
    ///
    /// Only ever fills an EMPTY slot. A second operation starting while an
    /// shr-rs-written value is still in place (a scrub right after a
    /// reshape whose restore has not run yet, or a crash between the two)
    /// must not overwrite the saved value with one of this project's own --
    /// that would make the real prior value unrecoverable, which is the bug
    /// this whole mechanism exists to fix, one level up.
    ///
    /// Saves nothing when the value cannot be read: under a `DryRunRunner`
    /// (which wrote nothing either, so there is nothing to put back) and on
    /// a genuine read failure, where inventing a number to "restore" later
    /// would be strictly worse than leaving the kernel alone.
    fn remember_speed_limit_max(&self, state: &mut StateFile) {
        if state.saved_speed_limit_max_kb.is_some() {
            return;
        }
        state.saved_speed_limit_max_kb = shr_exec::read_speed_limit_max(self.runner);
    }

    /// Put the saved host-wide `speed_limit_max` back once no md array on
    /// this host is running anything, and clear the slot. Returns the value
    /// restored, or `None` when it did nothing.
    ///
    /// The condition is every band of every group reading `sync_action ==
    /// "idle"`, which covers a reshape and a scrub (`check`) alike -- and a
    /// group whose array is not assembled at all counts as idle, since a
    /// device that does not exist cannot be consuming a speed limit.
    ///
    /// A running Btrfs scrub deliberately does NOT hold the restore off:
    /// `speed_limit_max` governs md's own sync threads and nothing else, so
    /// a Btrfs scrub still running long after every band went idle has no
    /// stake in this value.
    ///
    /// Reads go through `status_runner()` for the usual reason (a preview
    /// must see the REAL kernel state), but the whole call is skipped under
    /// a dry run: `preview_expand` replays a mutating path against a
    /// `DryRunRunner`, and a preview that listed a `speed_limit_max` write
    /// nobody is going to perform would be exactly the "don't show a command
    /// that isn't really going to execute" problem this codebase has already
    /// had to fix once.
    fn restore_speed_limit_if_idle(&self, state: &mut StateFile) -> Result<Option<u64>, OrchestrateError> {
        let Some(saved_kb) = state.saved_speed_limit_max_kb else {
            return Ok(None);
        };
        if self.runner.is_dry_run() {
            return Ok(None);
        }
        for group in &state.groups {
            for band in &group.bands {
                if let Some(action) = Self::live_sync_action(self.status_runner(), &band.md_name)? {
                    if action != "idle" {
                        return Ok(None);
                    }
                }
            }
        }
        shr_exec::write_speed_limit_max(self.runner, saved_kb)?;
        state.saved_speed_limit_max_kb = None;
        Ok(Some(saved_kb))
    }

    /// One band's live `sync_action`, or `None` when its array is not
    /// assembled at all.
    ///
    /// Real-guest repro (recorded on `tick_active_sync`, which this was
    /// factored out of): `state.toml` outlived the array after an unplanned
    /// power cycle, so `cat /sys/block/<md>/md/sync_action` had no file to
    /// read and failed with `No such file or directory`. Every sweep across
    /// every band has to survive that rather than abort. Matched on BOTH
    /// `program == "cat"` AND the ENOENT text, so that if `sync_action`'s
    /// read path ever grows a second command, a failure from THAT command is
    /// never swallowed here just because its message happens to say the same
    /// thing.
    fn live_sync_action(
        runner: &dyn CommandRunner,
        md_name: &str,
    ) -> Result<Option<String>, OrchestrateError> {
        match MdadmExecutor::new(runner).sync_action(md_name) {
            Ok(action) => Ok(Some(action)),
            Err(ExecError::NonZeroExit {
                ref program,
                ref stderr,
                ..
            }) if program == "cat" && stderr.contains("No such file or directory") => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Apply one throttle tick to every band, across every group, that
    /// currently has an md sync running, and hand back the limits of every
    /// band that has finished one. Meant to be invoked periodically by a
    /// systemd timer (`shr-rs internal reshape-throttle-tick`) rather than
    /// as a blocking in-process loop. A safe no-op when nothing is syncing.
    ///
    /// Every kind of md sync, not just reshape: the capability estimate is a
    /// closed loop, so a scrub cannot keep the old set-once model and still
    /// be capability-relative. It also closes two silent gaps, the resync
    /// after `create` and the recovery after `replace_disk`, which were
    /// governed by no profile at all.
    ///
    /// Unlike `start_sync_throttle` (called once, in the same process that
    /// received the `--priority` flag), this uses `ThrottleController::
    /// resume` to seed the current ceiling from the kernel's REAL value:
    /// each periodic invocation is a brand-new process with no memory of
    /// what a previous tick last wrote. Each band likewise uses ITS OWN
    /// persisted `sync_priority`, falling back to `self.priority` for a band
    /// syncing for a reason this project didn't record (an operator's own
    /// `mdadm --action=check`, or a `state.toml` from an older binary).
    pub fn tick_active_sync(&self) -> Result<usize, OrchestrateError> {
        /// Every `sync_action` md reports for work in progress. Anything
        /// else -- `idle`, `frozen` -- means this band has nothing running.
        const ACTIVE_SYNC_ACTIONS: [&str; 5] = ["reshape", "check", "repair", "resync", "recover"];

        let Some(mut state) = self.store.load()? else {
            return Ok(0);
        };
        let mut ticked = 0usize;
        let mut cleared = false;

        for group_idx in 0..state.groups.len() {
            for band_pos in 0..state.groups[group_idx].bands.len() {
                let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();
                // A band with no live array has nothing to throttle -- skip
                // it, don't abort the whole sweep. See `live_sync_action`,
                // which this ENOENT handling was factored out into once
                // `restore_speed_limit_if_idle` needed the same rule.
                let Some(action) = Self::live_sync_action(self.runner, &md_name)? else {
                    continue;
                };
                if !ACTIVE_SYNC_ACTIONS.contains(&action.as_str()) {
                    cleared |= self.clear_band_limits(&mut state, group_idx, band_pos)?;
                    continue;
                }
                let band_priority = state.groups[group_idx].bands[band_pos]
                    .sync_priority
                    .as_deref()
                    .and_then(SyncPriority::parse)
                    .unwrap_or(self.priority);
                self.tick_sync_throttle(&mut state, group_idx, band_pos, band_priority)?;
                ticked += 1;
            }
        }

        // The other half of this timer's job, and the only one that runs
        // when nothing is syncing: hand the host-wide speed limit back once
        // the operation that borrowed it has finished. Only the fallback
        // path ever borrows it, but a host that has since gained the
        // per-array attributes may still have a value left over from before.
        // `reconcile()` does this too, but nothing guarantees an operator
        // ever runs it -- this timer is what makes the restore automatic.
        let restored = self.restore_speed_limit_if_idle(&mut state)?.is_some();

        if (ticked > 0 || cleared || restored) && !self.runner.is_dry_run() {
            self.store.save(&state)?;
        }
        Ok(ticked)
    }

    /// Execute `ExpansionStep::GrowBand`/`LevelUp`: add `add_members` to an
    /// EXISTING band (`band_index`), optionally promoting its level
    /// (`new_level`), then grow the underlying LVM PV/LV and Btrfs
    /// filesystem to use the new capacity.
    ///
    /// Split into two phases with different rollback guarantees:
    /// - Phase 1 (partition + attach as mdadm spare): safely undoable on
    ///   failure -- nothing here has touched the array's live redundancy.
    /// - Phase 2 (`mdadm --grow`) onward: once `grow` succeeds, the reshape
    ///   is running on the live array. That is the point of no return --
    ///   automating a "rollback" of an in-progress reshape is not something
    ///   this engine attempts (the design principle: when blocked, stop and
    ///   report); any failure from here is reported directly.
    #[allow(clippy::too_many_arguments)]
    fn execute_grow(
        &self,
        parted: &PartedExecutor,
        mdadm: &MdadmExecutor,
        lvm: &LvmExecutor,
        btrfs: &BtrfsExecutor,
        state: &mut StateFile,
        group_idx: usize,
        gpt_initialized: &mut HashSet<String>,
        disks_by_id: &HashMap<&str, &ResolvedDisk>,
        band_index: u8,
        new_level: Option<RaidLevel>,
        add_members: &[DiskId],
        crossed_ponr: &mut bool,
    ) -> Result<(), OrchestrateError> {
        let band_pos = state.groups[group_idx]
            .bands
            .iter()
            .position(|b| b.index == band_index)
            .ok_or_else(|| OrchestrateError::Validation(format!("band {band_index} not found in state")))?;
        let md_name = state.groups[group_idx].bands[band_pos].md_name.clone();

        // This band's geometry is fixed -- plan_expansion never re-slices
        // an existing band, so every new member's partition must match it
        // exactly.
        let (offset, size) = state.groups[group_idx]
            .disks
            .iter()
            .flat_map(|d| d.partitions.iter())
            .find(|p| p.band_index == band_index)
            .map(|p| (p.offset_bytes, p.size_bytes))
            .ok_or_else(|| {
                OrchestrateError::Validation(format!(
                    "band {band_index} has no existing partitions to copy geometry from"
                ))
            })?;
        let part_num = (band_index + 1) as u32;

        let mut journal: Vec<UndoAction> = Vec::new();
        let mut new_members: Vec<(String, StatePartition)> = Vec::new();
        // Same purpose as `create()`'s identically-named set (see its
        // doc comment) -- every kernel device name this grow's OWN new
        // member partitions resolve to, so `stop_any_foreign_holder_before_create`
        // and `rollback` can tell "an array made purely of this grow's own
        // new partitions" apart from one that also spans a disk this
        // request never touched. Declared outside the closure so it
        // survives to the rollback call below on failure.
        let mut target_kernel_names: HashSet<String> = HashSet::new();
        let phase1: Result<(), OrchestrateError> = (|| {
            // Phase A: partition every new member first. `target_kernel_names`
            // must be COMPLETE before any holder check in Phase B runs --
            // otherwise a foreign array spanning two of this grow's own new
            // partitions (e.g. both being re-added after a prior `destroy`
            // left superblocks on both) would be misjudged foreign on
            // whichever partition is checked first, simply because the
            // other one hadn't been carved yet.
            let mut member_specs: Vec<(String, u32)> = Vec::new();
            let mut member_dev_paths: Vec<String> = Vec::new();
            for member_id in add_members {
                let resolved = disks_by_id.get(member_id.as_str()).ok_or_else(|| {
                    OrchestrateError::Validation(format!(
                        "add_member `{member_id}` was not supplied in this expand request"
                    ))
                })?;
                let disk_path = resolve_disk_path(&resolved.id).display().to_string();
                if gpt_initialized.insert(resolved.id.as_str().to_string()) {
                    parted.create_gpt(&disk_path)?;
                }
                parted.add_partition(&disk_path, offset, offset + size - 1)?;
                journal.push(UndoAction::RemovePartition {
                    disk_path: disk_path.clone(),
                    part_num,
                });
                parted.set_raid_flag(&disk_path, part_num)?;

                let part_path = parted.partition_path_for_read(&disk_path, part_num);
                target_kernel_names.insert(part_path.rsplit('/').next().unwrap_or(&part_path).to_string());
                let part_uuid = parted.read_partuuid(&part_path)?;
                parted.settle_udev()?;

                let member_dev_path = format!("/dev/disk/by-partuuid/{part_uuid}");
                member_specs.push((disk_path, part_num));
                member_dev_paths.push(member_dev_path.clone());

                new_members.push((
                    resolved.id.as_str().to_string(),
                    StatePartition {
                        part_uuid,
                        offset_bytes: offset,
                        size_bytes: size,
                        band_index,
                    },
                ));
            }

            // Phase B (reusing the helpers verbatim -- not a
            // parallel implementation): a resurrected foreign superblock
            // auto-assembles onto these same partitions the instant they're
            // carved (identical udev race `create()` hit -- see
            // `stop_any_foreign_holder_before_create`'s doc comment), so the
            // same stop-then-zero-then-use order applies before `mdadm
            // --add`, not just before `mdadm --create`.
            self.stop_any_foreign_holder_before_create(parted, mdadm, &member_specs, &target_kernel_names)?;
            for member_dev_path in &member_dev_paths {
                mdadm.zero_superblock(member_dev_path)?;
            }
            for member_dev_path in member_dev_paths {
                mdadm.add_member(&md_name, &member_dev_path)?;
                journal.push(UndoAction::RemoveSpareMember {
                    md_name: md_name.clone(),
                    member_path: member_dev_path,
                });
            }
            Ok(())
        })();
        if let Err(e) = phase1 {
            return Err(self.wrap_with_rollback(&journal, e, Some(&target_kernel_names)));
        }

        let current_level_str = state.groups[group_idx].bands[band_pos].level.clone();
        let current_count = state.groups[group_idx].bands[band_pos].member_partitions.len();
        let new_count = current_count + add_members.len();
        let level_str = new_level.map(raid_level_str);
        let backup_file = self.prepare_backup_file(&md_name)?;
        if let Err(e) = mdadm.grow(&md_name, level_str, new_count, &backup_file) {
            // An earlier review finding: `mdadm --grow` is not atomic -- a
            // level takeover and a device-count change are separate
            // internal steps, so a nonzero exit does not guarantee the
            // array is still exactly as it was. Verify against its REAL
            // state before assuming the phase-1 spares are still safe to
            // detach; if we can't even verify, don't assume it's safe.
            let unchanged = mdadm
                .level_and_device_count(&md_name)
                .map(|(level, count)| level == current_level_str && count == current_count)
                .unwrap_or(false);
            if !unchanged {
                return Err(OrchestrateError::Rollback {
                    source: Box::new(OrchestrateError::Exec(e)),
                    failures: vec![format!(
                        "mdadm --grow on {md_name} failed but the array's real state no \
                         longer matches what it was before (expected {current_level_str}/\
                         {current_count} members) -- NOT attempting automatic rollback; \
                         inspect `mdadm --detail {md_name}` and `/proc/mdstat` manually"
                    )],
                });
            }
            return Err(self.wrap_with_rollback(
                &journal,
                OrchestrateError::Exec(e),
                Some(&target_kernel_names),
            ));
        }

        // Point of no return from here on (see doc comment above): commit
        // and persist the real change IMMEDIATELY (an earlier review finding
        // F1) -- if pvresize/lvextend/resize_max below fails, the record
        // that the array really did grow must never be lost. `md_uuid` is
        // best-effort (a real UUID stays stable across `--grow`, so a
        // failed re-read here is not itself evidence anything is wrong)
        // and must not abort an already-committed step.
        for (disk_id, partition) in new_members {
            let group = &mut state.groups[group_idx];
            match group.disks.iter().position(|d| d.id == disk_id) {
                Some(i) => group.disks[i].partitions.push(partition),
                None => group
                    .disks
                    .push(new_state_disk(disks_by_id[disk_id.as_str()], vec![partition])),
            }
        }
        if let Some(l) = new_level {
            state.groups[group_idx].bands[band_pos].level = raid_level_str(l).to_string();
        }
        let level_final = new_level.unwrap_or(match state.groups[group_idx].bands[band_pos].level.as_str() {
            "raid1" => RaidLevel::Raid1,
            "raid5" => RaidLevel::Raid5,
            _ => RaidLevel::Raid6,
        });
        // Rebuild the member list from ground truth (every partition across
        // every disk tagged with this band_index) rather than appending,
        // since a disk touched by an earlier step in this same expand()
        // call may already carry an entry here.
        state.groups[group_idx].bands[band_pos].member_partitions = state.groups[group_idx]
            .disks
            .iter()
            .flat_map(|d| d.partitions.iter())
            .filter(|p| p.band_index == band_index)
            .map(|p| p.part_uuid.clone())
            .collect();
        // An earlier review finding: a real md_uuid never changes across a
        // `--grow` (only level/device-count do), so a re-read here is
        // best-effort refresh, not a required update -- but writing its
        // result unconditionally meant a transient read failure nulled out
        // an md_uuid that was ALREADY known-good, and `write_managed_configs`
        // would then delete the live array's real ARRAY line from
        // /etc/mdadm.conf on the next config refresh. Only overwrite on a
        // successful read; keep the existing value otherwise.
        if let Ok(uuid) = mdadm.read_uuid(&md_name) {
            state.groups[group_idx].bands[band_pos].md_uuid = Some(uuid);
        }
        state.groups[group_idx].bands[band_pos].usable_bytes =
            size * level_final.data_members(new_count) as u64;
        // `mdadm --grow` only STARTS the reshape -- the underlying block
        // device's reported size does not increase until the reshape's
        // data movement actually finishes, which real disks can take a
        // long time to do (discovered running this against real mdadm,
        // Phase 4 Step 8 SM-EXPAND-1: `lvextend` fails with "No size
        // change" if attempted mid-reshape). The array itself has already
        // grown for real and is already committed to state.toml below --
        // only the LVM/Btrfs layer's usable capacity is deferred until
        // `sync_action` goes back to `idle`. Recorded as `resize_pending`
        // (an earlier review: must be a persisted, completable record, not a
        // silently-dropped gap) -- `OrchestrationEngine::reconcile` (and
        // every `expand()` call, which reconciles opportunistically before
        // doing anything else) is what actually finishes this later.
        let reshape_still_running = mdadm.sync_action(&md_name)? != "idle";
        state.groups[group_idx].bands[band_pos].resize_pending = reshape_still_running;
        // This band just gained a member, so whatever it was measured doing
        // before describes a different array.
        Self::discard_capability_estimate(&mut state.groups[group_idx].bands[band_pos]);
        *crossed_ponr = true;
        // `start_sync_throttle` persists the profile, which is what the
        // periodic tick reads back instead of silently defaulting.
        if reshape_still_running && !self.runner.is_dry_run() {
            self.start_sync_throttle(state, group_idx, band_pos, self.priority)?;
        }
        if !self.runner.is_dry_run() {
            // NOTE: `state` here is the whole `StateFile` -- every group,
            // not just this one -- so this necessarily also re-persists
            // every OTHER group's `ARRAY`/fstab lines unchanged. That's the
            // multi-group correctness fix, not a side effect to work
            // around: see `write_managed_configs`'s doc comment.
            self.store.save(state)?;
            self.write_managed_configs(state)?;
        }

        if !reshape_still_running {
            let md_dev_path = format!("/dev/{md_name}");
            let group = &state.groups[group_idx];
            lvm.pvresize(&md_dev_path)?;
            lvm.lvextend_max(&group.filesystem.vg_name, &group.filesystem.lv_name)?;
            btrfs.resize_max(&group.filesystem.mount_point)?;
        }

        Ok(())
    }

    /// Execute `ExpansionStep::CreateBand`: partition each member disk for
    /// a brand-new band, `mdadm --create` it, and extend the EXISTING VG
    /// (`vgextend`, not `vgcreate` -- the VG already exists from the
    /// original `create()`) and LV/filesystem to use it.
    ///
    /// Same two-phase rollback split as `execute_grow`: partitions + fresh
    /// mdadm array + fresh PV are safely undoable (nothing shared yet);
    /// once `vgextend` joins the new PV to the shared VG, that's the point
    /// of no return (leaving unused capacity in the VG on a later failure
    /// is safe, unlike an in-progress reshape, so nothing further is rolled
    /// back).
    #[allow(clippy::too_many_arguments)]
    fn execute_create_band(
        &self,
        parted: &PartedExecutor,
        mdadm: &MdadmExecutor,
        lvm: &LvmExecutor,
        btrfs: &BtrfsExecutor,
        state: &mut StateFile,
        group_idx: usize,
        gpt_initialized: &mut HashSet<String>,
        disks_by_id: &HashMap<&str, &ResolvedDisk>,
        band: &RedundantBand,
        reserved_head: u64,
        used_md: &mut HashSet<u32>,
        crossed_ponr: &mut bool,
    ) -> Result<(), OrchestrateError> {
        let part_num = (band.band_index() + 1) as u32;
        let start_offset = reserved_head + band.offset();
        let end_offset = start_offset + band.size();

        let mut journal: Vec<UndoAction> = Vec::new();
        let mut new_partitions: Vec<(String, StatePartition)> = Vec::new();
        let mut member_part_uuids: Vec<String> = Vec::new();
        // Same purpose as `create()`'s identically-named set (see its
        // doc comment) -- every kernel device name this band's OWN new
        // member partitions resolve to. Declared outside the closure so it
        // survives to the rollback call below on failure.
        let mut target_kernel_names: HashSet<String> = HashSet::new();
        // (disk_path, part_num) per member, same shape `create()`
        // builds per-band -- needed by `stop_any_foreign_holder_before_create`
        // to re-resolve each member's live holder via the same
        // `partition_path_for_read` path `mdadm --create`'s member paths
        // come from.
        let mut member_specs: Vec<(String, u32)> = Vec::new();

        let phase1: Result<(String, String), OrchestrateError> = (|| {
            let mut member_part_paths: Vec<String> = Vec::new();
            for member_id in band.members() {
                let disk_path = resolve_disk_path(member_id).display().to_string();
                if gpt_initialized.insert(member_id.as_str().to_string()) {
                    parted.create_gpt(&disk_path)?;
                }
                parted.add_partition(&disk_path, start_offset, end_offset - 1)?;
                journal.push(UndoAction::RemovePartition {
                    disk_path: disk_path.clone(),
                    part_num,
                });
                parted.set_raid_flag(&disk_path, part_num)?;

                let part_path = parted.partition_path_for_read(&disk_path, part_num);
                target_kernel_names.insert(part_path.rsplit('/').next().unwrap_or(&part_path).to_string());
                member_specs.push((disk_path, part_num));
                let part_uuid = parted.read_partuuid(&part_path)?;
                member_part_uuids.push(part_uuid.clone());
                member_part_paths.push(format!("/dev/disk/by-partuuid/{part_uuid}"));
                new_partitions.push((
                    member_id.as_str().to_string(),
                    StatePartition {
                        part_uuid,
                        offset_bytes: start_offset,
                        size_bytes: band.size(),
                        band_index: band.band_index(),
                    },
                ));
            }
            parted.settle_udev()?;

            // Reusing the helpers verbatim: this band's new
            // partitions are just as exposed to a resurrected foreign
            // superblock as `create()`'s initial bands are (same udev
            // incremental-assembly race, see
            // `stop_any_foreign_holder_before_create`'s doc comment) --
            // stop any self-contained holder and zero before `mdadm
            // --create`, refuse (never proceed, never stop) if the holder
            // also spans a disk outside this request.
            self.stop_any_foreign_holder_before_create(parted, mdadm, &member_specs, &target_kernel_names)?;
            for member in &member_part_paths {
                mdadm.zero_superblock(member)?;
            }

            // Host-wide-unique, NOT `format!("md{}", band.band_index())` --
            // band indices restart at 0 within every group, so that scheme
            // would give this brand-new band the SAME `/dev/mdN` as some
            // other group's band0. See `allocate_md_name`'s doc comment.
            let md_name = allocate_md_name(used_md);
            let level_str = raid_level_str(band.level());
            let member_refs: Vec<&str> = member_part_paths.iter().map(AsRef::as_ref).collect();
            mdadm.create_array(&md_name, level_str, &member_refs)?;
            journal.push(UndoAction::TeardownArray {
                md_name: md_name.clone(),
                member_paths: member_part_paths,
            });

            let md_dev_path = format!("/dev/{md_name}");
            lvm.pvcreate(&md_dev_path)?;
            journal.push(UndoAction::RemovePv {
                dev_path: md_dev_path.clone(),
            });

            Ok((md_name, md_dev_path))
        })();

        let (md_name, md_dev_path) = match phase1 {
            Ok(v) => v,
            Err(e) => return Err(self.wrap_with_rollback(&journal, e, Some(&target_kernel_names))),
        };

        if let Err(e) = lvm.vgextend(&state.groups[group_idx].filesystem.vg_name, &md_dev_path) {
            // An earlier review finding: vgextend writes metadata across
            // every PV in the VG and can fail partway through committing.
            // Blindly running the journal here would `pvremove -ff -y` a PV
            // that may have already joined the shared, LIVE vg -- that
            // flag combination force-wipes a PV's label even if LVM
            // believes it belongs to a VG, which would corrupt vg metadata
            // out from under the user's existing, unrelated data. Probe
            // reality first.
            let actually_joined = lvm
                .pv_vg_name(&md_dev_path)
                .map(|vg| vg == state.groups[group_idx].filesystem.vg_name)
                .unwrap_or(false);
            if actually_joined {
                return Err(OrchestrateError::Rollback {
                    source: Box::new(OrchestrateError::Exec(e)),
                    failures: vec![format!(
                        "vgextend reported failure but {md_dev_path} now shows as a member \
                         of {} -- the VG may have partially accepted the new PV; inspect \
                         `vgs`/`pvs` manually before retrying, do NOT run `pvremove` on it",
                        state.groups[group_idx].filesystem.vg_name
                    )],
                });
            }
            return Err(self.wrap_with_rollback(
                &journal,
                OrchestrateError::Exec(e),
                Some(&target_kernel_names),
            ));
        }

        // Point of no return from here on (see doc comment above): commit
        // and persist immediately (an earlier review finding F1), before
        // lvextend/resize_max/read_uuid -- any of which failing must never
        // cost us the record that this band now really exists.
        for (disk_id, partition) in new_partitions {
            let group = &mut state.groups[group_idx];
            match group.disks.iter().position(|d| d.id == disk_id) {
                Some(i) => group.disks[i].partitions.push(partition),
                None => group
                    .disks
                    .push(new_state_disk(disks_by_id[disk_id.as_str()], vec![partition])),
            }
        }
        state.groups[group_idx].bands.push(StateBand {
            index: band.band_index(),
            level: raid_level_str(band.level()).to_string(),
            md_name: md_name.clone(),
            md_uuid: mdadm.read_uuid(&md_name).ok(), // best-effort, see execute_grow
            member_partitions: member_part_uuids,
            usable_bytes: band.usable_bytes(),
            ..Default::default()
        });
        *crossed_ponr = true;
        if !self.runner.is_dry_run() {
            // See the identical note in `execute_grow`: `state` is the
            // whole `StateFile`, so every other group's managed config
            // lines are preserved by construction, not by coincidence.
            self.store.save(state)?;
            self.write_managed_configs(state)?;
        }

        let group = &state.groups[group_idx];
        lvm.lvextend_max(&group.filesystem.vg_name, &group.filesystem.lv_name)?;
        btrfs.resize_max(&group.filesystem.mount_point)?;

        Ok(())
    }
}

/// How long since a band's last successfully COMPLETED scrub before
/// `expand()` requires a fresh one (the design safety table; ruled out
/// forcing a scrub immediately before EVERY expand as impractical, so this is
/// a staleness window instead). 30 days matches
/// this project's other RAID tooling conventions (a monthly scrub cadence
/// is the common default for `mdadm`/`btrfs` alike).
pub const SCRUB_FRESHNESS_DAYS: i64 = 30;

/// The reserved snapshot-name namespace for `snapshot_auto_run`'s own
/// automated snapshots -- see `snapshot_create`'s rejection of this prefix
/// and `prune_group_snapshots`'s doc comment for why this reservation is
/// what makes pruning provably never delete a snapshot an operator made by
/// hand.
pub const AUTO_SNAPSHOT_PREFIX: &str = "auto-";

/// Why `band.last_scrub` fails the pre-reshape safety check. The four ways
/// it can fail need four different things from the operator, and reporting
/// them with one "not checked in the last 30 days" sentence sent whoever hit
/// it looking for a scrub they had supposedly already run -- most often on a
/// group created minutes earlier, which has no history at all.
enum ScrubStaleness {
    /// No `last_scrub` record. What every freshly created group looks like:
    /// `create()` writes `last_scrub: None`, and the initial mdadm sync that
    /// follows writes parity rather than verifying anything, so it is not
    /// recorded as (and is not) a scrub.
    NeverScrubbed,
    /// A record exists but the scrub did not run to the end, so it proves
    /// nothing about the array.
    DidNotComplete(ScrubOutcome),
    /// Completed, but longer ago than `SCRUB_FRESHNESS_DAYS`.
    Stale { days: i64 },
    /// `finished_at` will not parse. Should never happen (this engine writes
    /// it with `Utc::now().to_rfc3339()`), and an age that cannot be read is
    /// treated as failing rather than passing -- the same "unknown never
    /// means safe" rule that applies to throttle metrics.
    UnreadableTimestamp,
}

impl ScrubStaleness {
    /// The half of the refusal that differs per cause. `expand()` supplies
    /// the band and the shared advice around it.
    fn describe(&self) -> String {
        match self {
            Self::NeverScrubbed => "has never been checked for errors (a group starts with no scrub \
                 history: the initial sync writes redundancy, it does not verify what it reads)"
                .to_string(),
            Self::DidNotComplete(outcome) => {
                let what = match outcome {
                    ScrubOutcome::Cancelled => "was cancelled",
                    ScrubOutcome::Failed => "failed",
                    // Unreachable: `scrub_staleness` only builds this variant
                    // for an outcome that is not `Completed`.
                    ScrubOutcome::Completed => "did not finish",
                };
                format!(
                    "has no completed error check: the last one {what} before it finished, so it \
                     proves nothing about the array"
                )
            }
            Self::Stale { days } => format!(
                "was last checked for errors {days} days ago, past the {SCRUB_FRESHNESS_DAYS}-day limit"
            ),
            Self::UnreadableTimestamp => {
                "has a scrub record whose finish time cannot be read, so its age cannot be trusted"
                    .to_string()
            }
        }
    }
}

/// `None` when `band.last_scrub` satisfies the pre-reshape safety check --
/// it must exist, must have `ScrubOutcome::Completed`, and must be within
/// `SCRUB_FRESHNESS_DAYS`.
fn scrub_staleness(band: &StateBand) -> Option<ScrubStaleness> {
    let Some(scrub) = &band.last_scrub else {
        return Some(ScrubStaleness::NeverScrubbed);
    };
    if scrub.outcome != ScrubOutcome::Completed {
        return Some(ScrubStaleness::DidNotComplete(scrub.outcome));
    }
    let Ok(finished) = chrono::DateTime::parse_from_rfc3339(&scrub.finished_at) else {
        return Some(ScrubStaleness::UnreadableTimestamp);
    };
    let age = Utc::now().signed_duration_since(finished);
    if age > chrono::Duration::days(SCRUB_FRESHNESS_DAYS) {
        return Some(ScrubStaleness::Stale { days: age.num_days() });
    }
    None
}

/// The system mountpoint currently reachable from `kernel_name` through
/// ANY depth of stacking, if any -- read live, via
/// `lsblk -n -o MOUNTPOINT /dev/<kernel_name>`.
///
/// This used to parse `/proc/mounts` and match its device names against
/// `kernel_name` as a prefix (`sda` -> `sda1`, `nvme0n1` -> `nvme0n1p1`).
/// That can only ever see a filesystem mounted DIRECTLY off a partition of
/// the disk, so it silently never fired on the two layouts most worth
/// protecting: an md RAID root, whose mount source reads `/dev/mdN`, and an
/// LVM root, whose mount source reads `/dev/mapper/<vg>-<lv>`. Neither
/// string contains the disk's kernel name at all. Measured on a real
/// RAID1-root guest with `/`, `/boot` and `/boot/efi` all on `sda`:
/// `grep -c '^/dev/sda' /proc/mounts` returned **0**, so the whole check
/// was dead code in exactly the configuration it existed for.
///
/// `lsblk` walks the holder tree, so one invocation reports the
/// mountpoints of every md array, LVM volume and plain partition layered
/// on the disk. Asking for only the MOUNTPOINT column keeps the output one
/// value per line (blank for unmounted nodes) with none of the tree-drawing
/// characters a NAME column would bring, so there is nothing to parse
/// beyond trimming. Matching goes through `shr_inspect::is_system_mountpoint`
/// -- the SAME predicate `preflight_write_targets` uses -- so the live gate
/// and the preflight gate can no longer disagree about what counts as a
/// system mountpoint.
///
/// Still cheap and trivially mockable: one command per target disk, and
/// every existing test's default, unstubbed `lsblk` response is empty,
/// which correctly reads as "nothing mounted, no system disk found here".
fn live_system_mountpoint_on(
    runner: &dyn CommandRunner,
    kernel_name: &str,
) -> Result<Option<String>, OrchestrateError> {
    let dev = format!("/dev/{kernel_name}");
    let out = runner.run("lsblk", &["-n", "-o", "MOUNTPOINT", &dev])?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .find(|mp| !mp.is_empty() && is_system_mountpoint(mp))
        .map(str::to_string))
}

/// Drop `StateRetiredArray` entries whose disks are being taken by a
/// `create`/`expand` that is about to repartition them.
///
/// A retired entry exists to stop a DEAD array from being auto-assembled off
/// superblocks still sitting on its old disks. The moment those disks are
/// handed to a new group, `create`/`expand` cut fresh partitions and
/// `mdadm --create` writes new superblocks over them, so there is no longer
/// anything for the old entry to suppress -- keeping it would leave
/// `mdadm.conf` accumulating `<ignore>` lines for arrays whose last physical
/// trace is gone.
///
/// Matched on ANY overlap rather than full containment: a retired array's
/// members are spread across all of that group's disks, so reusing even one
/// of them is enough to make the old array unassemblable anyway.
fn prune_retired_arrays_for(state: &mut StateFile, taken: &[ResolvedDisk]) {
    if state.retired_arrays.is_empty() {
        return;
    }
    let taken_ids: HashSet<&str> = taken.iter().map(|d| d.id.as_str()).collect();
    state
        .retired_arrays
        .retain(|r| !r.disk_ids.iter().any(|id| taken_ids.contains(id.as_str())));
}

/// Every mdN device NUMBER already claimed by a band in ANY group recorded
/// in `state` -- one of the two inputs `allocate_md_name`'s caller must
/// union together (see `host_md_numbers` for the other). Multi-group
/// correctness trap: band indices are local to a group (each group's bands
/// are numbered starting at 0 again), so deriving `md_name` from
/// `band_index()` alone (`format!("md{}", ...)`, the pre-multi-group
/// scheme) would give group A's band0 and group B's band0 the SAME name --
/// mdadm's `/dev/mdN` namespace is global across the whole host and has no
/// concept of "which group this belongs to" to disambiguate them.
///
/// This alone is NOT sufficient to pick a safe name, though: it only knows
/// about bands shr-rs itself put in `state.toml`. See `host_md_numbers`.
fn used_md_numbers(state: &StateFile) -> HashSet<u32> {
    state
        .groups
        .iter()
        .flat_map(|g| g.bands.iter())
        .filter_map(|b| b.md_name.strip_prefix("md").and_then(|n| n.parse::<u32>().ok()))
        .collect()
}

/// Every mdN device NUMBER that currently exists on the HOST, regardless of
/// whether shr-rs manages it. `/dev/mdN` is a host-global kernel namespace,
/// not something shr-rs owns exclusively: a foreign array assembled from a
/// previous OS install, a stray superblock the kernel auto-assembled as
/// `md127`, or an array some other tool created all claim a number the same
/// way a shr-rs-managed band does. `used_md_numbers` (state.toml alone)
/// cannot see any of these, so relying on it exclusively lets
/// `allocate_md_name` hand out a name mdadm will then refuse at `--create`
/// time -- AFTER partitions were already cut, forcing a rollback for a
/// mistake that was entirely avoidable up front.
///
/// Read via `self.runner` (a `cat /proc/mdstat`), the same pattern
/// `MdadmExecutor::degraded_count`/`sync_action` use for sysfs reads --
/// never a raw `std::fs` call, both so this stays mockable in tests and so
/// it doesn't unconditionally IO-error on the Windows dev host `cargo test`
/// runs on natively (no `/proc/mdstat` there at all). Parsing is delegated
/// to `shr_inspect::parse_mdstat` rather than a second hand-rolled parser --
/// that crate already parses this exact file for `shr-rs status`.
///
/// A no-op (returns empty) under dry-run: a dry-run's job is to report what
/// planning would produce from the disks/state it was given, not to vary
/// its (never-persisted, never-executed) output depending on some other
/// array that happens to already exist on whatever host the simulation is
/// running on -- and the project's dry-run rule is that it touches nothing
/// real, which a live `/proc/mdstat` read arguably isn't as clear-cut about
/// as a destructive command, but is still unnecessary I/O for output that
/// is discarded.
fn host_md_numbers(runner: &dyn CommandRunner) -> Result<HashSet<u32>, OrchestrateError> {
    if runner.is_dry_run() {
        return Ok(HashSet::new());
    }
    let output = runner.run("cat", &["/proc/mdstat"])?;
    let mdstat = parse_mdstat(&output.stdout);
    Ok(mdstat
        .arrays
        .iter()
        .filter_map(|a| a.name.strip_prefix("md").and_then(|n| n.parse::<u32>().ok()))
        .collect())
}

/// Claim and return the smallest `mdN` name not already in `used`. The
/// caller seeds `used` from `used_md_numbers` and keeps threading the SAME
/// set through every band allocated within one `create()`/`expand()` call,
/// so bands created together also never collide with EACH OTHER -- not just
/// with bands that already existed on disk before the call started (at the
/// point the first of several new bands is allocated, none of them are in
/// `state.toml`'s groups yet, so re-scanning `state` fresh for each one
/// would hand out the same number to all of them).
fn allocate_md_name(used: &mut HashSet<u32>) -> String {
    let mut n = 0u32;
    while used.contains(&n) {
        n += 1;
    }
    used.insert(n);
    format!("md{n}")
}

fn raid_level_str(level: RaidLevel) -> &'static str {
    match level {
        RaidLevel::Raid1 => "raid1",
        RaidLevel::Raid5 => "raid5",
        RaidLevel::Raid6 => "raid6",
    }
}

/// Shrink a `ResolvedDisk` down to what `StateExpansion::new_disks`
/// persists -- enough to reconstruct a planner-usable disk on resume (D3
/// identity + size/serial/model), not the full struct (`reference`/
/// `system_mounts`/`has_content` are resolution-time-only concerns with no
/// bearing on replaying an already-computed plan).
fn pending_disk_from_resolved(d: &ResolvedDisk) -> StatePendingDisk {
    StatePendingDisk {
        id: d.id.as_str().to_string(),
        kernel_name: d.kernel_name.clone(),
        size_bytes: d.size_bytes,
        serial: (!d.serial.is_empty()).then(|| d.serial.clone()),
        model: (!d.model.is_empty()).then(|| d.model.clone()),
    }
}

/// The inverse of `pending_disk_from_resolved`, for `resume_expand`.
/// `reference`/`system_mounts`/`has_content` are reconstructed as
/// best-effort placeholders: `execute_grow`/`execute_create_band` never
/// read them (they resolve real disk paths from `id` alone via
/// `resolve_disk_path`), so a resumed plan's behavior does not depend on
/// these fields being real.
fn resolved_disk_from_pending(p: &StatePendingDisk) -> ResolvedDisk {
    ResolvedDisk {
        reference: DiskRef::Path(p.kernel_name.clone()),
        kernel_name: p.kernel_name.clone(),
        id: DiskId::from(p.id.clone()),
        size_bytes: p.size_bytes,
        serial: p.serial.clone().unwrap_or_default(),
        model: p.model.clone().unwrap_or_default(),
        system_mounts: Vec::new(),
        has_content: false,
    }
}

fn new_state_disk(resolved: &ResolvedDisk, partitions: Vec<StatePartition>) -> StateDisk {
    StateDisk {
        id: resolved.id.as_str().to_string(),
        size_bytes: resolved.size_bytes,
        serial: (!resolved.serial.is_empty()).then(|| resolved.serial.clone()),
        model: (!resolved.model.is_empty()).then(|| resolved.model.clone()),
        added_at: Utc::now().to_rfc3339(),
        partitions,
    }
}

/// Fallback tier for `replace_disk`'s `--old`: resolve a live kernel name
/// (`sdc`, `/dev/sdc`) by `readlink -e`-ing each recorded disk's
/// `/dev/disk/by-id/<id>` symlink and comparing the target's trailing
/// segment. Only reachable once by-id/serial matching (`state.toml` alone)
/// has already failed, since this requires the disk to be physically
/// present and enumerable right now -- it can never find an already-gone
/// disk, unlike the by-id/serial tiers. A dangling symlink (disk absent) or
/// any other `readlink` failure is treated as "doesn't match", same as
/// `MdadmExecutor::resolve_member_kernel_name`.
fn find_disk_by_live_kernel_name<'a>(
    runner: &dyn CommandRunner,
    disks: &'a [StateDisk],
    kernel_ref: &str,
) -> Option<&'a StateDisk> {
    let wanted = kernel_ref.trim().trim_start_matches("/dev/");
    disks.iter().find(|d| {
        let by_id_path = resolve_disk_path(&DiskId::from(d.id.as_str()));
        match runner.run("readlink", &["-e", &by_id_path.display().to_string()]) {
            Ok(out) => out.stdout.trim().rsplit('/').next() == Some(wanted),
            Err(_) => false,
        }
    })
}

fn step_band_index(step: &ExpansionStep) -> Option<u8> {
    match step {
        ExpansionStep::LevelUp { band_index, .. } => Some(*band_index),
        ExpansionStep::GrowBand { band_index, .. } => Some(*band_index),
        ExpansionStep::CreateBand { band } => Some(band.band_index()),
        ExpansionStep::MarkUnusable { .. } => None,
    }
}

fn describe_step(step: &ExpansionStep) -> String {
    match step {
        ExpansionStep::LevelUp {
            band_index,
            from,
            to,
            add_members,
        } => format!(
            "band {band_index}: level up {from:?} -> {to:?}, adding {} member(s)",
            add_members.len()
        ),
        ExpansionStep::GrowBand {
            band_index,
            add_members,
        } => {
            format!("band {band_index}: growing by {} member(s)", add_members.len())
        }
        ExpansionStep::CreateBand { band } => format!(
            "creating band {} ({:?}, {} member(s))",
            band.band_index(),
            band.level(),
            band.members().len()
        ),
        ExpansionStep::MarkUnusable { disk, size, .. } => {
            format!("marking {size} bytes on {disk} as unusable")
        }
    }
}

/// Reconstruct the pure planner's view of the live layout from `state.toml`
/// so `plan_expansion` can recompute the ideal layout on the SAME grid the
/// array was originally created with.
///
/// `band_alignment`/`reserved_head`/`reserved_tail` are not yet persisted in
/// `ArrayState` -- every array `create()` produces today uses
/// `PlannerInput::new`'s defaults, so hardcoding them here is correct for
/// every array that exists, but would need real persistence (a schema
/// addition) if per-array grid tuning is ever exposed. Documented gap.
fn snapshot_from_state(state: &ArrayState) -> Result<LayoutSnapshot, OrchestrateError> {
    let mode = match state.mode.as_str() {
        "shr" => RedundancyMode::Shr,
        "shr2" => RedundancyMode::Shr2,
        other => {
            return Err(OrchestrateError::Validation(format!(
                "unknown mode `{other}` in the shr-rs configuration"
            )))
        }
    };

    let disks: Vec<shr_core::Disk> = state
        .disks
        .iter()
        .map(|d| {
            let mut disk = shr_core::Disk::new(DiskId::from(d.id.clone()), d.size_bytes);
            if let (Some(serial), Some(model)) = (&d.serial, &d.model) {
                disk = disk.with_meta(serial.clone(), model.clone());
            }
            disk
        })
        .collect();

    let mut bands = Vec::new();
    for b in &state.bands {
        let level = match b.level.as_str() {
            "raid1" => RaidLevel::Raid1,
            "raid5" => RaidLevel::Raid5,
            "raid6" => RaidLevel::Raid6,
            other => {
                return Err(OrchestrateError::Validation(format!(
                    "unknown level `{other}` for band {}",
                    b.index
                )))
            }
        };

        let mut members = Vec::new();
        let mut geometry: Option<(u64, u64)> = None;
        for uuid in &b.member_partitions {
            let owner = state
                .disks
                .iter()
                .find(|d| d.partitions.iter().any(|p| &p.part_uuid == uuid))
                .ok_or_else(|| {
                    OrchestrateError::Validation(format!(
                        "band {} member partition {uuid} not found on any disk",
                        b.index
                    ))
                })?;
            members.push(DiskId::from(owner.id.clone()));
            let part = owner.partitions.iter().find(|p| &p.part_uuid == uuid).unwrap();
            geometry.get_or_insert((part.offset_bytes, part.size_bytes));
        }
        let (absolute_offset, size) = geometry
            .ok_or_else(|| OrchestrateError::Validation(format!("band {} has no members", b.index)))?;
        // `StatePartition.offset_bytes` is the absolute physical offset on
        // disk (reserved_head + the planner's band.offset(), per create()'s
        // `let start_offset = reserved_head + band.offset()`). The pure
        // planner's own coordinate space is relative to the start of usable
        // space (0-based, right after reserved_head) -- convert back, or
        // `validate_snapshot` rejects band 0 for not starting at offset 0.
        let offset = absolute_offset.saturating_sub(shr_core::DEFAULT_RESERVED_HEAD);

        let band = RedundantBand::from_parts(b.index, offset, size, members, level)
            .map_err(|e| OrchestrateError::Validation(format!("band {} is invalid: {e}", b.index)))?;
        bands.push(band);
    }

    Ok(LayoutSnapshot {
        disks,
        bands,
        mode,
        layout_version: state.layout_version as u64,
        band_alignment: shr_core::DEFAULT_BAND_ALIGNMENT,
        reserved_head: shr_core::DEFAULT_RESERVED_HEAD,
        reserved_tail: shr_core::DEFAULT_RESERVED_TAIL,
    })
}
