//! `shr-rs` CLI. A thin frontend over `shr-command` and `shr-orchestrate`: it parses arguments,
//! calls the shared Command API / Orchestration Engine, and prints either human text or `--json`
//! (the machine contract the Cockpit plugin consumes). No business logic here.
//!
//! This crate is a library, not a binary -- `shr-bin` is the actual `shr-rs` entry point and
//! calls [`run`] after deciding (via `shr_command::detect_ui_mode`) that CLI mode, not the TUI, is
//! what this invocation wants. `run` takes an explicit argument list (not `std::env::args()`
//! directly) so `shr-bin` can hand it exactly the argv it received, unmodified.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use shr_command::{
    build_fs_df, build_plan_report, build_status, preflight_create, render, system_disk_aliases,
    AlwaysConfirmSink, RecordingProgressSink, TextProgressSink,
};
use shr_core::{plan_initial, Disk, PlannerInput, RedundancyMode};
use shr_exec::{CommandRunner, SystemRunner};
use shr_inspect::{resolve_disk_ref, DiskRef, Inspector, ResolvedDisk, SystemInspector, WriteBlocker};
use shr_orchestrate::{CreateRequest, ExpandRequest, OrchestrationEngine, ReconcileAction, ReconcileOutcome};
use shr_state::{policy::PolicyStore, ArrayState, NotifyPolicy, StateStore};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Where every `status`/`create`/`expand`/... handler in this file loads and
/// saves `state.toml` via `StateStore::new`. Named (rather than left as
/// repeated string literals) so `status --json`'s `state_path` field
/// can report exactly this constant -- a second, independently-typed copy of
/// the same literal could silently drift from the path actually used.
const STATE_PATH: &str = "/var/lib/shr-rs/state.toml";
const STATE_LOCK_PATH: &str = "/var/lib/shr-rs/.shr-rs.lock";

/// Error context for a failed state load. Names the file by path rather than
/// by its internal name: `std::fs`'s io errors carry no path of their own, so
/// without this the user is told only "No such file or directory".
fn state_load_ctx() -> String {
    format!("reading the shr-rs configuration ({STATE_PATH})")
}

const MDADM_CONF_PATH: &str = "/etc/mdadm.conf";
const FSTAB_PATH: &str = "/etc/fstab";
/// Where `schedule install`/`destroy` read and write shr-rs-generated
/// systemd unit files -- the standard host-wide unit
/// search path, same as `mdadm.conf`/`fstab`'s hardcoded `/etc` locations.
const SYSTEMD_UNIT_DIR: &str = "/etc/systemd/system";
/// Operator-authored policy (`[notify]`'s webhook URL/
/// `systemd_notify` on-off, `[snapshot]`'s schedule/retention) --
/// deliberately NOT under `/var/lib/shr-rs` (that's `state.toml`'s home,
/// machine-maintained state this project rewrites wholesale on every
/// write path) and NOT `state.toml` itself, even though state.toml is
/// already 0600 -- see `shr_state::policy`'s doc comment for the full
/// reasoning. Same 0600-equivalent expectation as state.toml (a webhook
/// URL commonly embeds a bearer token); `PolicyStore` doesn't chmod this
/// file itself since it's operator-authored/deployed, not
/// shr-rs-generated, matching a typical `/etc/<app>/config.toml`
/// convention -- the report documents this expectation for deployment.
const NOTIFY_POLICY_PATH: &str = "/etc/shr-rs/policy.toml";

/// Load the WHOLE policy file (`[notify]` + `[snapshot]`), falling back to
/// `PolicyFile::default()` on ANY load failure -- including a malformed
/// file -- rather than propagating it. A broken policy config must not be
/// able to block `create`/`expand`/`reconcile`/scrub commands; that would
/// be the exact "alerting breaks the main operation" class of bug the notification
/// wiring exists to prevent, just moved one step earlier (at config-load time
/// instead of delivery time) -- and now applies to `[snapshot]` the same
/// way.
fn load_policy_file() -> shr_state::policy::PolicyFile {
    match PolicyStore::new(NOTIFY_POLICY_PATH).load() {
        Ok(policy) => policy,
        Err(e) => {
            eprintln!("warning: could not load {NOTIFY_POLICY_PATH} ({e:#}); policy uses defaults");
            shr_state::policy::PolicyFile::default()
        }
    }
}

/// Load just the notification half of the policy file -- see
/// `load_policy_file`'s doc comment for the shared fallback behavior.
fn load_notify_policy() -> NotifyPolicy {
    load_policy_file().notify
}

/// Build the engine every command that writes real system config
/// (create/expand/reconcile/recompress) uses -- ONE place wires
/// `.with_conf_paths` at the real `/etc` locations (D8) so a new call site
/// can't silently forget it the way `Recompress` did (real-VM
/// repro): state.toml and the live mount both updated correctly on
/// `recompress`, but that call site built its own `OrchestrationEngine`
/// inline without this call, so `write_fstab` kept writing to the engine's
/// tempdir-adjacent default path instead of `/etc/fstab` -- silently, with
/// no error, since that path is just as writable. Also wires the
/// notify policy for the same "one place, can't be forgotten" reason.
fn production_engine(sys_runner: &SystemRunner, state_store: Arc<StateStore>) -> OrchestrationEngine<'_> {
    OrchestrationEngine::new(sys_runner, state_store)
        .with_conf_paths(MDADM_CONF_PATH, FSTAB_PATH)
        .with_unit_dir(SYSTEMD_UNIT_DIR)
        .with_notify_policy(load_notify_policy())
}

/// Exclusive, non-blocking lock guarding every real (non-dry-run)
/// create/expand/reconcile invocation (an earlier review finding: nothing
/// previously stopped two `shr-rs` processes from racing on the same
/// disks or on `state.toml`'s own tmp-write path). Dry-run never takes
/// this lock -- it never writes anything, and `StateStore::load`'s atomic-
/// rename read is already safe against a concurrent writer on its own; the
/// actual risk this closes is two WRITERS racing on the same `.tmp` path.
/// Fails fast rather than blocking: a hung first process must not silently
/// wedge the second one forever.
/// Stamp a status report with the `state.toml` path this invocation actually
/// resolved (the dashboard couldn't show which state file it was
/// looking at, since `status --json` never carried one). `build_status`
/// itself can't set this -- it has no filesystem access and never learns
/// which path `state` was loaded from -- so the CLI, which does the actual
/// `StateStore::new(STATE_PATH)` call, attaches it here instead. A separate,
/// pure function (rather than inlining the assignment at the `Status`
/// handler) so this exact behavior -- "the constant that gets stamped in is
/// the same one `StateStore::new` was actually called with, not a second,
/// possibly-drifted copy" -- is unit-testable without `SystemInspector`,
/// which can't run against a real host from this crate's tests on a
/// non-Linux dev machine.
fn attach_state_path(mut report: shr_command::StatusReport) -> shr_command::StatusReport {
    report.state_path = Some(STATE_PATH.to_string());
    report
}

/// What a periodic timer prints when `state.toml`'s lock is already held, in
/// place of the work it skipped.
///
/// The interactive commands turn lock contention into an ERROR, which is
/// right for an operator who typed a command and needs to know it did not
/// run. A timer is the opposite case: `shr-rs-throttle-tick.timer` fires
/// every two minutes, so a multi-hour `expand` would leave a trail of failed
/// units for a condition that is entirely normal and resolves itself. This
/// exits 0 and says what happened; the next firing picks the work up.
///
/// `base` is the arm's own success payload, so `--json` consumers keep every
/// key they already parse (`bands_ticked`, `ok`, `snapshots`) rather than
/// getting a differently-shaped object on a skipped run. `skipped` is what
/// tells them the zeroes mean "not attempted", not "nothing to do".
fn report_tick_skipped(json: bool, base: serde_json::Value) {
    if json {
        println!("{}", tick_skipped_report_json(base));
    } else {
        println!(
            "another shr-rs command is working on the state file right now; \
             skipping this run (lock: {STATE_LOCK_PATH})"
        );
    }
}

/// `report_tick_skipped`'s `--json` shape, split out so the "every existing
/// key survives, `skipped` is added alongside" contract is testable without
/// capturing stdout.
fn tick_skipped_report_json(mut base: serde_json::Value) -> serde_json::Value {
    base["skipped"] = serde_json::json!(true);
    base
}

fn acquire_state_lock() -> Result<fd_lock::RwLock<std::fs::File>> {
    let lock_path = PathBuf::from(STATE_LOCK_PATH);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;
    Ok(fd_lock::RwLock::new(file))
}

#[derive(Parser)]
#[command(
    name = "shr-rs",
    version,
    about = "Pool disks of different sizes into one protected storage space"
)]
struct Cli {
    /// Print machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show disks, RAID arrays and groups (read-only).
    Status {
        // No effect on `--json`, which always carries every one of these
        // fields on `StatusReport.groups[].bands[]` regardless of this flag
        // (see `shr_command::report::GroupBandStatus`), and ignored under
        // `--watch`, whose frame already shows band-level detail.
        /// Show per-band detail (members, sync progress, last scrub) instead
        /// of the compact summary.
        #[arg(long)]
        detail: bool,
        // Requires a real terminal: rejected outright when stdout has no size
        // (a pipe/redirect) rather than looping forever writing frames into
        // it. Rejected with `--json` too -- a live redraw loop has no defined
        // JSON streaming shape, so the combination is refused, not guessed at.
        /// Redraw the status in place every few seconds until interrupted
        /// (Ctrl-C). Needs a real terminal.
        #[arg(long)]
        watch: bool,
        /// Refresh interval for `--watch`, in seconds.
        #[arg(long, default_value_t = 2)]
        interval_secs: u64,
    },
    /// Try out a layout before committing to it.
    Plan {
        #[command(subcommand)]
        plan: PlanCmd,
    },
    /// Check whether disks are safe to use for `create`/`expand`. Changes nothing.
    Preflight {
        /// Disks to use. Accepts `sdb`, `/dev/sdb`, a `/dev/disk/by-id` name,
        /// or a serial number.
        #[arg(long, value_delimiter = ',', required = true)]
        disks: Vec<String>,
        /// Allow a disk that already holds partitions or a filesystem.
        #[arg(long)]
        force_content: bool,
    },
    /// Create a new storage group across the given disks.
    Create {
        /// Redundancy mode.
        #[arg(long, value_enum)]
        mode: ModeArg,
        /// Disks to use. Accepts `sdb`, `/dev/sdb`, a `/dev/disk/by-id` name,
        /// or a serial number.
        #[arg(long, value_delimiter = ',', required = true)]
        disks: Vec<String>,
        // "default" is also the name a pre-multi-group state file is migrated
        // to on load, so a single-group host gets the same identifier either
        // way.
        /// Name for this group. Several groups can coexist on one host, so
        /// the name must be unique and none of `--disks` may already belong
        /// to another group.
        #[arg(long, default_value = "default")]
        name: String,
        /// Target mount point.
        #[arg(long, default_value = "/mnt/shr_data")]
        mount: String,
        /// LVM Volume Group name.
        #[arg(long, default_value = "shr_vg")]
        vg_name: String,
        /// LVM Logical Volume name.
        #[arg(long, default_value = "data")]
        lv_name: String,
        /// Compression algorithm.
        #[arg(long, default_value = "zstd:3")]
        compression: String,
        /// Print the commands that would run, without running any of them.
        #[arg(long)]
        dry_run: bool,
        /// Allow a disk that already holds partitions or a filesystem.
        /// Blocked by default -- wiping a disk with real content on it must
        /// be explicit.
        #[arg(long)]
        force_content: bool,
        /// Skip the "type the exact group name" confirmation prompt (for
        /// scripts and automation).
        #[arg(long)]
        yes: bool,
    },
    /// Add disk(s) to an existing group to enlarge it.
    Expand {
        /// Disks to add. Accepts `sdb`, `/dev/sdb`, a `/dev/disk/by-id` name,
        /// or a serial number.
        #[arg(long = "add", value_delimiter = ',', required = true)]
        disks: Vec<String>,
        // Required as soon as a second group exists, so an operator can never
        // expand the wrong group just by omitting it.
        /// Which group to expand (`shr-rs groups` lists the names). Optional
        /// only while there is exactly one group.
        #[arg(long)]
        name: Option<String>,
        /// How much of the disks' speed the rebuild may take from everyday
        /// file access while it runs.
        #[arg(long, value_enum, default_value = "balanced")]
        priority: PriorityArg,
        /// Print the commands that would run, without running any of them.
        #[arg(long)]
        dry_run: bool,
        /// Allow a disk that already holds partitions or a filesystem.
        #[arg(long)]
        force_content: bool,
        /// Skip the "type the exact group name" confirmation prompt (for
        /// scripts and automation).
        #[arg(long)]
        yes: bool,
        // A rebuild started while silent corruption may already be present
        // risks amplifying or mismatching parity as it goes.
        /// Start even if a group has not been fully checked for errors
        /// recently. Not recommended.
        #[arg(long)]
        skip_scrub_check: bool,
    },
    // The loop stays despite the misleading name because
    // `packaging/shr-rs.service` runs it as a long-lived `Restart=always`
    // service.
    /// Reprint every group's status every 10 seconds until interrupted, for
    /// watching progress on a second terminal. This is not a monitoring
    /// daemon -- it detects nothing and acts on nothing. The routine
    /// background work (error checks, rebuild throttling, health checks,
    /// snapshots) is set up by `shr-rs schedule install` instead.
    Daemon {
        /// Path to the configuration file to read.
        #[arg(long, default_value = STATE_PATH)]
        state_path: PathBuf,
    },
    /// Finish the space-expansion step that a previous `expand` had to
    /// postpone until its rebuild finished. Safe to run any time, including
    /// when nothing is pending.
    Reconcile,
    // Without this, a hand-teardown leaves orphaned managed-block entries
    // behind in the configuration file.
    /// Remove a group completely: unmount it, delete its volumes, stop its
    /// RAID arrays and drop it from the configuration. The data is not
    /// recoverable afterwards.
    Destroy {
        /// Group to remove. Required when more than one group exists.
        #[arg(long)]
        name: Option<String>,
        // Leaving this off leaves each member partition's mdadm superblock in
        // place: if these same disks are later reused by `create`, it
        // re-partitions them at the same offsets, so the old superblock lands
        // on the new partition -- `create` neutralizes it itself right before
        // `mdadm --create` runs, so THIS tool copes either way, but an
        // `mdadm --assemble --scan` run by anything else in the meantime (a
        // different tool, a stray cron job, a reboot before the reuse) can
        // still find and reassemble the old array from it.
        /// Also erase the RAID markers left on the disks, leaving them
        /// completely blank.
        #[arg(long)]
        zero_superblocks: bool,
        /// Leave the RAID markers in place, keeping some chance of
        /// recovering the old arrangement by hand. The array is still
        /// recorded so it is never auto-assembled again.
        #[arg(long, conflicts_with = "zero_superblocks")]
        no_zero_superblocks: bool,
        /// Print the teardown commands without running any of them.
        #[arg(long)]
        dry_run: bool,
        /// Skip the typed-name confirmation (for scripts and automation).
        #[arg(long)]
        yes: bool,
    },
    /// List every storage group on this host (read-only).
    Groups,
    /// Filesystem operations: error checks, compression, snapshots, usage.
    Fs {
        #[command(subcommand)]
        command: FsCmd,
    },
    /// Disk operations: inventory, health, replacement.
    Disk {
        #[command(subcommand)]
        command: DiskCmd,
    },
    /// Set up the background schedule for routine error checks and rebuild
    /// throttling. Requires root.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCmd,
    },
    /// Hidden, internal entry points invoked by shr-rs's OWN generated
    /// systemd units -- not meant for interactive use.
    #[command(hide = true)]
    Internal {
        #[command(subcommand)]
        command: InternalCmd,
    },
}

#[derive(Subcommand)]
enum FsCmd {
    /// Check a group for storage errors and repair what it can.
    Scrub {
        #[command(subcommand)]
        action: ScrubCmd,
    },
    // Btrfs only applies a changed `compress=` mount option to newly-written
    // data -- this is how existing data picks up a new setting.
    /// Rewrite every existing file with a new compression setting.
    Recompress {
        /// Which group to rewrite. Optional only while there is exactly one
        /// group.
        #[arg(long)]
        name: Option<String>,
        /// The compression setting to apply.
        #[arg(long, default_value = "zstd:3")]
        compression: String,
        /// Skip the "type the exact group name" confirmation prompt (for
        /// scripts and automation).
        #[arg(long)]
        yes: bool,
    },
    /// Point-in-time snapshots of a group's data.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
    },
    // Btrfs live chunk usage isn't parsed anywhere yet -- every figure that
    // would come from it renders as `?` rather than being fabricated; see
    // `build_fs_df`'s doc comment.
    /// Show how much space each group has and how much is in use.
    Df,
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// Create a read-only snapshot of a group's data at
    /// `@snapshots/<name>`.
    Create {
        /// Snapshot name (must not contain `/`).
        name: String,
        /// Which group to snapshot. Optional only while there is exactly
        /// one group.
        #[arg(long)]
        group: Option<String>,
    },
}

#[derive(Subcommand)]
enum DiskCmd {
    // Read-only inventory: `status`/`groups` cover overall array health,
    // this covers "what disks are there and how do I refer to each one".
    /// List every disk on this host, with its size, model, serial, type and
    /// which group it belongs to.
    List,
    // Nonzero exit is deliberate, so a cron job or systemd timer can act on
    // it without parsing the output.
    /// Report the health of every managed disk. Exits nonzero if any disk
    /// reports a problem.
    Smart,
    /// Swap one disk out for another of the same size or larger.
    Replace {
        /// The disk to remove. Give its `/dev/disk/by-id` name or serial
        /// number so this still works after the disk has failed; a short
        /// name (`sdc`) works too, but only while the disk is still visible.
        #[arg(long)]
        old: String,
        /// The new disk to replace it with.
        #[arg(long)]
        new: String,
        /// Which group `old` belongs to. Optional only while there is
        /// exactly one group.
        #[arg(long)]
        name: Option<String>,
        /// Skip the "type the exact disk name" confirmation prompt (for
        /// scripts and automation).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ScrubCmd {
    /// Start a full error check of one group's disks and files.
    Start {
        /// Which group to check. Optional only while there is exactly one
        /// group.
        #[arg(long)]
        name: Option<String>,
        // Deliberately no default, unlike `expand --priority`: leaving it
        // out has to keep meaning "change no kernel parameter", which is
        // what every `fs scrub start` did before this flag existed and what
        // the scheduled-scrub timer still relies on.
        /// How much of the disks' speed the check may take from everyday
        /// file access. Left out, whatever speed limit the system already
        /// has is used unchanged.
        #[arg(long, value_enum)]
        priority: Option<PriorityArg>,
    },
    /// Show how far the check has got, and record the result once it ends.
    Status {
        /// Which group to report on. Optional only while there is exactly
        /// one group.
        #[arg(long)]
        name: Option<String>,
    },
    /// Stop a running check.
    Cancel {
        /// Which group to stop checking. Optional only while there is
        /// exactly one group.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum ScheduleCmd {
    // Writes each group's scrub timer, the global throttle-tick and
    // health-check timers, and -- only when `policy.toml`'s
    // `[snapshot].enabled` is true -- the snapshot-automation timer, then
    // `systemctl daemon-reload` and `enable --now` for each. Also prunes any
    // shr-rs-owned `shr-rs-scrub-*` unit left behind for a group that no
    // longer exists; an operator's own same-named unit is only warned about,
    // never touched.
    /// Create or refresh the background schedule for every group. Safe to
    /// re-run after adding or removing a group.
    Install {
        // The generated unit file IS the record: `policy.toml` is
        // operator-authored and shr-rs never writes it, so re-running this
        // without the flag goes back to whatever that file says (by default,
        // no `--priority` at all).
        /// How much of the disks' speed a SCHEDULED check may take from
        /// everyday file access. Left out, `policy.toml`'s `[scrub]
        /// priority` decides, and with that unset too the scheduled check
        /// changes no kernel parameter.
        #[arg(long, value_enum)]
        scrub_priority: Option<PriorityArg>,
    },
}

#[derive(Subcommand)]
enum InternalCmd {
    /// Apply one adaptive-throttle tick to every band with an md sync
    /// running, across every group, and hand back the limits of every band
    /// that has finished one. Invoked periodically by the
    /// `shr-rs-throttle-tick.timer` unit `schedule install` creates -- a
    /// brand-new process each time, by design (see
    /// `OrchestrationEngine::tick_active_sync`'s doc comment).
    ReshapeThrottleTick,
    /// Poll every group for notification triggers (scrub errors, degraded,
    /// worsening SMART) and fire enabled channels. Invoked periodically by
    /// the `shr-rs-health-check.timer` unit `schedule install` creates.
    HealthCheckTick,
    /// Create one automated snapshot per group and prune old ones beyond
    /// `policy.toml`'s `[snapshot].keep`. Invoked periodically by the
    /// `shr-rs-snapshot-auto.timer` unit `schedule install` creates ONLY
    /// when `[snapshot].enabled` is `true` -- a no-op if that timer
    /// somehow still fires after being turned back off in policy.toml
    /// (checked live here, not just at install time).
    ///
    /// Named `SnapshotAutoRun`, not `...Tick` (unlike its two siblings
    /// above) -- purely to keep clippy's `enum_variant_names` happy once a
    /// third variant existed to compare against; the CLI surface itself is
    /// unaffected via the explicit rename below.
    #[command(name = "snapshot-auto-tick")]
    SnapshotAutoRun,
}

#[derive(Subcommand)]
enum PlanCmd {
    /// Show what a group built from these disks would look like. Changes
    /// nothing.
    Create {
        /// Redundancy mode.
        #[arg(long, value_enum)]
        mode: ModeArg,
        /// Hypothetical disk sizes to try out, e.g. `--sizes 4TB,4TB,6TB`.
        #[arg(
            long,
            value_delimiter = ',',
            conflicts_with = "disks",
            required_unless_present = "disks"
        )]
        sizes: Vec<String>,
        /// Disks to use. Accepts `sdb`, `/dev/sdb`, a `/dev/disk/by-id` name,
        /// or a serial number.
        #[arg(long, value_delimiter = ',', conflicts_with = "sizes")]
        disks: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Shr,
    Shr2,
}

impl From<ModeArg> for RedundancyMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Shr => RedundancyMode::Shr,
            ModeArg::Shr2 => RedundancyMode::Shr2,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PriorityArg {
    Background,
    Balanced,
    Max,
}

impl From<PriorityArg> for shr_exec::SyncPriority {
    fn from(p: PriorityArg) -> Self {
        match p {
            PriorityArg::Background => shr_exec::SyncPriority::Background,
            PriorityArg::Balanced => shr_exec::SyncPriority::Balanced,
            PriorityArg::Max => shr_exec::SyncPriority::Max,
        }
    }
}

/// Entry point `shr-bin` calls once it has decided (via
/// `shr_command::detect_ui_mode`) that this invocation wants CLI mode, not
/// the TUI. `args` is the raw command line INCLUDING argv[0] (clap uses it
/// only for the program name shown in `--help`/usage text) -- `shr-bin`
/// passes its own `std::env::args()` straight through unfiltered except for
/// `--tui`/`--no-tui`, which `detect_ui_mode` already consumed and which
/// this crate's own `Cli` knows nothing about.
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    let json = cli.json;
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if json {
                let payload = serde_json::json!({ "error": format!("{e:#}") });
                eprintln!("{payload}");
            } else {
                eprintln!("error: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Status {
            detail,
            watch,
            interval_secs,
        } => {
            if watch {
                // No defined JSON streaming contract for a redraw loop
                // -- reject up front, before any real I/O, rather than
                // silently picking one meaning. Cockpit/scripts already poll
                // plain `status --json` on their own interval instead.
                if cli.json {
                    bail!(
                        "`status --watch` cannot be combined with --json -- run \
                         `status --json` (without --watch) on your own interval instead"
                    );
                }
                run_status_watch(Duration::from_secs(interval_secs))?;
            } else {
                // A fresh host (or Cockpit polling before anything has ever
                // been `create`d) has no state.toml at all --
                // `StateStore::load` already returns `Ok(None)` for that case
                // (it's not an error), and `build_status` turns `None` into
                // an empty `groups` list rather than inventing data or
                // failing the whole status read.
                let state = StateStore::new(STATE_PATH).load().with_context(state_load_ctx)?;
                let report = attach_state_path(
                    build_status(&SystemInspector, state.as_ref())
                        .context("inspecting system (needs lsblk / /proc/mdstat / smartctl on Linux)")?,
                );
                if cli.json {
                    // `--detail` carries no separate JSON shape: `StatusReport`
                    // already includes every per-band field
                    // `render_status_detail` shows, regardless of this flag
                    // (see `GroupBandStatus`'s doc comment) -- so
                    // `--detail --json` and plain `--json` are byte-identical,
                    // deliberately.
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    if detail {
                        print!("{}", render::render_status_detail(&report));
                    } else {
                        print!("{}", render::render_status(&report));
                    }
                    if let Some(warning) = stale_speed_limit_warning(state.as_ref(), &report) {
                        println!("{warning}");
                    }
                }
            }
        }
        Command::Plan {
            plan: PlanCmd::Create { mode, sizes, disks },
        } => {
            let disk_list = resolve_disks(&sizes, &disks)?;
            let report = build_plan_report(mode.into(), disk_list)?;
            emit(cli.json, &report, render::render_plan)?;
        }
        Command::Preflight { disks, force_content } => {
            let names = resolve_kernel_names(&disks)?;
            let report =
                preflight_create(&SystemInspector, &names, force_content).context("running safety checks")?;
            // `--json` is the machine contract Cockpit's group-creation
            // wizard (and any other automated caller) consumes: it must
            // always exit 0 once preflight itself ran successfully, blockers
            // included, so the report on stdout is reliably parseable and
            // `report.ok`/`report.blockers` is how the caller learns
            // "blocked" -- not a nonzero exit code. Exiting nonzero here (as
            // the non-JSON branch still does, for interactive/scripted use)
            // would route through `main()`'s error path, which prints a
            // second, generic `{"error": ...}` payload to STDERR while
            // leaving the real, detailed blocker list stranded on stdout
            // behind a failed process -- exactly the kind of two-JSONs-that-
            // can-disagree shape this project's UI layers must never have to
            // reconcile.
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_preflight(&report));
                if !report.ok {
                    bail!("safety checks failed: these disks are not safe to use");
                }
            }
        }
        Command::Create {
            mode,
            disks,
            name,
            mount,
            vg_name,
            lv_name,
            compression,
            dry_run,
            force_content,
            yes,
        } => {
            let resolved = resolve_real_disks(&disks)?;
            let kernel_names: Vec<String> = resolved.iter().map(|d| d.kernel_name.clone()).collect();
            let preflight = preflight_create(&SystemInspector, &kernel_names, force_content)?;
            if !preflight.ok && !dry_run {
                bail!("safety checks failed: these disks are not safe to use");
            }

            // Independent of the request's target set: this is the host's
            // full protected-disk list, not just aliases of whatever was
            // requested (see shr_command::system_disk_aliases doc comment).
            let system_disks = system_disk_aliases(&SystemInspector)?;

            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let preview_req = CreateRequest {
                name: name.clone(),
                mode: mode.into(),
                disks: resolved.clone(),
                vg_name: vg_name.clone(),
                lv_name: lv_name.clone(),
                mount_point: mount.clone(),
                compression: compression.clone(),
                system_disks: system_disks.clone(),
            };
            // Preview always runs first (D13's own dry-run glue) -- both to
            // serve `--dry-run` itself and, for a real run, to learn the
            // resolved group name/planned commands the interactive gate
            // shows BEFORE any real disk is touched.
            let (preview_state, commands) =
                shr_orchestrate::preview_create(state_store.clone(), preview_req)?;

            if dry_run {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&merge_planned_commands(&preview_state, &commands)?)?
                    );
                } else {
                    println!(
                        "Dry run -- array would be created. Name: {}, Mode: {}, Version: {}",
                        preview_state.name, preview_state.mode, preview_state.layout_version
                    );
                    print!("{}", render_planned_commands_text(&commands));
                    print!("{}", dry_run_layout_diagram_text(&resolved, mode.into())?);
                }
                return Ok(());
            }

            // A human at a TTY (stdout AND stdin, any size) without
            // `--yes` must type the exact group name before anything
            // destructive runs -- same gate as Cockpit/TUI (constraint 5,
            // `shr-tui`'s `wizard::can_execute`). A non-interactive caller (a
            // script, `ssh nas 'shr-rs ...'`, Cockpit's own spawned process)
            // keeps today's auto-approve behavior untouched. Deliberately
            // `can_prompt_operator`, NOT `is_interactive_terminal`: the
            // latter is the design's TUI-render check (size, TERM=dumb) and a
            // small or `TERM=dumb` terminal is still a human who must
            // confirm -- see `shr_command::can_prompt_operator`.
            require_typed_confirmation(
                &preview_state.name,
                &commands,
                yes,
                "create",
                shr_command::can_prompt_operator(),
                &mut std::io::stdin().lock(),
            )?;

            let mut lock = acquire_state_lock()?;
            let _guard = lock.try_write().map_err(|_| {
                anyhow!("another shr-rs create/expand/reconcile is already running (lock: {STATE_LOCK_PATH})")
            })?;

            let req = CreateRequest {
                name,
                mode: mode.into(),
                disks: resolved,
                vg_name,
                lv_name,
                mount_point: mount,
                compression,
                system_disks,
            };
            let sys_runner = SystemRunner::new();
            // `create` runs the most destructive command sequence of
            // any operation here (partition every disk, then mdadm, then
            // LVM, then mkfs) and gave a CLI/SSH operator no feedback at
            // all between invocation and completion.
            //
            // `TextProgressSink`, NOT the `RecordingProgressSink` that
            // `reconcile`/`disk replace` use: a recording sink buffers and
            // is drained after the call returns, which reports what
            // happened but still leaves the operator watching a dead
            // terminal for the whole run. Those two handlers can afford
            // that -- their updates are end-of-run summaries. This one
            // can't. Writes to stderr as each update arrives, so the
            // `--json` stdout contract is untouched.
            let progress = TextProgressSink::new(std::io::stderr());
            // The engine's default confirm sink is fail-closed
            // (`AlwaysRejectConfirmSink`) precisely so a call site can't
            // silently skip confirmation just by forgetting to wire one.
            // shr-cli explicitly opts back into auto-approve here: the
            // gate just above (or the caller's own `--yes`/non-interactive
            // status) IS the approval -- this line is that opt-in, not a
            // reintroduction of the old fail-open default.
            let engine = production_engine(&sys_runner, state_store)
                .with_confirm_sink(&AlwaysConfirmSink)
                .with_progress_sink(&progress);
            let state = engine.create(req)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                // No `layout_version`: it is an internal on-disk revision
                // number, meaningless to whoever just created a group.
                println!(
                    "Storage group `{}` created ({}), mounted at {}.",
                    state.name, state.mode, state.filesystem.mount_point
                );
            }
        }
        Command::Expand {
            disks,
            name,
            priority,
            dry_run,
            force_content,
            yes,
            skip_scrub_check,
        } => {
            let resolved = resolve_real_disks(&disks)?;
            let kernel_names: Vec<String> = resolved.iter().map(|d| d.kernel_name.clone()).collect();
            let preflight = preflight_create(&SystemInspector, &kernel_names, force_content)?;
            if !preflight.ok && !dry_run {
                bail!("safety checks failed: these disks are not safe to use");
            }
            let system_disks = system_disk_aliases(&SystemInspector)?;

            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            let preview_req = ExpandRequest {
                name: name.clone(),
                new_disks: resolved.clone(),
                system_disks: system_disks.clone(),
                skip_scrub_check,
            };
            // The preview must answer `expand()`'s
            // live-status validation checks (degraded/background-activity/
            // scrub-running) from the REAL system, not the preview's own
            // internal `DryRunRunner` -- otherwise a scrub genuinely
            // running right now is invisible to this call (`sync_action`
            // always fabricated as `"idle"`), and the resulting stale,
            // misleading error kills the command before the real
            // `engine.expand(req)` call below -- which DOES see the real
            // system -- ever runs. See `preview_expand_against`'s doc
            // comment.
            let (preview_state, commands) =
                shr_orchestrate::preview_expand_against(state_store.clone(), preview_req, Some(&sys_runner))?;

            if dry_run {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&merge_planned_commands(&preview_state, &commands)?)?
                    );
                } else {
                    println!(
                        "Dry run -- array would be expanded. Name: {}, Layout Version: {}",
                        preview_state.name, preview_state.layout_version
                    );
                    print!("{}", render_planned_commands_text(&commands));
                    print!("{}", expand_dry_run_layout_diagram_text(&preview_state)?);
                }
                return Ok(());
            }

            // See the identical `create` gate above: confirmation
            // uses `can_prompt_operator`, not `is_interactive_terminal`.
            require_typed_confirmation(
                &preview_state.name,
                &commands,
                yes,
                "expand",
                shr_command::can_prompt_operator(),
                &mut std::io::stdin().lock(),
            )?;

            let mut lock = acquire_state_lock()?;
            let _guard = lock.try_write().map_err(|_| {
                anyhow!("another shr-rs create/expand/reconcile is already running (lock: {STATE_LOCK_PATH})")
            })?;

            let req = ExpandRequest {
                name,
                new_disks: resolved,
                system_disks,
                skip_scrub_check,
            };
            // `expand` can run for hours (a live reshape) and gave a
            // CLI/SSH operator nothing to watch. Streaming (not buffered)
            // for the reason spelled out on the `Create` handler above --
            // buffering updates until a multi-hour reshape returns would
            // print them all at the one moment the operator no longer
            // needs them.
            let progress = TextProgressSink::new(std::io::stderr());
            // Reuses the SAME `sys_runner` the preview above used for its
            // `status_runner` -- explicit auto-approve opt-in, see the
            // identical comment on the `Create` handler's engine
            // construction above.
            let engine = production_engine(&sys_runner, state_store)
                .with_confirm_sink(&AlwaysConfirmSink)
                .with_progress_sink(&progress)
                .with_priority(priority.into());
            let state = engine.expand(req)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                println!(
                    "Array expanded successfully! Name: {}, Layout Version: {}",
                    state.name, state.layout_version
                );
            }
        }
        Command::Daemon { state_path } => {
            // This loop is NOT a monitoring daemon -- it detects
            // nothing and reacts to nothing. It only re-reads state.toml
            // and prints it, which is honestly just a state tailer. Real
            // periodic work is installed separately; see the doc comment on
            // `Command::Daemon` above for why the loop itself still exists.
            //
            // The startup banner is prose, not data, and must never
            // reach stdout under `--json` -- it would be the first line of
            // what is otherwise an NDJSON stream (see `daemon_tick_report_
            // json`'s doc comment below) and would break every consumer
            // that parses each line as JSON. Human mode keeps this exactly
            // as before.
            if !cli.json {
                println!(
                    "shr-rs daemon: this is NOT a monitoring daemon. It only reprints each \
                     group's status from {} every 10 seconds until you stop it (Ctrl-C) -- it \
                     detects nothing and acts on nothing. To set up the routine background work \
                     that keeps a group healthy (error checks, rebuild throttling, health \
                     checks, snapshots), run `shr-rs schedule install` instead.",
                    state_path.display()
                );
            }
            let store = StateStore::new(state_path);
            loop {
                match store.load() {
                    Ok(Some(state)) if !state.groups.is_empty() => {
                        if cli.json {
                            println!("{}", daemon_tick_report_json(&state.groups));
                        } else {
                            for g in &state.groups {
                                println!(
                                    "[status] group {}: mode: {}, layout {}, disks: {}",
                                    g.name,
                                    g.mode,
                                    g.layout_version,
                                    g.disks.len()
                                );
                            }
                        }
                    }
                    _ => {
                        if cli.json {
                            println!("{}", daemon_tick_report_json(&[]));
                        } else {
                            println!("[status] No storage group configured yet.");
                        }
                    }
                }
                thread::sleep(Duration::from_secs(10));
            }
        }
        Command::Reconcile => {
            let mut lock = acquire_state_lock()?;
            let _guard = lock.try_write().map_err(|_| {
                anyhow!("another shr-rs create/expand/reconcile is already running (lock: {STATE_LOCK_PATH})")
            })?;

            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            // Real-guest repro: `reconcile()` reports its member-
            // removal/resize-completion steps through `ProgressSink` as it
            // performs them (same as every other long-running operation),
            // but this handler never wired one up, so those updates went
            // nowhere -- the exact `disk replace` gap an earlier fix already addressed
            // (see that handler's comment), left open here.
            let progress = RecordingProgressSink::new();
            let engine = production_engine(&sys_runner, state_store).with_progress_sink(&progress);
            match engine.reconcile()? {
                Some(outcome) if cli.json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&reconcile_report_json(&outcome))?
                    )
                }
                Some(outcome) => {
                    for update in progress.updates() {
                        eprintln!("[{}] {}: {}", update.operation, update.stage, update.message);
                    }
                    // Report what this call actually DID first --
                    // a member it removed, a deferred resize it completed,
                    // a stale flag it self-healed -- before saying anything
                    // about what remains. The previous report was built
                    // purely from the returned state's `resize_pending`
                    // flags: a member removal never showed up there at
                    // all, and a resize this very call just finished read
                    // identically to one that was never pending in the
                    // first place. Real-guest repro: `shr-rs reconcile`
                    // removed a faulty old member (`loop12p1`) from a live
                    // array and rewrote `state.toml`, then printed only
                    // `Reconcile: nothing pending.`
                    for action in &outcome.performed {
                        println!("{}", describe_reconcile_action(action));
                    }

                    // Flattened across every group -- a plain `shr-rs
                    // reconcile` with no `--name` of its own must still
                    // report a deferred resize STILL pending on ANY group,
                    // not just whichever one happens to be first.
                    let pending: Vec<String> = outcome
                        .state
                        .groups
                        .iter()
                        .flat_map(|g| {
                            g.bands
                                .iter()
                                .filter(|b| b.resize_pending)
                                .map(move |b| format!("`{}` band {}", g.name, b.index))
                        })
                        .collect();
                    if !pending.is_empty() {
                        println!(
                            "Still rebuilding, so the expansion stays unfinished for now: {}.",
                            pending.join(", ")
                        );
                    } else if outcome.performed.is_empty() {
                        // The ONLY case this line may print: nothing was
                        // pending AND nothing remains pending now.
                        println!("Nothing left to finish.");
                    }
                }
                None if cli.json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({"error": "no active array"}))?
                    )
                }
                None => println!("No storage group found."),
            }
        }
        Command::Destroy {
            name,
            zero_superblocks,
            no_zero_superblocks,
            dry_run,
            yes,
        } => {
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            // Resolved BEFORE the preview, deliberately: the planned command
            // list differs by this choice (it is what puts the `mdadm
            // --zero-superblock` calls in or leaves them out), so previewing
            // first would show the operator a plan that is not the one they
            // then type a group name to confirm. Applies to `--dry-run` too
            // -- "what would happen" has no single answer until this is
            // settled.
            let zero_superblocks = resolve_zero_superblocks(
                zero_superblocks,
                no_zero_superblocks,
                yes,
                shr_command::can_prompt_operator(),
                &mut std::io::stdin().lock(),
            )?;
            // Same reason as `Expand`: the preview replays
            // `destroy()` under a DryRunRunner, so its live-status
            // validation (expansion in progress, etc.) would otherwise read
            // fabricated values and either pass or fail for the wrong
            // reason before the real call ever runs.
            let commands = shr_orchestrate::preview_destroy_against(
                state_store.clone(),
                name.clone(),
                zero_superblocks,
                Some(&sys_runner),
            )?;

            if dry_run {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "planned_commands": commands,
                            "zero_superblocks": zero_superblocks,
                        }))?
                    );
                } else {
                    print!("{}", render_planned_commands_text(&commands));
                }
                return Ok(());
            }

            // The most destructive command in the tool: everything on these
            // disks goes away. Same gate as `create`/`expand`, and
            // deliberately `can_prompt_operator` rather than
            // `is_interactive_terminal` -- a 79x23 or `TERM=dumb` terminal
            // is still a human who must type the group name.
            let confirm_name = name.clone().unwrap_or_else(|| {
                state_store
                    .load()
                    .ok()
                    .flatten()
                    .and_then(|s| s.groups.first().map(|g| g.name.clone()))
                    .unwrap_or_default()
            });
            require_typed_confirmation(
                &confirm_name,
                &commands,
                yes,
                "destroy",
                shr_command::can_prompt_operator(),
                &mut std::io::stdin().lock(),
            )?;

            let mut lock = acquire_state_lock()?;
            let _guard = lock.try_write().map_err(|_| {
                anyhow!("another shr-rs create/expand/reconcile is already running (lock: {STATE_LOCK_PATH})")
            })?;
            let engine = production_engine(&sys_runner, state_store).with_confirm_sink(&AlwaysConfirmSink);
            engine.destroy(name.as_deref(), zero_superblocks)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"destroyed": confirm_name}))?
                );
            } else {
                println!("group `{confirm_name}` destroyed");
            }
        }
        Command::Groups => {
            // Read-only: no lock needed (mirrors `Status`/`Preflight`, which
            // also never take `acquire_state_lock`). `--json` emits the
            // WHOLE `StateFile` verbatim (not a hand-picked summary) --
            // no bespoke "groups list" schema to keep in sync with
            // `state.toml`. No production caller parses this today;
            // Cockpit reads `status --json` instead.
            let state_store = StateStore::new(STATE_PATH);
            let state = state_store.load().with_context(state_load_ctx)?;
            if cli.json {
                let payload = state.unwrap_or_else(|| shr_state::StateFile::new(Vec::new()));
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                match state {
                    Some(state) if !state.groups.is_empty() => {
                        for g in &state.groups {
                            // No `layout_version`: it is an internal on-disk
                            // revision number. `--json` still carries it for
                            // callers that track the on-disk format.
                            println!(
                                "{}  mode={}  disks={}  bands={}  mount={}{}",
                                g.name,
                                g.mode,
                                g.disks.len(),
                                g.bands.len(),
                                g.filesystem.mount_point,
                                if g.expansion.in_progress {
                                    "  [expansion in progress]"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                    _ => println!("No groups."),
                }
            }
        }
        Command::Fs {
            command: FsCmd::Scrub { action },
        } => {
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            // Same explicit auto-approve rationale as Create/Expand's engine
            // construction: scrub start/cancel are real, root-only, already
            // gated by this process even being invoked (no separate
            // typed-confirmation gate -- unlike create/expand, a scrub
            // doesn't destroy anything; it's the intended safety NET).
            // `ScrubCmd::Status` (below) is where a finished scrub's
            // error count is actually OBSERVED and, via
            // `reconcile_group_scrub`, where the "scrub found errors"
            // notification fires -- must be wired here for real delivery.
            // `.clone()`'d rather than moved: the `--json` payload for
            // Start/Cancel needs the resolved group name (see
            // `resolve_group_name_for_report`'s doc comment), read in each
            // arm below via this same `state_store` -- which would
            // otherwise already be consumed by `OrchestrationEngine::new`.
            let engine = OrchestrationEngine::new(&sys_runner, state_store.clone())
                .with_confirm_sink(&AlwaysConfirmSink)
                .with_notify_policy(load_notify_policy());
            match action {
                ScrubCmd::Start { name, priority } => {
                    let mut lock = acquire_state_lock()?;
                    let _guard = lock.try_write().map_err(|_| {
                        anyhow!("another shr-rs create/expand/reconcile is already running (lock: {STATE_LOCK_PATH})")
                    })?;
                    let group = resolve_group_name_for_report(&state_store, name.as_deref());
                    let speed = priority.map(shr_exec::SyncPriority::from);
                    engine.scrub_start(name.as_deref(), speed)?;
                    // Reported because it is a real kernel change this
                    // command made -- an operator who passed `--priority`
                    // and saw only "scrub started" would have no way to tell
                    // it took effect. The profile rather than a KB/s number:
                    // the limits are a fraction of each band's own measured
                    // capability, so there is no single number to name here.
                    // Absent from the JSON (rather than null) when no
                    // profile was given, matching "no parameter was
                    // touched".
                    let priority_name = speed.map(|p| p.as_str());
                    if cli.json {
                        println!("{}", scrub_start_report_json(&group, priority_name));
                    } else {
                        match priority_name {
                            Some(p) => println!("scrub started (priority {p})"),
                            None => println!("scrub started"),
                        }
                    }
                }
                ScrubCmd::Status { name } => {
                    let report = engine.scrub_status(name.as_deref())?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "group": report.group_name,
                                "running": report.running,
                                "error_count": report.error_count,
                            })
                        );
                    } else {
                        println!(
                            "group `{}`: {} -- {} error(s) found",
                            report.group_name,
                            if report.running { "running" } else { "finished" },
                            report.error_count
                        );
                        if report.error_count > 0 {
                            bail!(
                                "scrub found {} error(s) in group `{}`",
                                report.error_count,
                                report.group_name
                            );
                        }
                    }
                }
                ScrubCmd::Cancel { name } => {
                    let group = resolve_group_name_for_report(&state_store, name.as_deref());
                    engine.scrub_cancel(name.as_deref())?;
                    if cli.json {
                        println!("{}", scrub_action_report_json(&group, "cancelled"));
                    } else {
                        println!("scrub cancelled");
                    }
                }
            }
        }
        Command::Fs {
            command:
                FsCmd::Recompress {
                    name,
                    compression,
                    yes,
                },
        } => {
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();

            // `recompress` used to run immediately,
            // unlike create/expand/disk-replace/destroy -- despite
            // rewriting EVERY file's data extents (hours of IO on a real
            // array) via `btrfs filesystem defragment -r`, which also
            // breaks extent sharing with any `@snapshots` and can
            // sharply increase used space. No preview exists for this
            // command (out of scope here), so `planned_commands` is a
            // short, hand-written summary of what changes and why it's
            // irreversible -- not a byte-for-byte replay of the real
            // commands the way create/expand/destroy's previews are.
            // `can_prompt_operator`, NOT `is_interactive_terminal`:
            // the latter is the design's TUI-render check (80x24, TERM!=dumb),
            // not "is a human here to confirm". Non-interactive callers
            // (Cockpit spawns this without `--yes`) keep auto-approving,
            // same as every other gate here.
            let loaded = state_store.load().ok().flatten();
            let confirm_name = name.clone().unwrap_or_else(|| {
                loaded
                    .as_ref()
                    .and_then(|s| s.groups.first().map(|g| g.name.clone()))
                    .unwrap_or_default()
            });
            let mount_point = loaded.as_ref().and_then(|s| match &name {
                Some(n) => s
                    .groups
                    .iter()
                    .find(|g| &g.name == n)
                    .map(|g| g.filesystem.mount_point.clone()),
                None => s.groups.first().map(|g| g.filesystem.mount_point.clone()),
            });
            let planned_commands = mount_point
                .map(|mp| {
                    vec![
                        format!("mount -o remount,compress={compression} {mp}"),
                        format!(
                            "btrfs filesystem defragment -r {mp}  (rewrites EVERY file's data \
                             extents at the new compression level; if `@snapshots` exist, this \
                             breaks their extent sharing with `@` and can sharply increase used \
                             space)"
                        ),
                        format!("update `compress=` in /etc/fstab for group `{confirm_name}`"),
                    ]
                })
                .unwrap_or_default();
            require_typed_confirmation(
                &confirm_name,
                &planned_commands,
                yes,
                "recompress",
                shr_command::can_prompt_operator(),
                &mut std::io::stdin().lock(),
            )?;

            // This call site used to build its own
            // `OrchestrationEngine` inline WITHOUT `.with_conf_paths`, so
            // `recompress()`'s `write_fstab` wrote to the engine's
            // tempdir-adjacent default (next to state.toml) instead of the
            // real `/etc/fstab` -- state.toml and the live mount both
            // updated correctly (neither depends on conf paths), but
            // `/etc/fstab` silently kept the OLD compression, so a reboot
            // reverted the filesystem while state.toml kept claiming the
            // new value. Now routed through the same `production_engine`
            // helper every other real-config-writing command uses, so this
            // class of "forgot to wire conf paths" mistake can't recur here.
            let engine = production_engine(&sys_runner, state_store).with_confirm_sink(&AlwaysConfirmSink);
            engine.recompress(name.as_deref(), &compression)?;
            if cli.json {
                println!("{}", recompress_report_json(&confirm_name, &compression));
            } else {
                println!("recompress started at {compression}");
            }
        }
        Command::Fs {
            command:
                FsCmd::Snapshot {
                    action: SnapshotCmd::Create { name, group },
                },
        } => {
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            // Resolved BEFORE `state_store` is moved into the engine
            // below -- `snapshot_create` itself returns `Result<(), ..>`, no
            // group name, so this is the only place left to learn it for
            // the `--json` report (see `resolve_group_name_for_report`).
            let resolved_group = resolve_group_name_for_report(&state_store, group.as_deref());
            let engine =
                OrchestrationEngine::new(&sys_runner, state_store).with_confirm_sink(&AlwaysConfirmSink);
            engine.snapshot_create(group.as_deref(), &name)?;
            if cli.json {
                println!("{}", snapshot_create_report_json(&resolved_group, &name));
            } else {
                println!("snapshot `{name}` created");
            }
        }
        Command::Fs { command: FsCmd::Df } => {
            let state = StateStore::new(STATE_PATH).load().with_context(state_load_ctx)?;
            let report = build_status(&SystemInspector, state.as_ref())
                .context("inspecting system (needs lsblk / /proc/mdstat / smartctl on Linux)")?;
            let sys_runner = SystemRunner::new();
            let usage = fs_usage_map(&sys_runner, &report.groups);
            let df = build_fs_df(&report.groups, &usage);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&df)?);
            } else {
                print!("{}", render::render_fs_df(&df));
            }
        }
        Command::Disk {
            command: DiskCmd::List,
        } => {
            // Read-only, same fixture `status`/`disk smart` already read --
            // no new inspector logic (its own doc comment on `DiskCmd::
            // List`).
            let state = StateStore::new(STATE_PATH).load().with_context(state_load_ctx)?;
            let report = build_status(&SystemInspector, state.as_ref())
                .context("inspecting system (needs lsblk / /proc/mdstat / smartctl on Linux)")?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report.disks)?);
            } else {
                print!("{}", render::render_disk_list(&report));
            }
        }
        Command::Disk {
            command: DiskCmd::Smart,
        } => {
            let state = StateStore::new(STATE_PATH).load().with_context(state_load_ctx)?;
            let report = build_status(&SystemInspector, state.as_ref())
                .context("inspecting system (needs lsblk / /proc/mdstat / smartctl on Linux)")?;
            let mut any_warning = false;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report.disks)?);
            } else {
                // Plain words, not Rust `Debug`: `Some(38)`/`None` are how
                // this code is written, not how a disk report reads.
                let health = |s: &shr_command::SmartState| match s {
                    shr_command::SmartState::Ok => "ok",
                    shr_command::SmartState::Warning => "warning",
                    shr_command::SmartState::Unknown => "unknown",
                };
                let count = |v: Option<u64>| v.map_or_else(|| "unknown".to_string(), |n| n.to_string());
                for d in &report.disks {
                    println!(
                        "{}  health={}  temp={}  reallocated-sectors={}  pending-sectors={}",
                        d.name,
                        health(&d.smart.state),
                        d.smart
                            .temperature_c
                            .map_or_else(|| "unknown".to_string(), |c| format!("{c}C")),
                        count(d.smart.reallocated_sectors),
                        count(d.smart.pending_sectors)
                    );
                }
            }
            any_warning |= report
                .disks
                .iter()
                .any(|d| matches!(d.smart.state, shr_command::SmartState::Warning));
            if any_warning {
                bail!("one or more disks report a SMART warning");
            }
        }
        Command::Disk {
            command: DiskCmd::Replace { old, new, name, yes },
        } => {
            // The gate, simplified: `replace_disk` has no `--dry-run`
            // preview yet (documented Stage C gap -- see the report), so
            // this can't show the exact planned commands the way
            // `create`/`expand`'s typed-name confirmation does. An
            // interactive TTY without `--yes` is refused outright rather
            // than silently auto-approving a destructive, irreversible
            // action -- never a "type the name" gate for now.
            if !yes && shr_command::can_prompt_operator() {
                bail!("refusing to replace a disk without --yes: re-run with --yes to confirm");
            }
            let new_resolved = resolve_real_disks(std::slice::from_ref(&new))?;
            let new_disk = new_resolved
                .into_iter()
                .next()
                .context("resolving replacement disk")?;
            let system_disks = system_disk_aliases(&SystemInspector)?;

            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let mut lock = acquire_state_lock()?;
            let _guard = lock.try_write().map_err(|_| {
                anyhow!("another shr-rs create/expand/reconcile is already running (lock: {STATE_LOCK_PATH})")
            })?;
            let sys_runner = SystemRunner::new();
            // `replace_disk` calls `write_managed_configs` (engine.rs) just
            // like create/expand/reconcile/recompress do, so it MUST go
            // through `production_engine` -- building the engine inline here
            // is exactly the earlier defect, and it stayed hidden because that
            // default path is writable and the write silently succeeds.
            //
            // `replace_disk` can only report a deferred old-member
            // removal (a real disk's copy still running when this command
            // returns) through `ProgressSink` -- no prior CLI command ever
            // wired one up, so that report went nowhere and the operator saw
            // only a plain success line. Wired here so it's actually shown,
            // regardless of `--json` (printed to stderr, so it never taints
            // the `--json` stdout contract).
            let progress = RecordingProgressSink::new();
            let engine = production_engine(&sys_runner, state_store)
                .with_confirm_sink(&AlwaysConfirmSink)
                .with_progress_sink(&progress);
            let state = engine.replace_disk(name.as_deref(), &old, &new_disk, &system_disks)?;
            for update in progress.updates() {
                eprintln!("[{}] {}: {}", update.operation, update.stage, update.message);
            }
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                println!(
                    "disk `{old}` replaced with `{}` in group `{}`",
                    new_disk.id.as_str(),
                    state.name
                );
            }
        }
        Command::Schedule {
            command: ScheduleCmd::Install { scrub_priority },
        } => {
            let state = StateStore::new(STATE_PATH)
                .load()
                .with_context(state_load_ctx)?
                .unwrap_or_else(|| shr_state::StateFile::new(Vec::new()));
            let unit_dir = PathBuf::from(SYSTEMD_UNIT_DIR);
            let sys_runner = SystemRunner::new();
            // Embed the path of the binary actually running THIS
            // command, never a hardcoded `/usr/bin/shr-rs` -- this project
            // has been burned by both a `/usr/bin`-only unit pointing at a
            // nonexistent binary on a `/usr/local/bin`-only install, and by
            // sudo's `secure_path` excluding `/usr/local/bin` so Cockpit ran
            // a stale `/usr/bin/shr-rs`. Fail closed rather than falling
            // back to a guessed path: a generated unit with the wrong path
            // fails silently at the NEXT scheduled fire, long after this
            // command reported success, which is worse than refusing now.
            let exe_path = std::env::current_exe().context(
                "resolving the running shr-rs binary's own path via current_exe() \
                          to embed in the generated systemd units",
            )?;
            let mut policy = load_policy_file();
            // The flag wins over the file for this install, and the
            // generated unit is what carries it afterward.
            if let Some(p) = scrub_priority {
                policy.scrub.priority = Some(shr_exec::SyncPriority::from(p).as_str().to_string());
            }
            let (installed_timers, pruned, warnings) =
                install_schedule_units(&unit_dir, &state, &exe_path, &policy, &sys_runner)?;
            // Mutation-testing follow-up: warnings are
            // collected, not printed, inside `install_schedule_units` --
            // this is the ONLY place they actually reach the operator.
            // stderr only, so `--json`'s stdout stays a clean contract.
            for warning in &warnings {
                eprintln!("{warning}");
            }
            if cli.json {
                println!("{}", schedule_install_report_json(installed_timers, pruned));
            } else {
                println!(
                    "installed and enabled {installed_timers} timer unit(s); pruned {pruned} orphaned unit file(s)"
                );
            }
        }
        Command::Internal {
            command: InternalCmd::ReshapeThrottleTick,
        } => {
            // Held for the same reason `create`/`expand`/`scrub start` hold
            // it: this tick is a read-modify-write of `state.toml` (the
            // per-band SMART baseline, and the saved host-wide speed limit),
            // so without the lock a command running concurrently can load an
            // older copy and write it back over this one -- silently losing
            // the saved speed limit, which nothing would then ever restore.
            let mut lock = acquire_state_lock()?;
            let Ok(_guard) = lock.try_write() else {
                report_tick_skipped(cli.json, serde_json::json!({ "bands_ticked": 0 }));
                return Ok(());
            };
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            let engine =
                OrchestrationEngine::new(&sys_runner, state_store).with_confirm_sink(&AlwaysConfirmSink);
            let ticked = engine.tick_active_sync()?;
            if cli.json {
                println!("{}", serde_json::json!({ "bands_ticked": ticked }));
            } else {
                println!("ticked {ticked} syncing band(s)");
            }
        }
        Command::Internal {
            command: InternalCmd::HealthCheckTick,
        } => {
            // Production wiring: this is the periodic entrypoint
            // `shr-rs-health-check.timer` (installed by `schedule install`,
            // every 15 minutes) actually invokes.
            //
            // Locked for the same reason as the throttle tick above:
            // `check_health` self-heals `scrub_in_progress` and records SMART
            // baselines, both of which write `state.toml`.
            let mut lock = acquire_state_lock()?;
            let Ok(_guard) = lock.try_write() else {
                report_tick_skipped(cli.json, serde_json::json!({ "ok": true }));
                return Ok(());
            };
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            let engine = OrchestrationEngine::new(&sys_runner, state_store)
                .with_confirm_sink(&AlwaysConfirmSink)
                .with_notify_policy(load_notify_policy());
            engine.check_health()?;
            if cli.json {
                println!("{}", serde_json::json!({ "ok": true }));
            } else {
                println!("health check complete");
            }
        }
        Command::Internal {
            command: InternalCmd::SnapshotAutoRun,
        } => {
            // Production wiring: this is the periodic entrypoint
            // `shr-rs-snapshot-auto.timer` (installed by `schedule install`
            // only when `[snapshot].enabled` is `true`) actually invokes.
            // Policy is re-checked LIVE here, not just at install time: an
            // operator who flips `enabled` back to `false` without
            // immediately re-running `schedule install` must not still get
            // silently-run automation on the next tick.
            let policy = load_policy_file();
            if !policy.snapshot.enabled {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({ "ok": true, "enabled": false, "snapshots": [] })
                    );
                } else {
                    println!("snapshot automation is disabled in policy.toml; nothing to do");
                }
                return Ok(());
            }
            let state_store = Arc::new(StateStore::new(STATE_PATH));
            let sys_runner = SystemRunner::new();
            let engine =
                OrchestrationEngine::new(&sys_runner, state_store).with_confirm_sink(&AlwaysConfirmSink);
            let summary = engine.snapshot_auto_run(policy.snapshot.keep)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "enabled": true, "snapshots": summary })
                );
            } else if summary.is_empty() {
                println!("no storage group found; nothing to snapshot yet");
            } else {
                for line in &summary {
                    println!("{line}");
                }
            }
        }
    }
    Ok(())
}

/// Best-effort `systemctl disable --now` before deleting one shr-rs-
/// owned unit file -- a no-op for `.service` paths (a `.service` is never
/// itself `systemctl enable`d, only invoked BY its `.timer`, same
/// distinction `OrchestrationEngine::remove_group_scrub_unit` already
/// draws). A failed disable does NOT stop the deletion or propagate an
/// error out of this function -- see `install_schedule_units`'s doc comment
/// for why. Only a genuine file-removal failure (`remove_owned_unit_file`'s
/// own `?`) stays fatal.
///
/// Mutation-testing follow-up: the failure used to go
/// straight to `eprintln!` from here, which no unit test can observe --
/// mutating it to a silent `let _ = ...` left the whole suite green. Pushed
/// onto `warnings` instead, following `OrchestrationEngine::
/// remove_group_scrub_unit`'s `failures: Vec<String>` precedent, so the
/// caller (ultimately the `Schedule Install` handler, the only thing that
/// actually owns a stderr) is what prints it, and a test can assert on the
/// `Vec` directly.
fn disable_and_prune_unit(
    runner: &dyn CommandRunner,
    path: &std::path::Path,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    if path.extension().and_then(|e| e.to_str()) == Some("timer") {
        if let Some(unit_name) = path.file_name().and_then(|n| n.to_str()) {
            if let Err(e) = runner.run("systemctl", &["disable", "--now", unit_name]) {
                warnings.push(format!(
                    "warning: `systemctl disable --now {unit_name}` failed ({e}) -- removing the \
                     unit file anyway so it stops being reported as orphaned on the next run"
                ));
            }
        }
    }
    shr_state::conf::remove_owned_unit_file(path).with_context(|| format!("removing {}", path.display()))
}

/// `ScheduleCmd::Install`'s prune-then-install body, pulled out of the
/// `dispatch` handler so it's reachable from a unit test against a tempdir
/// and a fake `CommandRunner` -- the handler itself hardcodes `/etc/systemd/
/// system`, `SystemRunner`, and `current_exe()`, none of which this
/// Windows dev host's `cargo test` can exercise for real (see this crate's
/// other source-scan tests for the established fallback where extraction
/// isn't possible; here it is). Every value the handler used to reach for
/// directly is now a parameter instead; the sequence of steps is otherwise
/// unchanged. Returns `(installed_timers, pruned, warnings)`: the same two
/// counts the handler already prints/emits as `--json`, plus every
/// non-fatal warning collected along the way (see `disable_and_prune_unit`'s
/// doc comment above) for the handler to `eprintln!` -- collecting rather
/// than printing here is what lets a unit test observe them at all; see the
/// `*_yields_a_warning*`/`*_yields_no_warnings` tests below.
///
/// `systemctl disable --now` failing for an
/// orphaned unit -- e.g. a crash/power-loss-truncated unit file that still
/// starts with the ownership marker (written first) but has no `[Install]`
/// section, so `disable` exits 5 ("no installation config") -- used to `?`-
/// propagate straight out of this whole function, aborting BEFORE this
/// run's own timers were ever written/enabled. Because the offending file
/// was still on disk, every subsequent run failed identically: `schedule
/// install`, the only command that arms health-check/snapshot-auto/
/// throttle-tick/per-group scrub timers, was permanently wedged. Both prune
/// sites below now go through `disable_and_prune_unit`, following the SAME
/// precedent already established immediately adjacent in the older code
/// for `unowned_lookalikes` (a warning, then continue) and in
/// `OrchestrationEngine::remove_group_scrub_unit` (collects the disable
/// failure, deletes the file anyway): a failed disable never blocks the
/// deletion, and never blocks the install that follows.
fn install_schedule_units(
    unit_dir: &std::path::Path,
    state: &shr_state::StateFile,
    exe_path: &std::path::Path,
    policy: &shr_state::policy::PolicyFile,
    runner: &dyn CommandRunner,
) -> Result<(usize, usize, Vec<String>)> {
    let mut warnings: Vec<String> = Vec::new();
    // Prune any shr-rs-owned `shr-rs-scrub-*` unit left behind for a
    // group state.toml no longer has (e.g. `destroy()`d on a older
    // binary, or a run of this same cleanup that failed partway through)
    // -- BEFORE writing/enabling this run's units, same "clean up stale
    // first" ordering `write_mdadm_conf`/`write_fstab` already use via
    // their managed-block splice. A same-named unit with no shr-rs marker
    // is reported, never touched.
    let orphans = shr_state::conf::find_orphaned_scrub_units(unit_dir, state)
        .context("scanning for orphaned scrub timer units")?;
    let mut pruned = 0usize;
    // `find_orphaned_scrub_units` returns files in whatever order
    // `read_dir` yields -- each path (service or timer, for whichever
    // orphaned group(s)) is handled independently, never assumed to be
    // adjacent/paired with its sibling.
    for unit_path in &orphans.owned {
        if disable_and_prune_unit(runner, unit_path, &mut warnings)? {
            pruned += 1;
        }
    }
    for lookalike in &orphans.unowned_lookalikes {
        warnings.push(format!(
            "warning: `{}` looks like a shr-rs error-check unit for a group that no longer \
             exists, but carries no shr-rs ownership marker -- left untouched (looks hand-written)",
            lookalike.display()
        ));
    }

    let mut units =
        shr_state::conf::write_scrub_timer_units(unit_dir, state, exe_path, policy.scrub.priority.as_deref())
            .context("writing scrub timer units")?;
    units.extend(
        shr_state::conf::write_throttle_timer_unit(unit_dir, exe_path)
            .context("writing throttle timer unit")?,
    );
    units.extend(
        shr_state::conf::write_health_check_timer_unit(unit_dir, exe_path)
            .context("writing health check timer unit")?,
    );
    // The snapshot-automation timer only gets installed/enabled when
    // the operator has actually opted in (`policy.toml`'s
    // `[snapshot].enabled`) -- an always-installed timer that immediately
    // no-ops every tick is more confusing to find in `systemctl
    // list-timers` than simply not existing. Running this command again
    // after flipping `enabled` back to `false` removes it via the SAME
    // pruning path used above for destroyed groups: `find_orphaned_scrub_
    // units` only scans `shr-rs-scrub-*`, so the snapshot timer needs its
    // own small symmetric check here instead.
    let snapshot_timer_path = unit_dir.join("shr-rs-snapshot-auto.timer");
    let snapshot_service_path = unit_dir.join("shr-rs-snapshot-auto.service");
    if policy.snapshot.enabled {
        units.extend(
            shr_state::conf::write_snapshot_timer_unit(unit_dir, exe_path, &policy.snapshot.schedule)
                .context("writing snapshot automation timer unit")?,
        );
    } else if shr_state::conf::is_shr_rs_owned_unit(&snapshot_timer_path) {
        for path in [&snapshot_timer_path, &snapshot_service_path] {
            if disable_and_prune_unit(runner, path, &mut warnings)? {
                pruned += 1;
            }
        }
    }

    runner
        .run("systemctl", &["daemon-reload"])
        .context("systemctl daemon-reload")?;
    for unit in &units {
        if unit.extension().and_then(|e| e.to_str()) != Some("timer") {
            continue;
        }
        let unit_name = unit.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        runner
            .run("systemctl", &["enable", "--now", unit_name])
            .with_context(|| format!("systemctl enable --now {unit_name}"))?;
    }
    let installed_timers = units
        .iter()
        .filter(|u| u.extension().and_then(|e| e.to_str()) == Some("timer"))
        .count();
    Ok((installed_timers, pruned, warnings))
}

/// One human-readable line per [`ReconcileAction`] `reconcile()`
/// actually performed -- printed in the order the engine returned them,
/// BEFORE any "here's what's still pending" line, so `shr-rs reconcile`'s
/// text output leads with what just happened instead of burying it (or, as
/// the real-guest repro showed, omitting it while quietly acting anyway).
fn describe_reconcile_action(action: &ReconcileAction) -> String {
    match action {
        ReconcileAction::MemberRemoved {
            group,
            band_index,
            md_name,
            member_path,
        } => format!(
            "Group `{group}` band {band_index} ({md_name}): removed the old disk \
             `{member_path}` -- its replacement had finished syncing."
        ),
        ReconcileAction::ResizeCompleted {
            group,
            band_index,
            md_name,
        } => format!(
            "Group `{group}` band {band_index} ({md_name}): finished growing the storage onto \
             the new space -- its RAID rebuild had completed."
        ),
        ReconcileAction::ScrubSelfHealed {
            group,
            band_index,
            md_name,
            error_count,
        } => format!(
            "Group `{group}` band {band_index} ({md_name}): a scheduled error check had \
             finished on its own; recorded the result ({error_count} error(s))."
        ),
        // No group or band: the parameter is one host-wide kernel setting,
        // and saying otherwise would suggest it can be tuned per array.
        ReconcileAction::SpeedLimitRestored { speed_kb } => format!(
            "Restored the system's RAID speed limit to {speed_kb} KB/s -- the rebuild or error \
             check that had lowered it has finished."
        ),
    }
}

/// The `--json` shape of a `reconcile()` call -- BEFORE this fix, `--
/// json` mode dumped only the post-reconcile `StateFile`, the exact same
/// blind spot the text report had (a completed member removal/resize is
/// indistinguishable from one that was never pending). `performed` carries
/// the same facts [`describe_reconcile_action`] renders as text, in the
/// same order, each tagged with a machine-stable `kind` so a Cockpit/script
/// consumer doesn't have to parse prose.
fn reconcile_report_json(outcome: &ReconcileOutcome) -> serde_json::Value {
    let performed: Vec<serde_json::Value> = outcome
        .performed
        .iter()
        .map(|action| match action {
            ReconcileAction::MemberRemoved {
                group,
                band_index,
                md_name,
                member_path,
            } => serde_json::json!({
                "kind": "member_removed",
                "group": group,
                "band_index": band_index,
                "md_name": md_name,
                "member_path": member_path,
            }),
            ReconcileAction::ResizeCompleted {
                group,
                band_index,
                md_name,
            } => serde_json::json!({
                "kind": "resize_completed",
                "group": group,
                "band_index": band_index,
                "md_name": md_name,
            }),
            ReconcileAction::ScrubSelfHealed {
                group,
                band_index,
                md_name,
                error_count,
            } => serde_json::json!({
                "kind": "scrub_self_healed",
                "group": group,
                "band_index": band_index,
                "md_name": md_name,
                "error_count": error_count,
            }),
            ReconcileAction::SpeedLimitRestored { speed_kb } => serde_json::json!({
                "kind": "speed_limit_restored",
                "speed_kb": speed_kb,
            }),
        })
        .collect();
    serde_json::json!({
        "performed": performed,
        "state": outcome.state,
    })
}

/// `--name`-optional commands (scrub start/cancel, snapshot create)
/// need the resolved group name for their `--json` report, but the engine
/// methods behind them (`scrub_start`/`scrub_cancel`/`snapshot_create`)
/// return `Result<(), OrchestrateError>` -- no group name comes back. This
/// mirrors the SAME "explicit name, else the sole existing group" fallback
/// `recompress`/`destroy`'s `confirm_name` binding already uses above (see
/// those handlers) rather than reproducing `OrchestrationEngine::
/// resolve_group_index`'s full logic here: the engine call remains the real
/// authority on which group gets touched, and it errors BEFORE any of these
/// call sites ever print, on "no active array" or "multiple groups, --name
/// required" -- this function's simpler fallback (silently pick the first
/// group) is only ever observed on the success path, where it is
/// guaranteed to agree with what the engine actually resolved.
fn resolve_group_name_for_report(state_store: &StateStore, name: Option<&str>) -> String {
    name.map(str::to_string).unwrap_or_else(|| {
        state_store
            .load()
            .ok()
            .flatten()
            .and_then(|s| s.groups.first().map(|g| g.name.clone()))
            .unwrap_or_default()
    })
}

/// The `--json` shape for `scrub start`/`scrub cancel` -- both used to
/// read `cli.json` never and always print a bare human sentence. Follows
/// `ScrubCmd::Status`'s own idiom just below in `dispatch` (a bare, compact
/// `serde_json::json!` object, not `to_string_pretty`, and not a typed
/// struct) rather than the pretty-printed whole-report idiom `Status`/
/// `Reconcile`/`Destroy` use elsewhere in this file -- those print an
/// entire `StatusReport`/`StateFile`; this is a small action-confirmation
/// object, the same shape as the `Internal::*Tick` arms' `json!({ "ok":
/// true, .. })`.
fn scrub_action_report_json(group: &str, status: &str) -> serde_json::Value {
    serde_json::json!({ "group": group, "status": status })
}

/// "This project borrowed the host-wide speed limit and nothing has handed
/// it back, even though nothing is syncing anymore." Only the periodic timer
/// and `reconcile` restore it, and on a host where `schedule install` was
/// never run there is no timer -- so the borrowed value silently governs
/// every later md sync until someone notices. Naming the command that fixes
/// it is the point; reporting only the fact would leave an operator to
/// guess.
///
/// `None` (nothing to say) whenever no value is saved, or any band is still
/// syncing, where the limit is doing exactly what it should be.
fn stale_speed_limit_warning(
    state: Option<&shr_state::StateFile>,
    report: &shr_command::StatusReport,
) -> Option<String> {
    let saved_kb = state?.saved_speed_limit_max_kb?;
    let any_syncing = report
        .groups
        .iter()
        .flat_map(|g| g.bands.iter())
        .any(|b| b.sync.is_some());
    if any_syncing {
        return None;
    }
    Some(format!(
        "warning: shr-rs still holds the host-wide sync speed limit ({saved_kb} KB/s was here \
         before), but nothing is syncing -- run `shr-rs reconcile` to put it back"
    ))
}

/// `scrub start`'s `--json` shape: `scrub_action_report_json`'s object plus
/// the speed profile, when `--priority` asked for one.
///
/// The key is OMITTED rather than set to null when no profile was given, so
/// a caller can tell "the scrub runs under whatever limit was already in
/// place" apart from any particular profile -- the same distinction
/// `OrchestrationEngine::scrub_start`'s `Option<SyncPriority>` draws.
fn scrub_start_report_json(group: &str, priority: Option<&str>) -> serde_json::Value {
    let mut value = scrub_action_report_json(group, "started");
    if let Some(p) = priority {
        value["priority"] = serde_json::json!(p);
    }
    value
}

/// The `--json` shape for `fs recompress`. Includes the group and the
/// new compression setting -- information the caller supplied but doesn't
/// otherwise get echoed back, same reasoning as `scrub_action_report_json`.
fn recompress_report_json(group: &str, compression: &str) -> serde_json::Value {
    serde_json::json!({ "group": group, "compression": compression, "status": "started" })
}

/// The `--json` shape for `fs snapshot create`. Both the resolved
/// group and the snapshot name -- a caller that omitted `--group` (single-
/// group host) otherwise has no way to learn which group `--json` mode
/// just snapshotted.
fn snapshot_create_report_json(group: &str, name: &str) -> serde_json::Value {
    serde_json::json!({ "group": group, "name": name, "status": "created" })
}

/// The `--json` shape for `schedule install`. Both counts the human
/// text already computes (installed timer count, pruned orphan count) --
/// no new data, just the same two numbers machine-readable.
fn schedule_install_report_json(installed_timers: usize, pruned: usize) -> serde_json::Value {
    serde_json::json!({ "installed_timers": installed_timers, "pruned": pruned })
}

/// `Command::Daemon`'s `--json` shape -- one compact object per 10s
/// tick (NDJSON: newline-delimited, each line independently parseable), not
/// `to_string_pretty` (a multi-line pretty object would break the
/// one-line-per-tick contract a streaming consumer relies on). This is
/// deliberately NOT the same choice `status --watch` made just above in
/// `dispatch` (see that handler's own `bail!`): `--watch` redraws the SAME
/// screen region in place via cursor movement, which has no sensible JSON-
/// stream meaning at all, so it refuses `--json` outright. This loop only
/// ever APPENDS a new block once per tick and never moves the cursor --
/// the same append-only shape `journalctl -f -o json` (or any tailer)
/// produces -- so NDJSON is the natural fit here, not a rejection. Mirrors
/// the text branch's own per-group fields (name/mode/layout_version/disk
/// count) exactly, including the "no groups" case collapsing to an empty
/// list, same as the text branch's "No active array state loaded." not
/// distinguishing "state.toml missing" from "state.toml has zero groups".
fn daemon_tick_report_json(groups: &[ArrayState]) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = groups
        .iter()
        .map(|g| {
            serde_json::json!({
                "name": g.name,
                "mode": g.mode,
                "layout_version": g.layout_version,
                "disks": g.disks.len(),
            })
        })
        .collect();
    serde_json::json!({ "groups": groups })
}

/// Append a dry-run ASCII layout diagram to `create --dry-run`'s text
/// output. `resolved` must be the exact disk set the preceding
/// `preview_create` call planned against -- `render_layout_diagram` only
/// visualizes an existing plan, so `plan_initial` is called a second time
/// here on the same input rather than threading `PlannerOutput` out of
/// `shr-orchestrate`.
/// Wasteful only in the sense of one extra pure computation on a handful of
/// disks; `preview_create` above already proved this exact input plans
/// successfully, so this call cannot newly fail in practice.
fn dry_run_layout_diagram_text(resolved: &[ResolvedDisk], mode: RedundancyMode) -> Result<String> {
    let disks: Vec<Disk> = resolved.iter().map(|d| d.to_planner_disk()).collect();
    let out = plan_initial(&PlannerInput::new(disks.clone(), mode))?;
    Ok(render::render_layout_diagram(&disks, &out))
}

/// For `expand --dry-run`: unlike `create`, expand has no
/// `RedundancyMode`/disk-set argument of its own (a group's mode never
/// changes) -- both come from `preview_state`, which `preview_expand_against`
/// already returned with the new disks folded into `ArrayState::disks`
/// (`OrchestrationEngine::expand` pushes them there; see `engine.rs`'s
/// `new_state_disk` call sites). Recomputes the ideal *full* layout
/// (existing + new disks) the same way `shr_core::plan_expansion` does
/// internally to validate the expansion -- band_alignment/reserved_head/tail
/// are never persisted in `ArrayState` because every group is planned with
/// `PlannerInput::new`'s production defaults, so recomputing on those same
/// defaults here reproduces the identical grid.
fn expand_dry_run_layout_diagram_text(state: &ArrayState) -> Result<String> {
    let mode = parse_redundancy_mode(&state.mode)?;
    let disks: Vec<Disk> = state
        .disks
        .iter()
        .map(|d| Disk::new(d.id.clone(), d.size_bytes))
        .collect();
    let out = plan_initial(&PlannerInput::new(disks.clone(), mode))?;
    Ok(render::render_layout_diagram(&disks, &out))
}

fn parse_redundancy_mode(s: &str) -> Result<RedundancyMode> {
    match s {
        "shr" => Ok(RedundancyMode::Shr),
        "shr2" => Ok(RedundancyMode::Shr2),
        other => bail!("unknown redundancy mode `{other}` in the shr-rs configuration"),
    }
}

/// D1 `fs df`: best-effort live Btrfs usage for every group, keyed by group
/// name -- each group's figures come from `BtrfsExecutor::usage`/
/// `free_bytes` against its own mount point. A failure (unmounted, `btrfs`/
/// `df` not on `PATH` -- e.g. this project's own Windows dev host) leaves
/// that group's entry all `None` rather than aborting the whole `fs df`
/// report or fabricating a figure (see `FsUsageInput`'s doc comment: an
/// absent/failed read is a fully valid, honest outcome, not a placeholder to
/// fix later). Pulled out of `dispatch` so this wiring is unit-testable
/// without a real `CommandRunner`.
fn fs_usage_map(
    runner: &dyn CommandRunner,
    groups: &[shr_command::GroupStatus],
) -> BTreeMap<String, shr_command::FsUsageInput> {
    let btrfs = shr_exec::BtrfsExecutor::new(runner);
    groups
        .iter()
        .map(|g| (g.name.clone(), fs_usage_input(&btrfs, &g.mount_point)))
        .collect()
}

fn fs_usage_input(btrfs: &shr_exec::BtrfsExecutor<'_>, mount_point: &str) -> shr_command::FsUsageInput {
    let usage = btrfs.usage(mount_point).unwrap_or_default();
    shr_command::FsUsageInput {
        data_used_bytes: usage.data_used_bytes,
        data_total_bytes: usage.data_total_bytes,
        metadata_used_bytes: usage.metadata_used_bytes,
        metadata_total_bytes: usage.metadata_total_bytes,
        unallocated_bytes: usage.unallocated_bytes,
        statvfs_avail_bytes: btrfs.free_bytes(mount_point).unwrap_or_default(),
    }
}

/// Terminal rows [`render_status_watch_frame`] must NOT draw into, so its
/// frame's own trailing newline can never scroll the terminal by one line --
/// see `run_status_watch`'s doc comment for why an unplanned scroll breaks
/// the redraw loop. `.max(1)` keeps a degenerate one-row terminal drawable
/// (a blank/truncated frame) rather than producing a zero-height one
/// `render_status_watch_frame` was never asked to handle.
fn watch_frame_meta_from(cols: u16, rows: u16) -> render::WatchFrameMeta {
    render::WatchFrameMeta {
        width: cols as usize,
        max_height: rows.saturating_sub(1).max(1) as usize,
    }
}

/// `status --watch`'s redraw loop. `render_status_watch_frame` (the pure
/// function `shr-command` provides) only draws ONE frame, always exactly
/// `meta.max_height` lines of exactly `meta.width` columns -- this loop's
/// entire job is measuring the terminal once, then repeatedly re-fetching
/// status, redrawing, and sleeping.
///
/// Never clears the screen (would flicker) and never enables raw mode or an
/// alternate screen buffer -- it moves the cursor back up by
/// `meta.max_height` lines and overwrites in place, which only works because
/// every frame is the exact same fixed size (this is also why
/// `watch_frame_meta_from` reserves one row: printing exactly `rows` lines
/// plus the final newline would scroll the terminal by one line right when
/// the cursor is already on the last row, silently breaking the "move up N"
/// math on the next iteration). Because no terminal mode is ever changed,
/// there is nothing to restore on exit: Ctrl-C's default SIGINT action just
/// terminates the process, and the terminal is left exactly as it was --
/// satisfying the "must not leave the terminal broken" requirement by never
/// touching terminal modes in the first place, rather than by installing a
/// signal handler that has to remember to undo them.
///
/// Rejected outright when stdout is not a real terminal at all: looping
/// forever writing frames into a pipe/redirect is never useful and was
/// explicitly ruled out for this wave. Checked via `std::io::IsTerminal`
/// (the same primitive `shr_command::ui_mode`'s `is_interactive_terminal`/
/// `can_prompt_operator` already use for this exact question), NOT by
/// treating a `crossterm::terminal::size()` failure as the non-TTY signal --
/// on this project's Windows dev host `crossterm::terminal::size()` returns
/// `Ok` unconditionally regardless of whether stdout is a real console, a
/// pipe, or a redirected file (verified directly: see this wave's report),
/// so it cannot be trusted alone to detect "not a terminal" even though it
/// reliably is on Linux via `ioctl`/`TIOCGWINSZ`. `is_terminal()` is checked
/// first and unconditionally, on every platform, closing that gap.
fn run_status_watch(interval: Duration) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        bail!(
            "`status --watch` needs a real terminal, and this output is redirected -- for \
             scripts or piped output, run `status --detail` on your own interval instead, \
             e.g. `watch -n2 shr-rs status --detail`"
        );
    }
    let (cols, rows) = crossterm::terminal::size().context("reading terminal size for status --watch")?;
    let meta = watch_frame_meta_from(cols, rows);
    let mut first_frame = true;
    loop {
        let state = StateStore::new(STATE_PATH).load().with_context(state_load_ctx)?;
        let report = build_status(&SystemInspector, state.as_ref())
            .context("inspecting system (needs lsblk / /proc/mdstat / smartctl on Linux)")?;
        let frame = render::render_status_watch_frame(&report, &meta);
        if !first_frame {
            crossterm::execute!(
                std::io::stdout(),
                crossterm::cursor::MoveUp(meta.max_height as u16)
            )
            .context("moving the cursor for status --watch's redraw")?;
        }
        first_frame = false;
        println!("{frame}");
        thread::sleep(interval);
    }
}

/// `--yes` or a non-interactive caller keeps today's silent
/// auto-approve. An interactive TTY without `--yes` must show the planned
/// commands and the irreversibility warning, then require the operator to
/// type `expected_name` (the resolved group's real name -- from the
/// preview, so it's correct even when the caller omitted `--name`) exactly,
/// same gate as `shr-tui`'s `wizard::AddDiskController::can_execute`. Bails
/// with an error BEFORE any real command runs on a mismatch or empty input
/// -- both call sites place this ahead of `acquire_state_lock`/
/// `OrchestrationEngine::new(...)`, so a bailed `?` here guarantees the
/// engine (and therefore `SystemRunner`) is never even constructed, let
/// alone asked to run anything destructive.
///
/// `interactive`/`input` are dependency-injected (rather than this function
/// calling `shr_command::can_prompt_operator()`/`io::stdin()` directly) so
/// the mismatch-cancels path is unit-testable without a real terminal --
/// see the `require_typed_confirmation_*` tests below.
fn require_typed_confirmation(
    expected_name: &str,
    planned_commands: &[String],
    yes: bool,
    operation: &str,
    interactive: bool,
    input: &mut dyn std::io::BufRead,
) -> Result<()> {
    if yes || !interactive {
        return Ok(());
    }

    use std::io::Write as _;
    print!("{}", render_planned_commands_text(planned_commands));
    println!(
        "This will make real, IRREVERSIBLE changes to group `{expected_name}` ({operation}) -- it \
         cannot be undone once started."
    );
    print!("Type the group name (`{expected_name}`) to confirm, anything else cancels: ");
    std::io::stdout().flush().ok();

    let mut typed_line = String::new();
    input
        .read_line(&mut typed_line)
        .context("reading confirmation from stdin")?;
    let typed = typed_line.trim_end_matches(['\n', '\r']);

    if typed != expected_name {
        bail!("{operation} of group `{expected_name}` cancelled: confirmation text did not match");
    }
    Ok(())
}

/// Decide whether `destroy` also wipes the member partitions' mdadm
/// superblocks, from the two mutually exclusive flags plus, when neither is
/// given, the operator.
///
/// This used to be a bare `--zero-superblocks: bool` defaulting to off, so
/// an operator who never thought about it silently got "markers left
/// behind" -- a real decision (it is the difference between disks that can
/// still be pieced back together by hand and disks that cannot) made by
/// nobody. Cockpit has always presented it as a checkbox; the CLI now
/// insists on an answer too.
///
/// Non-interactive (`--yes`, or no terminal to prompt on) with neither flag
/// is an ERROR rather than a fallback to the old default: a script that
/// never states an intent is exactly the case that used to get one assigned
/// to it. Existing scripts need one flag added, once.
fn resolve_zero_superblocks(
    zero: bool,
    no_zero: bool,
    yes: bool,
    interactive: bool,
    input: &mut dyn std::io::BufRead,
) -> Result<bool> {
    // clap enforces the mutual exclusion; this is the belt-and-braces read.
    if zero && no_zero {
        bail!("--zero-superblocks and --no-zero-superblocks cannot both be given");
    }
    if zero {
        return Ok(true);
    }
    if no_zero {
        return Ok(false);
    }

    if yes || !interactive {
        bail!(
            "destroy needs an explicit decision about the RAID markers on these disks: pass \
             --zero-superblocks to erase them (disks left blank) or --no-zero-superblocks to \
             leave them in place (recoverable by hand). Either way the array is recorded so it \
             is never auto-assembled again."
        );
    }

    use std::io::Write as _;
    println!(
        "Erase the RAID markers on these disks as well?\n  \
         yes -- the disks are left completely blank\n  \
         no  -- the markers stay, so the old arrangement can still be pieced back together by hand"
    );
    print!("Type `yes` or `no`: ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("reading the superblock decision from stdin")?;
    match line.trim() {
        "yes" | "y" => Ok(true),
        "no" | "n" => Ok(false),
        other => {
            bail!("destroy cancelled: expected `yes` or `no` for the RAID marker decision, got `{other}`")
        }
    }
}

fn render_preflight(r: &shr_inspect::WritePreflight) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "SHR-RS preflight: {}", if r.ok { "OK" } else { "BLOCKED" });
    for t in &r.targets {
        let id = t.id.as_deref().unwrap_or("(no stable id)");
        let _ = writeln!(
            out,
            "  {}  id={}  system={}  content={}",
            t.kernel_name, id, t.system_disk, t.has_content
        );
        if !t.system_mounts.is_empty() {
            let _ = writeln!(out, "    mounts: {}", t.system_mounts.join(", "));
        }
    }
    for b in &r.blockers {
        let _ = writeln!(out, "  BLOCK: {b}");
    }
    for w in &r.warnings {
        let _ = writeln!(out, "  WARN:  {w}");
    }
    // `WriteBlocker::HasContent`'s shared message used to name
    // `--force-content` itself, which was wrong for the TUI/Cockpit
    // callers rendering the same string. shr-inspect no longer names any
    // frontend's control, so the CLI -- the one frontend where this flag
    // actually applies -- states it here instead.
    if r.blockers
        .iter()
        .any(|b| matches!(b, shr_inspect::WriteBlocker::HasContent { .. }))
    {
        let _ = writeln!(
            out,
            "  (pass --force-content to reuse a disk with existing content anyway)"
        );
    }
    out
}

fn resolve_kernel_names(disks: &[String]) -> Result<Vec<String>> {
    let inspector = SystemInspector;
    let lsblk = inspector.block_devices().context("running lsblk")?;
    let by_id = inspector.by_id_index().context("scanning /dev/disk/by-id")?;
    disks
        .iter()
        .map(|raw| {
            let clean = raw.trim().trim_start_matches("/dev/disk/by-id/");
            let r = DiskRef::parse(clean);
            match resolve_disk_ref(&r, &lsblk, &by_id) {
                Ok(resolved) => Ok(resolved.kernel_name),
                Err(e) => {
                    if let DiskRef::Path(p) = &r {
                        let name = p.trim().trim_start_matches("/dev/").to_string();
                        if lsblk.disks().any(|d| d.name == name) {
                            return Ok(name);
                        }
                    }
                    Err(anyhow!(e))
                }
            }
        })
        .collect()
}

/// Resolve user-supplied disk references to stable identity + live metadata
/// (real by-id name, kernel name, size, serial, model) via `shr-inspect`.
/// This is what `create`/`expand` feed into `shr-orchestrate`'s request
/// types, so the engine never has to invent identifiers (D3).
fn resolve_real_disks(disks: &[String]) -> Result<Vec<ResolvedDisk>> {
    let inspector = SystemInspector;
    let lsblk = inspector.block_devices().context("running lsblk")?;
    let by_id = inspector.by_id_index().context("scanning /dev/disk/by-id")?;

    disks
        .iter()
        .map(|raw| {
            let clean = raw.trim().trim_start_matches("/dev/disk/by-id/");
            let r = DiskRef::parse(clean);
            resolve_disk_ref(&r, &lsblk, &by_id).map_err(|e| anyhow!(e))
        })
        .collect()
}

fn emit<T: serde::Serialize>(json: bool, report: &T, render: impl Fn(&T) -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print!("{}", render(report));
    }
    Ok(())
}

fn resolve_disks(sizes: &[String], disks: &[String]) -> Result<Vec<Disk>> {
    if !sizes.is_empty() {
        sizes
            .iter()
            .enumerate()
            .map(|(i, s)| Ok(Disk::new(format!("disk{i}"), parse_size(s)?)))
            .collect()
    } else if !disks.is_empty() {
        let inspector = SystemInspector;
        let lsblk = inspector.block_devices().context("running lsblk")?;
        let by_id = inspector.by_id_index().context("scanning /dev/disk/by-id")?;
        let mut resolved = Vec::new();
        for raw in disks {
            let r = DiskRef::parse(raw);
            let disk = resolve_disk_ref(&r, &lsblk, &by_id).map_err(|e| anyhow!(e))?;
            let blockers = disk.write_blockers();
            if !blockers.is_empty() {
                return Err(anyhow!(format_blockers(&blockers)));
            }
            resolved.push(disk.to_planner_disk());
        }
        Ok(resolved)
    } else {
        bail!("provide either --sizes or --disks")
    }
}

fn format_blockers(blockers: &[WriteBlocker]) -> String {
    blockers
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult): (&str, u64) = if let Some(n) = s.strip_suffix("TiB") {
        (n, 1u64 << 40)
    } else if let Some(n) = s.strip_suffix("GiB") {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix("TB").or_else(|| s.strip_suffix('T')) {
        (n, 1_000_000_000_000)
    } else if let Some(n) = s.strip_suffix("GB").or_else(|| s.strip_suffix('G')) {
        (n, 1_000_000_000)
    } else if let Some(n) = s.strip_suffix("MB").or_else(|| s.strip_suffix('M')) {
        (n, 1_000_000)
    } else {
        (s, 1)
    };
    let value: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size `{s}`"))?;
    if !value.is_finite() {
        bail!("size `{s}` is not a finite number");
    }
    if value < 0.0 {
        bail!("size `{s}` is negative");
    }
    let bytes = value * mult as f64;
    if bytes >= u64::MAX as f64 {
        bail!("size `{s}` is too large");
    }
    Ok(bytes as u64)
}

/// Splice a dry-run's recorded command list into the JSON already produced
/// for `state` (D13), in the exact order they'd run for real. Kept separate
/// from `ArrayState`'s own schema -- `planned_commands` is an artifact of
/// this one CLI invocation, not persisted array state, and adding it as a
/// real field would leak into `state.toml`/the real (non-dry-run) JSON
/// output the Cockpit plugin's schema already validates against.
fn merge_planned_commands(state: &impl serde::Serialize, commands: &[String]) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(state)?;
    if let serde_json::Value::Object(ref mut map) = value {
        map.insert("planned_commands".to_string(), serde_json::to_value(commands)?);
    }
    Ok(value)
}

/// Human-readable rendering of a dry-run's recorded command list, in the
/// exact order they'd run for real (D13).
fn render_planned_commands_text(commands: &[String]) -> String {
    let mut out = String::from("Planned commands:\n");
    for cmd in commands {
        out.push_str("  ");
        out.push_str(cmd);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn production_engine_points_conf_paths_at_the_real_etc_locations() {
        // Regression: `Recompress`'s handler used to build its own
        // engine inline and forget `.with_conf_paths`, silently writing
        // `fstab` next to `state.toml` instead of to `/etc/fstab`. Every
        // command that writes real system config now goes through this ONE
        // helper -- proving the helper is wired correctly proves every one
        // of its callers is, without needing to exercise the real
        // filesystem (which `dispatch` itself can't be, on this Windows
        // dev host -- see the crate's `CommandRunner` convention).
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.toml")));
        let sys_runner = SystemRunner::new();
        let engine = production_engine(&sys_runner, store);
        assert_eq!(engine.mdadm_conf_path(), std::path::Path::new(MDADM_CONF_PATH));
        assert_eq!(engine.fstab_path(), std::path::Path::new(FSTAB_PATH));
        // `destroy()`'s scrub-unit cleanup must look in the real
        // systemd unit directory, not the tempdir-adjacent default a test
        // engine gets -- same class of regression already caught
        // for mdadm.conf/fstab.
        assert_eq!(engine.unit_dir(), std::path::Path::new(SYSTEMD_UNIT_DIR));
    }

    #[test]
    fn every_handler_that_writes_managed_configs_builds_its_engine_via_production_engine() {
        // Came straight back: `disk replace` reaches
        // `write_managed_configs` exactly like create/expand/reconcile/
        // recompress do, but its handler built an engine inline and so wrote
        // mdadm.conf and fstab next to state.toml instead of into /etc --
        // silently, because that path is just as writable. The test above
        // proves `production_engine` is correct, which says nothing about a
        // handler that never calls it, so scan the dispatch source itself:
        // any engine binding whose scope reaches a config-writing engine
        // method has to come from the helper. Add to WRITERS whenever an
        // engine method starts writing managed config.
        const WRITERS: [&str; 6] = [
            "engine.create(",
            "engine.expand(",
            "engine.reconcile(",
            "engine.recompress(",
            "engine.replace_disk(",
            "engine.destroy(",
        ];
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");

        let mut checked = 0;
        for chunk in dispatch.split("let engine = ").skip(1) {
            // A chunk spans one engine binding up to just before the next,
            // so a writer call always lands in its own binding's chunk.
            let Some(writer) = WRITERS.iter().find(|w| chunk.contains(**w)) else {
                continue;
            };
            assert!(
                chunk.starts_with("production_engine("),
                "a handler calling `{writer}` builds its engine inline instead of via production_engine(), \
                 so its mdadm.conf/fstab writes miss /etc"
            );
            checked += 1;
        }
        assert_eq!(
            checked,
            WRITERS.len(),
            "every config-writing engine method should be dispatched exactly once"
        );
    }

    #[test]
    fn every_long_running_handler_wires_a_progress_sink_and_the_multi_hour_ones_stream_it() {
        // SPEC S17 claims ProgressSink is "used by every long-running
        // operation", but `create`/`expand` -- precisely the two operations
        // that run for hours (a reshape) or perform the most destructive
        // sequence (create) -- passed nothing and silently fell back to
        // NullProgressSink. A CLI/SSH operator saw nothing between
        // invocation and completion; TUI/Cockpit at least get periodic
        // state polling. Same source-scan technique as the WRITERS test
        // above, since exercising `dispatch` for real needs a live system
        // this Windows dev host doesn't have.
        //
        // Two tiers, because "wires a sink" is NOT the property that
        // matters for the slow ones. `RecordingProgressSink` buffers and is
        // drained after the engine call returns -- fine for
        // `reconcile`/`disk replace`, whose updates are end-of-run
        // summaries, useless for a multi-hour `create`/`expand` where the
        // whole point is watching it work. The first version of this fix
        // wired the buffered sink to all four and read as a fix while
        // leaving the slow operators staring at a dead terminal, so this
        // test pins the streaming sink specifically where it's load-bearing.
        const STREAMING: [&str; 2] = ["engine.create(", "engine.expand("];
        const BUFFERED_OK: [&str; 2] = ["engine.reconcile(", "engine.replace_disk("];
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");

        let mut checked = 0;
        for chunk in dispatch.split("let engine = ").skip(1) {
            let call = STREAMING
                .iter()
                .chain(BUFFERED_OK.iter())
                .find(|w| chunk.contains(**w));
            let Some(call) = call else { continue };
            assert!(
                chunk.contains(".with_progress_sink(&progress)"),
                "{call} builds an engine with no progress sink -- a CLI operator sees nothing \
                 while it runs"
            );
            checked += 1;
        }

        // The `let progress = ...` binding sits BEFORE `let engine = ...`, so
        // it lands in the PREVIOUS chunk of the split above -- checking the
        // sink type there would be checking the wrong handler. Locate each
        // call directly and read backwards to its nearest preceding binding.
        for call in STREAMING {
            let at = dispatch
                .find(call)
                .unwrap_or_else(|| panic!("{call} not found in dispatch"));
            let (before, _) = dispatch.split_at(at);
            let bind = before
                .rfind("let progress = ")
                .unwrap_or_else(|| panic!("{call} has no `let progress` binding before it"));
            let decl = before[bind..].lines().next().unwrap_or_default();
            assert!(
                decl.contains("TextProgressSink::new(std::io::stderr())"),
                "{call} can run for hours; its progress must stream to stderr as it happens, \
                 not be buffered in a RecordingProgressSink and dumped after it returns. \
                 Found: {decl}"
            );
        }
        assert_eq!(
            checked,
            STREAMING.len() + BUFFERED_OK.len(),
            "every long-running engine call should be dispatched exactly once"
        );
    }

    #[test]
    fn recompress_handler_is_gated_by_require_typed_confirmation_before_the_real_engine_call() {
        // `recompress` ran immediately, with no
        // `--yes` flag and no confirmation gate at all, unlike create/
        // expand/disk-replace/destroy -- despite rewriting EVERY file's
        // data extents (hours of real IO) and, now that `@snapshots` can
        // exist, risking a used-space spike from broken extent
        // sharing. Same source-scan technique as the WRITERS test above,
        // since exercising `dispatch` for real needs a live system this
        // Windows dev host doesn't have: prove the call exists AND runs
        // BEFORE `engine.recompress(` -- gating after the fact would let a
        // rejected/mismatched confirmation not actually stop anything.
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        // Hand-rolled slicing here originally, predating `find_handler`;
        // switched over when adopting rustfmt broke the literal marker (the
        // arm's pattern is four lines now). See `squeeze`.
        let handler = find_handler(dispatch, "Command::Fs { command: FsCmd::Recompress");

        let confirm_pos = handler.position("require_typed_confirmation(").expect(
            "FsCmd::Recompress's handler must call require_typed_confirmation, same as \
             create/expand/disk replace/destroy",
        );
        let engine_call_pos = handler
            .position("engine.recompress(")
            .expect("engine.recompress( call not found in the handler");
        assert!(
            confirm_pos < engine_call_pos,
            "require_typed_confirmation must run BEFORE engine.recompress(), or a rejected/\
             mismatched confirmation still lets the recompress happen"
        );
    }

    #[test]
    fn daemon_help_text_says_it_is_not_a_monitoring_daemon_and_names_schedule_install() {
        // The `Daemon` variant's doc comment (== `shr-rs daemon --help`
        // text) claimed "monitor state and background tasks". The loop
        // underneath only re-reads state.toml every 10s and prints one line
        // per group -- it detects nothing, runs no background task, reacts
        // to nothing. An operator who reads only `--help` and never the spec
        // would reasonably believe their array is being watched. Fix: the
        // help text must say plainly this is not a monitoring daemon AND
        // name `schedule install` (the command that actually installs the
        // periodic scrub/throttle/health-check/snapshot timers, see
        // `ScheduleCmd::Install`) so the operator has somewhere real to go.
        // CRLF-tolerant (unrelated to the report itself): this file's on-disk line
        // endings depend on whatever last saved it, and this Windows dev
        // host has seen it flip to CRLF mid-session. Normalize before
        // searching so this pre-existing test doesn't spuriously fail on a
        // hardcoded LF marker for reasons that have nothing to do with the
        // Daemon variant's actual doc comment.
        let src = include_str!("lib.rs").replace("\r\n", "\n");
        let marker = "\n    Daemon {\n";
        let variant_pos = src
            .find(marker)
            .expect("Daemon variant not found in the Command enum");
        let before = &src[..variant_pos];
        let doc_lines: Vec<&str> = before
            .lines()
            .rev()
            .take_while(|l| l.trim_start().starts_with("///"))
            .collect();
        assert!(
            !doc_lines.is_empty(),
            "Daemon variant has no doc comment directly above it"
        );
        // Strip the leading `///` and joined with a space, not "\n": doc
        // comments wrap prose across lines, so a phrase like "not a
        // monitoring daemon" can legitimately straddle a line break and
        // must still be found as one substring, with no `///` in the way.
        let doc: String = doc_lines
            .into_iter()
            .rev()
            .map(|l| l.trim_start().trim_start_matches("///").trim())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(
            doc.to_lowercase().contains("not a monitoring daemon"),
            "help text must say plainly this is NOT a monitoring daemon (not merely drop the \
             word \"monitoring\" -- a rename to a different lie would still pass a weaker \
             check). Found doc comment:\n{doc}"
        );
        assert!(
            doc.contains("schedule install"),
            "help text must name `shr-rs schedule install` as what actually installs the \
             periodic background work (scrub/throttle/health-check/snapshot timers), so the \
             operator has a real command to reach for. Found doc comment:\n{doc}"
        );
    }

    #[test]
    fn daemon_handler_runtime_banner_says_it_is_not_a_monitoring_daemon_and_names_schedule_install() {
        // Same defect as the help-text test above, but for the banner
        // printed at runtime (`println!` at the top of the `Daemon` arm) --
        // the two are independent strings and either could regress alone.
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let handler = find_handler(dispatch, "Command::Daemon { state_path }");

        assert!(
            handler.contains_ignoring_case("not a monitoring daemon"),
            "the startup banner must say plainly this is not a monitoring daemon. Handler:\n{handler}"
        );
        assert!(
            handler.contains_ignoring_case("schedule install"),
            "the startup banner must name `shr-rs schedule install` as what actually performs \
             periodic background work; otherwise an operator watching this loop has no path to \
             the real mechanism. Handler:\n{handler}"
        );
        assert!(
            !handler.contains_ignoring_case("Starting shr-rs daemon (monitoring"),
            "the old misleading banner text must be gone, not merely joined by a new one. Handler:\n{handler}"
        );
    }

    // ---- Six handlers that used to read `cli.json` never ----
    //
    // Each pure JSON-builder function below is tested by actually parsing
    // its emitted string back with `serde_json::from_str` and checking
    // fields -- not merely asserting that a call to it appears somewhere in
    // the source. That proves the shape of what gets printed. It does NOT
    // by itself prove `dispatch()` reaches the builder with the right
    // arguments at runtime, since exercising the real handlers needs a live
    // system this Windows dev host doesn't have (see this module's other
    // source-scan tests, e.g. the WRITERS/STREAMING ones above, for the
    // established alternative). So each builder test is paired with a
    // source-scan test that confirms, within that ONE handler's own text: a
    // `cli.json` branch reaching the builder call, AND the pre-existing
    // human sentence still present verbatim in the `else` -- catching
    // either "added JSON but broke/moved the human path" or "the JSON
    // branch exists in source but is dead code that never actually runs"
    // (well, the latter not fully -- a source match can't prove the branch
    // executes -- but it does prove the two are wired to the SAME `if`/
    // `else`, not two independent, driftable code paths).

    #[test]
    fn scrub_action_report_json_parses_back_with_group_and_status() {
        let value = scrub_action_report_json("mygroup", "started");
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(parsed["group"], "mygroup");
        assert_eq!(parsed["status"], "started");

        let value = scrub_action_report_json("mygroup", "cancelled");
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(parsed["status"], "cancelled");
    }

    #[test]
    fn tick_skipped_report_json_adds_skipped_without_dropping_the_arms_own_keys() {
        // A skipped run must stay parseable by whatever already reads the
        // successful shape -- a consumer watching `bands_ticked` should see
        // 0 with a reason, not a missing key it has to special-case.
        let value = tick_skipped_report_json(serde_json::json!({ "bands_ticked": 0 }));
        assert_eq!(value["bands_ticked"], 0);
        assert_eq!(value["skipped"], true);

        let value = tick_skipped_report_json(serde_json::json!({ "ok": true }));
        assert_eq!(value["ok"], true);
        assert_eq!(value["skipped"], true);
    }

    /// The periodic timers used to read and write `state.toml` with no lock
    /// at all, while every interactive command took one. A tick landing in
    /// the middle of a `create`/`expand`/`scrub start` could therefore load
    /// an older copy and write it back over that command's -- losing, among
    /// other things, the saved host-wide speed limit, which nothing would
    /// then ever put back.
    ///
    /// Source-scanned within each arm's own bounded text, the same way this
    /// module's other handler tests work (the real handlers need a live
    /// system this dev host does not have). Asserts on the non-blocking
    /// `try_write` + skip specifically: a timer that BLOCKED on the lock, or
    /// that turned contention into an error, would be a different and worse
    /// behavior that a bare "mentions the lock" check would happily accept.
    #[test]
    fn the_periodic_tick_handlers_take_the_state_lock_and_skip_rather_than_fail() {
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");

        for arm in ["InternalCmd::ReshapeThrottleTick", "InternalCmd::HealthCheckTick"] {
            let handler = find_handler(dispatch, &format!("Command::Internal {{ command: {arm} }}"));
            assert!(
                handler.contains("acquire_state_lock()"),
                "{arm} writes state.toml without taking the lock"
            );
            assert!(
                handler.contains("lock.try_write()"),
                "{arm} must not BLOCK on the lock -- a timer that waits pins a process for the \
                 whole of a multi-hour expand"
            );
            assert!(
                handler.contains("report_tick_skipped(cli.json"),
                "{arm} must skip cleanly on contention, not fail the systemd unit every firing"
            );
        }
    }

    /// The counterpart: `snapshot-auto` only READS `state.toml`
    /// (`snapshot_auto_run` never calls `store.save`), so it is not part of
    /// the lost-update race and must NOT take the write lock -- doing so
    /// would skip scheduled snapshots for the whole duration of an expand
    /// for no reason at all.
    #[test]
    fn the_snapshot_tick_does_not_take_a_write_lock_it_does_not_need() {
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let handler = find_handler(
            dispatch,
            "Command::Internal { command: InternalCmd::SnapshotAutoRun }",
        );
        assert!(
            !handler.contains("acquire_state_lock()"),
            "snapshot-auto is a reader; locking it would block scheduled snapshots behind expand"
        );
    }

    #[test]
    fn recompress_report_json_parses_back_with_group_and_compression() {
        let value = recompress_report_json("mygroup", "zstd:5");
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(parsed["group"], "mygroup");
        assert_eq!(parsed["compression"], "zstd:5");
        assert_eq!(parsed["status"], "started");
    }

    #[test]
    fn snapshot_create_report_json_parses_back_with_group_and_name() {
        let value = snapshot_create_report_json("mygroup", "before-upgrade");
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(parsed["group"], "mygroup");
        assert_eq!(parsed["name"], "before-upgrade");
        assert_eq!(parsed["status"], "created");
    }

    #[test]
    fn schedule_install_report_json_parses_back_with_both_counts() {
        let value = schedule_install_report_json(4, 1);
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(parsed["installed_timers"], 4);
        assert_eq!(parsed["pruned"], 1);
    }

    #[test]
    fn daemon_tick_report_json_parses_back_with_per_group_fields_and_the_empty_case() {
        let group = expand_state_fixture("shr");
        let value = daemon_tick_report_json(std::slice::from_ref(&group));
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(parsed["groups"][0]["name"], group.name);
        assert_eq!(parsed["groups"][0]["mode"], group.mode);
        assert_eq!(parsed["groups"][0]["layout_version"], group.layout_version);
        assert_eq!(parsed["groups"][0]["disks"], group.disks.len());

        // The empty case (no state.toml / no groups) must serialize to
        // an empty array, matching the text branch's own "No active array
        // state loaded." not distinguishing "file missing" from "file has
        // zero groups" -- no invented placeholder group, no `null`.
        let empty = daemon_tick_report_json(&[]);
        let parsed_empty: serde_json::Value = serde_json::from_str(&empty.to_string()).unwrap();
        assert_eq!(parsed_empty["groups"], serde_json::json!([]));
    }

    #[test]
    fn resolve_group_name_for_report_prefers_an_explicit_name_over_state_toml() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.toml"));
        // No state.toml written at all -- an explicit name must still win,
        // never touching the (nonexistent) file.
        assert_eq!(
            resolve_group_name_for_report(&store, Some("explicit")),
            "explicit"
        );
    }

    #[test]
    fn resolve_group_name_for_report_falls_back_to_the_sole_group_in_state_toml() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.toml"));
        let state = shr_state::StateFile::new(vec![expand_state_fixture("shr")]);
        store.save(&state).unwrap();
        assert_eq!(resolve_group_name_for_report(&store, None), "shr1");
    }

    #[test]
    fn resolve_group_name_for_report_defaults_to_empty_string_with_no_state_and_no_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.toml"));
        assert_eq!(resolve_group_name_for_report(&store, None), "");
    }

    /// Reduce a source fragment to a form that survives being re-wrapped:
    /// every whitespace character removed, and the trailing comma rustfmt
    /// adds before a closing delimiter when it explodes an argument list
    /// dropped.
    ///
    /// Adopting rustfmt broke all seven source-scanning tests at once, and
    /// none of them because the code they check had changed. `Command::Fs {
    /// command: FsCmd::Scrub { action } }` became four lines, and
    /// `f(&a, &b)` became `f(\n    &a,\n    &b,\n)`, so every literal marker
    /// and every asserted call stopped matching. Re-pinning them to whatever
    /// the formatter produced today would just re-arm the same trap for the
    /// next reformat.
    ///
    /// Squeezing both sides is what makes these assertions about the CODE
    /// rather than about its layout. It does also collapse spaces inside
    /// string literals, which is harmless here: the needle is squeezed the
    /// same way, so `println!("scrub started")` still matches itself and
    /// nothing else in this file squeezes to the same text.
    fn squeeze(source: &str) -> String {
        source
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .replace(",)", ")")
            .replace(",]", "]")
            .replace(",}", "}")
    }

    /// Locate ONE handler arm bounded by a unique starting marker and the
    /// next top-level `Command::` arm -- same technique the pre-existing
    /// `recompress_handler_is_gated_by_require_typed_confirmation_before_
    /// the_real_engine_call`/WRITERS/STREAMING tests above already use, so
    /// this doesn't invent a second convention.
    ///
    /// Returns SQUEEZED text (see `squeeze`), so every caller has to squeeze
    /// what it looks for too. That is deliberate: an assertion written
    /// against the raw source would pass today and break the next time
    /// rustfmt decides a line is one character too long.
    ///
    /// The arm boundary is `}Command::` rather than a line-anchored form for
    /// the same reason -- after squeezing there are no lines left to anchor
    /// to, only the closing brace of the previous arm followed by the next
    /// one. That also makes it CRLF-agnostic for free, which the raw-text
    /// version had to handle explicitly (this Windows host has seen both
    /// line endings for this file).
    fn find_handler(dispatch: &str, start_marker: &str) -> Handler {
        let squeezed = squeeze(dispatch);
        let marker = squeeze(start_marker);
        let start = squeezed
            .find(&marker)
            .unwrap_or_else(|| panic!("{start_marker} not found in dispatch"));
        let end = squeezed[start..]
            .find("}Command::")
            .map(|offset| start + offset)
            .unwrap_or(squeezed.len());
        Handler(squeezed[start..end].to_string())
    }

    /// One handler arm's source, already squeezed.
    ///
    /// The squeezing lives behind `contains`/`arm` rather than at each call
    /// site so the assertions can go on reading as ordinary source text
    /// (`handler.contains("if cli.json")`) instead of every one of them
    /// having to remember to normalize its own needle -- a rule that would
    /// be silently wrong, not loudly wrong, the one time somebody forgot.
    struct Handler(String);

    /// So a failing assertion can still print the arm it was looking at.
    /// Squeezed text is unpleasant to read, but it is what was actually
    /// searched, and a message showing the pretty original would send the
    /// reader hunting for a mismatch that is not there.
    impl std::fmt::Display for Handler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl Handler {
        fn contains(&self, needle: &str) -> bool {
            self.0.contains(&squeeze(needle))
        }

        /// For prose rather than code: the daemon banner is a sentence whose
        /// capitalisation ("NOT a monitoring daemon") is a deliberate part of
        /// the wording and could reasonably be re-cased without weakening
        /// what the test is checking.
        fn contains_ignoring_case(&self, needle: &str) -> bool {
            self.0.to_lowercase().contains(&squeeze(&needle.to_lowercase()))
        }

        /// Where `needle` sits within this handler, for the assertions that
        /// are about ORDER rather than presence (a gate that runs after the
        /// thing it gates is not a gate).
        fn position(&self, needle: &str) -> Option<usize> {
            self.0.find(&squeeze(needle))
        }

        /// The stretch of this handler starting at `from` and ending just
        /// before `until` (or at the end of the handler when `until` is
        /// absent or never appears) -- for arms that hold a nested `match`,
        /// where "somewhere in this handler" is too weak an assertion.
        fn arm(&self, from: &str, until: Option<&str>) -> Handler {
            let start = self
                .0
                .find(&squeeze(from))
                .unwrap_or_else(|| panic!("{from} not found in this handler"));
            let end = until
                .and_then(|u| self.0[start..].find(&squeeze(u)))
                .map(|offset| start + offset)
                .unwrap_or(self.0.len());
            Handler(self.0[start..end].to_string())
        }
    }

    /// The seven source-scanning tests are only as good as this helper. If
    /// `find_handler` ever failed to find its arm's END, every one of them
    /// would keep passing while actually asserting against the rest of the
    /// file -- "the JSON branch exists SOMEWHERE below here", which is
    /// exactly the "adjacent, not the same" trap they were written to avoid.
    /// So the helper is tested against a fixture whose arms are known.
    #[test]
    fn find_handler_stops_at_the_next_arm_and_ignores_how_the_source_is_wrapped() {
        let dispatch = "
        Command::First { a } => {
            first_only();
        }
        Command::Second {
            command: Second::Deep,
        } => {
            second_only(
                &one,
                &two,
            );
        }
        Command::Third => {}
        ";

        let first = find_handler(dispatch, "Command::First { a }");
        assert!(first.contains("first_only()"));
        assert!(
            !first.contains("second_only"),
            "the arm must stop at the next Command::"
        );

        // The marker spans four lines in the source and one in the test, and
        // the asserted call was exploded across four lines by the formatter.
        // Both still match: that is the whole point of squeezing.
        let second = find_handler(dispatch, "Command::Second { command: Second::Deep }");
        assert!(second.contains("second_only(&one, &two)"));
        assert!(!second.contains("first_only"));

        // `arm` carves within a handler, and `None` means "to the end of it"
        // -- never past it.
        assert!(second.arm("second_only", None).contains("&two"));
        assert!(!second.arm("second_only", None).contains("Third"));
    }

    #[test]
    fn squeeze_drops_layout_without_merging_distinct_code() {
        assert_eq!(squeeze("f(\n    &a,\n    &b,\n)"), squeeze("f(&a, &b)"));
        assert_eq!(
            squeeze("Command::Fs {\n  command: X,\n}"),
            squeeze("Command::Fs { command: X }")
        );
        assert_ne!(squeeze("f(&a)"), squeeze("f(&b)"));
        assert_ne!(
            squeeze("println!(\"scrub started\")"),
            squeeze("println!(\"scrub cancelled\")")
        );
    }

    #[test]
    fn scrub_start_and_cancel_handlers_branch_on_cli_json_without_losing_their_human_text() {
        // ScrubCmd::Start/Cancel used to read `cli.json` never. Proves,
        // within EACH arm's own bounded text (not just "somewhere in the
        // file"), that an `if cli.json` branch reaches the JSON
        // builder AND the pre-existing human sentence is still printed in
        // the `else` -- the exact "adjacent, not the same" trap this
        // project's ledger warns about would be a test that only checked
        // one of these two facts.
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let block = find_handler(dispatch, "Command::Fs { command: FsCmd::Scrub { action } }");

        let start_arm = block.arm("ScrubCmd::Start { name, priority } =>", Some("ScrubCmd::Status"));
        assert!(
            start_arm.contains("if cli.json"),
            "ScrubCmd::Start never branches on cli.json"
        );
        assert!(
            start_arm.contains("scrub_start_report_json(&group, priority_name)"),
            "ScrubCmd::Start doesn't emit the JSON report"
        );
        // The no-`--priority` wording, unchanged: the flag is opt-in, so the
        // sentence an operator has always seen must still be what they get.
        assert!(
            start_arm.contains("println!(\"scrub started\")"),
            "ScrubCmd::Start dropped its human text"
        );
        // And the `--priority` half, which would otherwise be free to reach
        // only one of the two output surfaces.
        assert!(
            start_arm.contains("engine.scrub_start(name.as_deref(), speed)"),
            "ScrubCmd::Start doesn't forward --priority to the engine"
        );
        assert!(
            start_arm.contains("println!(\"scrub started (priority {p})\")"),
            "ScrubCmd::Start's human text never reports the profile it just set"
        );

        let cancel_arm = block.arm("ScrubCmd::Cancel { name } =>", None);
        assert!(
            cancel_arm.contains("if cli.json"),
            "ScrubCmd::Cancel never branches on cli.json"
        );
        assert!(
            cancel_arm.contains("scrub_action_report_json(&group, \"cancelled\")"),
            "ScrubCmd::Cancel doesn't emit the JSON report"
        );
        assert!(
            cancel_arm.contains("println!(\"scrub cancelled\")"),
            "ScrubCmd::Cancel dropped its human text"
        );
    }

    #[test]
    fn recompress_handler_branches_on_cli_json_without_losing_its_human_text() {
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let handler = find_handler(dispatch, "Command::Fs { command: FsCmd::Recompress");
        assert!(
            handler.contains("if cli.json"),
            "FsCmd::Recompress never branches on cli.json"
        );
        assert!(
            handler.contains("recompress_report_json(&confirm_name, &compression)"),
            "FsCmd::Recompress doesn't emit the JSON report"
        );
        assert!(
            handler.contains("println!(\"recompress started at {compression}\")"),
            "FsCmd::Recompress dropped its human text"
        );
    }

    #[test]
    fn snapshot_create_handler_branches_on_cli_json_without_losing_its_human_text() {
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let handler = find_handler(dispatch, "Command::Fs { command: FsCmd::Snapshot");
        assert!(
            handler.contains("if cli.json"),
            "SnapshotCmd::Create never branches on cli.json"
        );
        assert!(
            handler.contains("snapshot_create_report_json(&resolved_group, &name)"),
            "SnapshotCmd::Create doesn't emit the JSON report"
        );
        assert!(
            handler.contains("println!(\"snapshot `{name}` created\")"),
            "SnapshotCmd::Create dropped its human text"
        );
    }

    #[test]
    fn schedule_install_handler_branches_on_cli_json_without_losing_its_human_text() {
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let handler = find_handler(
            dispatch,
            "Command::Schedule { command: ScheduleCmd::Install { scrub_priority } }",
        );
        assert!(
            handler.contains("if cli.json"),
            "ScheduleCmd::Install never branches on cli.json"
        );
        assert!(
            handler.contains("schedule_install_report_json(installed_timers, pruned)"),
            "ScheduleCmd::Install doesn't emit the JSON report"
        );
        assert!(
            handler.contains("installed and enabled {installed_timers} timer unit(s)"),
            "ScheduleCmd::Install dropped its human text"
        );
        // Mutation-testing follow-up: `install_schedule_
        // units` only COLLECTS its warnings now -- this handler is the
        // only place left that can actually print them. `disable_and_
        // prune_unit`'s and the `unowned_lookalikes` doc comments both
        // point back here as "the caller ... is what prints it"; this
        // source-scan is what holds that promise, since the handler itself
        // hardcodes `/etc/systemd/system`/`SystemRunner`/`current_exe()`
        // and can't be driven directly by a unit test (see `install_
        // schedule_units`'s own doc comment on why it was pulled out).
        // Dropping the returned `warnings` on the floor here would be
        // strictly worse than the pre-fix `eprintln!`-from-inside-the-
        // function shape it replaced: an earlier fix already burned this project on
        // a channel that collects a report but never delivers it.
        assert!(
            handler.contains("for warning in &warnings") && handler.contains("eprintln!(\"{warning}\")"),
            "ScheduleCmd::Install must eprintln! every warning `install_schedule_units` returns"
        );
    }

    // ---- A failed `systemctl disable --now` during `schedule
    // install`'s orphan pruning must not abort the install ----
    //
    // These drive `install_schedule_units` directly (not `dispatch`, which
    // hardcodes `/etc/systemd/system` and `SystemRunner` -- see that
    // function's own doc comment) with a fake `CommandRunner` that can be
    // told to fail one specific command, same "fail on a substring match"
    // shape as `shr-orchestrate/tests/orchestrate.rs`'s `FailingRunner`,
    // trimmed to only what these two tests need.

    /// Records every command run through it; fails any command whose full
    /// `program arg1 arg2 ...` string CONTAINS `fail_trigger`, with an exit
    /// code (5, "no installation config") matching the real-guest repro
    /// -- a crash-truncated unit file that still carries the shr-rs
    /// ownership marker but has no `[Install]` section.
    #[derive(Default)]
    struct ScheduleInstallTestRunner {
        recorded: std::sync::Mutex<Vec<String>>,
        fail_trigger: Option<String>,
    }

    impl ScheduleInstallTestRunner {
        fn healthy() -> Self {
            Self::default()
        }
        fn failing(trigger: &str) -> Self {
            Self {
                recorded: std::sync::Mutex::new(Vec::new()),
                fail_trigger: Some(trigger.to_string()),
            }
        }
        fn recorded(&self) -> Vec<String> {
            self.recorded.lock().unwrap().clone()
        }
    }

    impl CommandRunner for ScheduleInstallTestRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
        ) -> std::result::Result<shr_exec::CommandOutput, shr_exec::ExecError> {
            let cmd = format!("{program} {}", args.join(" "));
            self.recorded.lock().unwrap().push(cmd.clone());
            if self.fail_trigger.as_deref().is_some_and(|t| cmd.contains(t)) {
                return Err(shr_exec::ExecError::NonZeroExit {
                    program: program.to_string(),
                    exit_code: 5,
                    stdout: String::new(),
                    stderr: "Unit file has no installation config".to_string(),
                });
            }
            Ok(shr_exec::CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    /// A group with one band, so `write_scrub_timer_units` actually writes
    /// a unit pair for it -- `expand_state_fixture`'s own `bands: vec![]`
    /// would make it a no-op group (see `write_scrub_timer_units`'s "a
    /// group with none has nothing meaningful to scrub" skip).
    fn scrub_state_fixture(group_name: &str) -> ArrayState {
        let mut state = expand_state_fixture("shr");
        state.name = group_name.to_string();
        state.bands = vec![shr_state::StateBand {
            index: 0,
            level: "raid5".to_string(),
            md_name: format!("md_{group_name}"),
            md_uuid: None,
            member_partitions: vec![],
            usable_bytes: 1,
            ..Default::default()
        }];
        state
    }

    #[test]
    fn install_schedule_units_prunes_orphan_and_still_installs_when_systemctl_disable_fails() {
        // Real-guest repro: a `systemctl disable --now` failing for an
        // orphaned unit (its own cited scenario -- "a run of this same
        // cleanup that failed partway through") used to `?`-propagate
        // straight out of the whole handler, aborting BEFORE this run's own
        // timers were ever written/enabled -- and since the offending file
        // was still on disk, every subsequent run failed identically,
        // permanently wedging `schedule install`. This drives the actual
        // production function `dispatch` now calls, not a reimplementation
        // of its logic.
        let dir = tempfile::tempdir().unwrap();
        let unit_dir = dir.path().join("units");
        let exe_path = dir.path().join("shr-rs");

        // Seed an orphaned scrub unit pair for a group `current_state`
        // below no longer has -- exactly what a destroyed/renamed group
        // leaves behind.
        let orphan_state = shr_state::StateFile::new(vec![scrub_state_fixture("gone")]);
        shr_state::conf::write_scrub_timer_units(&unit_dir, &orphan_state, &exe_path, None).unwrap();
        let (orphan_service, orphan_timer) = shr_state::conf::scrub_unit_paths(&unit_dir, "gone");
        assert!(orphan_service.exists() && orphan_timer.exists());

        let current_state = shr_state::StateFile::new(vec![scrub_state_fixture("shr1")]);
        let policy = shr_state::policy::PolicyFile::default();
        let runner = ScheduleInstallTestRunner::failing("disable --now shr-rs-scrub-gone.timer");

        let (installed, pruned, warnings) =
            install_schedule_units(&unit_dir, &current_state, &exe_path, &policy, &runner).unwrap();

        // The orphan is still deleted despite the disable failure -- pruning
        // one unit failing must not leave it behind to wedge the NEXT run.
        assert!(!orphan_service.exists(), "orphaned service must still be deleted");
        assert!(!orphan_timer.exists(), "orphaned timer must still be deleted");
        assert_eq!(pruned, 2, "both orphan files (service+timer) counted as pruned");

        // Mutation-testing follow-up: the failure must be
        // surfaced, not merely tolerated -- swallowing it silently would be
        // strictly worse than the older `?`-propagate, because nothing
        // would tell the operator a stale unit's disable ever failed.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("shr-rs-scrub-gone.timer") && w.contains("disable")),
            "a failed disable must be reported by name: {warnings:?}"
        );

        // The install itself must have proceeded PAST the prune failure:
        // shr1's own unit pair got written and its timer `enable --now`d,
        // and daemon-reload ran.
        let (shr1_service, shr1_timer) = shr_state::conf::scrub_unit_paths(&unit_dir, "shr1");
        assert!(
            shr1_service.exists() && shr1_timer.exists(),
            "this run's own unit must still be written"
        );
        // 3 timers every run always installs regardless of group count:
        // shr1's own scrub timer, plus the two global ones (throttle-tick,
        // health-check).
        assert_eq!(
            installed, 3,
            "this run's timers must all still be counted as installed"
        );

        let cmds = runner.recorded();
        assert!(
            cmds.iter()
                .any(|c| c == "systemctl disable --now shr-rs-scrub-gone.timer"),
            "the failing disable must actually have been attempted: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c == "systemctl daemon-reload"),
            "daemon-reload must still run: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c == "systemctl enable --now shr-rs-scrub-shr1.timer"),
            "this run's own scrub timer must still be enabled: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c == "systemctl enable --now shr-rs-throttle-tick.timer"),
            "the throttle-tick timer must still be enabled: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c == "systemctl enable --now shr-rs-health-check.timer"),
            "the health-check timer must still be enabled: {cmds:?}"
        );
    }

    #[test]
    fn install_schedule_units_prunes_orphan_normally_when_systemctl_disable_succeeds() {
        // Guards the normal (non-failure) path: the earlier fix must not
        // accidentally break the pruned count or skip a real disable call
        // when nothing is actually wrong.
        let dir = tempfile::tempdir().unwrap();
        let unit_dir = dir.path().join("units");
        let exe_path = dir.path().join("shr-rs");

        let orphan_state = shr_state::StateFile::new(vec![scrub_state_fixture("gone")]);
        shr_state::conf::write_scrub_timer_units(&unit_dir, &orphan_state, &exe_path, None).unwrap();
        let (orphan_service, orphan_timer) = shr_state::conf::scrub_unit_paths(&unit_dir, "gone");

        let current_state = shr_state::StateFile::new(vec![scrub_state_fixture("shr1")]);
        let policy = shr_state::policy::PolicyFile::default();
        let runner = ScheduleInstallTestRunner::healthy();

        let (installed, pruned, warnings) =
            install_schedule_units(&unit_dir, &current_state, &exe_path, &policy, &runner).unwrap();

        assert!(
            !orphan_service.exists() && !orphan_timer.exists(),
            "orphan must be pruned"
        );
        assert_eq!(pruned, 2);
        assert_eq!(
            installed, 3,
            "shr1's scrub timer plus the two global timers (throttle-tick, health-check)"
        );
        let (shr1_service, shr1_timer) = shr_state::conf::scrub_unit_paths(&unit_dir, "shr1");
        assert!(shr1_service.exists() && shr1_timer.exists());

        let cmds = runner.recorded();
        assert!(
            cmds.iter()
                .any(|c| c == "systemctl disable --now shr-rs-scrub-gone.timer"),
            "the (successful) disable must actually have been attempted: {cmds:?}"
        );

        // E-gap: the discriminator's other half -- a function that always
        // warns would still pass the failure-side assertion above. A fully
        // healthy run (no failed disable, no unowned lookalike) must
        // produce NO warnings at all.
        assert!(
            warnings.is_empty(),
            "a healthy run must not warn about anything: {warnings:?}"
        );
    }

    #[test]
    fn install_schedule_units_prunes_disabled_snapshot_timer_even_when_disable_fails() {
        // Named TWO sites: the orphan-scrub-unit prune covered
        // above, and this one -- the `else if` branch that removes
        // `shr-rs-snapshot-auto.*` once `[snapshot].enabled` flips back to
        // `false`. Both route through the same `disable_and_prune_unit`
        // helper, but this proves the second call site specifically, not
        // just the first.
        let dir = tempfile::tempdir().unwrap();
        let unit_dir = dir.path().join("units");
        let exe_path = dir.path().join("shr-rs");

        // Seed a previously-installed, still-enabled snapshot timer, as a
        // prior `schedule install` run with `[snapshot].enabled = true`
        // would have left it.
        shr_state::conf::write_snapshot_timer_unit(&unit_dir, &exe_path, "daily").unwrap();
        let snapshot_timer = unit_dir.join("shr-rs-snapshot-auto.timer");
        let snapshot_service = unit_dir.join("shr-rs-snapshot-auto.service");
        assert!(snapshot_timer.exists() && snapshot_service.exists());

        let current_state = shr_state::StateFile::new(vec![scrub_state_fixture("shr1")]);
        let mut policy = shr_state::policy::PolicyFile::default();
        policy.snapshot.enabled = false; // operator turned it back off
        let runner = ScheduleInstallTestRunner::failing("disable --now shr-rs-snapshot-auto.timer");

        let (installed, pruned, warnings) =
            install_schedule_units(&unit_dir, &current_state, &exe_path, &policy, &runner).unwrap();

        assert!(
            !snapshot_timer.exists() && !snapshot_service.exists(),
            "the stale snapshot unit must still be pruned despite the failed disable"
        );
        assert_eq!(pruned, 2);
        assert_eq!(
            installed, 3,
            "shr1's own timer plus the two global timers must still be installed"
        );
        let (shr1_service, shr1_timer) = shr_state::conf::scrub_unit_paths(&unit_dir, "shr1");
        assert!(shr1_service.exists() && shr1_timer.exists());

        // Same discriminator as the scrub-unit prune site above, proven at
        // this second call site.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("shr-rs-snapshot-auto.timer") && w.contains("disable")),
            "a failed disable at the snapshot prune site must also be reported: {warnings:?}"
        );
    }

    #[test]
    fn install_schedule_units_warns_about_an_unowned_lookalike_and_leaves_it_on_disk() {
        // The other of the two pre-existing `eprintln!` call sites (the
        // `unowned_lookalikes` loop) has the identical untested-visibility
        // problem the `disable_and_prune_unit` failure had: an operator's
        // own hand-written unit that merely LOOKS like an orphaned shr-rs
        // scrub timer (same naming convention, no ownership marker) must be
        // reported and left alone, never silently ignored or deleted.
        let dir = tempfile::tempdir().unwrap();
        let unit_dir = dir.path().join("units");
        let exe_path = dir.path().join("shr-rs");
        std::fs::create_dir_all(&unit_dir).unwrap();

        // No shr-rs ownership marker at the top -- exactly what makes this
        // a lookalike rather than something `disable_and_prune_unit` would
        // ever touch.
        let lookalike = unit_dir.join("shr-rs-scrub-handwritten.timer");
        std::fs::write(
            &lookalike,
            "[Unit]\nDescription=hand-written\n[Timer]\nOnCalendar=daily\n[Install]\nWantedBy=timers.target\n",
        )
        .unwrap();

        let current_state = shr_state::StateFile::new(vec![scrub_state_fixture("shr1")]);
        let policy = shr_state::policy::PolicyFile::default();
        let runner = ScheduleInstallTestRunner::healthy();

        let (_installed, pruned, warnings) =
            install_schedule_units(&unit_dir, &current_state, &exe_path, &policy, &runner).unwrap();

        assert!(lookalike.exists(), "an unowned lookalike must never be deleted");
        assert_eq!(pruned, 0, "an unowned lookalike must not be counted as pruned");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("shr-rs-scrub-handwritten.timer") && w.contains("hand-written")),
            "the unowned lookalike must be reported by name: {warnings:?}"
        );
    }

    #[test]
    fn daemon_handler_emits_ndjson_under_cli_json_and_keeps_its_tick_text_otherwise() {
        let src = include_str!("lib.rs");
        let dispatch = src
            .split("mod tests")
            .next()
            .expect("source before the test module");
        let handler = find_handler(dispatch, "Command::Daemon { state_path }");
        // Startup banner gated -- must never taint an NDJSON stream (see
        // `daemon_tick_report_json`'s doc comment).
        assert!(
            handler.contains("if !cli.json {"),
            "Daemon's startup banner must be suppressed under --json, not print into the NDJSON stream"
        );
        assert!(
            handler.contains("daemon_tick_report_json(&state.groups)"),
            "Daemon's non-empty tick doesn't emit the NDJSON report"
        );
        assert!(
            handler.contains("daemon_tick_report_json(&[])"),
            "Daemon's empty/no-active-array tick doesn't emit the NDJSON report"
        );
        assert!(
            handler.contains("[status] group {}: mode: {}, layout {}, disks: {}"),
            "Daemon dropped its per-group human text"
        );
        assert!(
            handler.contains("[status] No storage group configured yet."),
            "Daemon dropped its no-active-array human text"
        );
    }

    #[test]
    fn merge_planned_commands_adds_the_array_in_call_order_without_losing_state_fields() {
        let state = json!({"mode": "shr", "layout_version": 1});
        let commands = vec![
            "parted /dev/sdb mkpart primary 1MiB 100%".to_string(),
            "mdadm --create /dev/md0".to_string(),
        ];

        let merged = merge_planned_commands(&state, &commands).unwrap();

        assert_eq!(merged["mode"], "shr");
        assert_eq!(merged["layout_version"], 1);
        assert_eq!(
            merged["planned_commands"],
            json!([
                "parted /dev/sdb mkpart primary 1MiB 100%",
                "mdadm --create /dev/md0",
            ])
        );
    }

    #[test]
    fn merge_planned_commands_with_no_commands_yields_an_empty_array_not_a_missing_field() {
        let state = json!({"mode": "shr"});
        let merged = merge_planned_commands(&state, &[]).unwrap();
        assert_eq!(merged["planned_commands"], json!([]));
    }

    #[test]
    fn render_planned_commands_text_lists_each_command_in_order_on_its_own_line() {
        let commands = vec!["parted ...".to_string(), "mdadm --create ...".to_string()];
        let text = render_planned_commands_text(&commands);
        let parted_pos = text.find("parted ...").unwrap();
        let mdadm_pos = text.find("mdadm --create ...").unwrap();
        assert!(parted_pos < mdadm_pos, "commands must appear in execution order");
    }

    #[test]
    fn require_typed_confirmation_auto_approves_with_yes_without_touching_stdin() {
        // `yes` short-circuits before `interactive` is even consulted --
        // pass `interactive: true` with an EMPTY reader (which would fail
        // to read a matching line) to prove `--yes` really does bypass it.
        let mut input: &[u8] = b"";
        require_typed_confirmation("shr1", &[], true, "create", true, &mut input).unwrap();
    }

    #[test]
    fn require_typed_confirmation_auto_approves_when_not_an_interactive_terminal() {
        // The real non-interactive path a script or Cockpit's spawned
        // process takes -- `interactive: false` must bypass the prompt
        // (and therefore never touch `input`) regardless of what it
        // contains.
        let mut input: &[u8] = b"";
        require_typed_confirmation("shr1", &[], false, "create", false, &mut input).unwrap();
    }

    #[test]
    fn require_typed_confirmation_rejects_a_mismatched_typed_name_in_an_interactive_terminal() {
        // The core safety requirement: TTY + no --yes + wrong typed name
        // must return an error BEFORE the caller ever reaches
        // `acquire_state_lock`/`OrchestrationEngine::new` -- both call sites
        // place this behind a bare `?`, so an `Err` here is, by
        // construction, a guarantee zero destructive commands run (see this
        // function's own doc comment).
        let mut input: &[u8] = b"definitely-not-the-group-name\n";
        let err = require_typed_confirmation("shr1", &[], false, "create", true, &mut input).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn require_typed_confirmation_rejects_empty_input_in_an_interactive_terminal() {
        let mut input: &[u8] = b"\n";
        let err = require_typed_confirmation("shr1", &[], false, "create", true, &mut input).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn require_typed_confirmation_proceeds_when_the_typed_name_matches_exactly() {
        let mut input: &[u8] = b"shr1\n";
        require_typed_confirmation("shr1", &[], false, "create", true, &mut input).unwrap();
    }

    // -- `destroy`'s superblock decision.

    #[test]
    fn zero_superblocks_flags_are_taken_at_face_value_without_touching_stdin() {
        // Either flag short-circuits before the prompt -- an EMPTY reader
        // proves stdin is never consulted.
        let mut empty: &[u8] = b"";
        assert!(resolve_zero_superblocks(true, false, false, true, &mut empty).unwrap());
        let mut empty: &[u8] = b"";
        assert!(!resolve_zero_superblocks(false, true, false, true, &mut empty).unwrap());
    }

    #[test]
    fn destroy_refuses_to_pick_a_superblock_default_for_a_script() {
        // The whole point of the change: `--yes` with neither flag used to
        // inherit "leave the markers", a real decision nobody made. It must
        // now fail, and name both flags so the fix is obvious.
        let mut empty: &[u8] = b"";
        let err = resolve_zero_superblocks(false, false, true, true, &mut empty).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--zero-superblocks"), "{msg}");
        assert!(msg.contains("--no-zero-superblocks"), "{msg}");

        // Same when there is simply no terminal to ask on (Cockpit's
        // spawned process, a cron job), independently of `--yes`.
        let mut empty: &[u8] = b"";
        let err = resolve_zero_superblocks(false, false, false, false, &mut empty).unwrap_err();
        assert!(err.to_string().contains("explicit decision"), "{err}");
    }

    #[test]
    fn destroy_asks_the_operator_when_neither_flag_is_given_in_a_terminal() {
        let mut yes_input: &[u8] = b"yes\n";
        assert!(resolve_zero_superblocks(false, false, false, true, &mut yes_input).unwrap());

        let mut no_input: &[u8] = b"no\n";
        assert!(!resolve_zero_superblocks(false, false, false, true, &mut no_input).unwrap());

        // Short forms, and a trailing CRLF, are accepted the same way.
        let mut short: &[u8] = b"n\r\n";
        assert!(!resolve_zero_superblocks(false, false, false, true, &mut short).unwrap());
    }

    #[test]
    fn destroy_cancels_rather_than_guessing_at_an_unrecognized_answer() {
        // Anything that is not clearly yes or no must NOT fall through to a
        // default -- that would be the original bug wearing a prompt.
        let mut vague: &[u8] = b"maybe\n";
        let err = resolve_zero_superblocks(false, false, false, true, &mut vague).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        let mut empty_line: &[u8] = b"\n";
        let err = resolve_zero_superblocks(false, false, false, true, &mut empty_line).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn destroy_rejects_both_superblock_flags_at_once() {
        // clap's `conflicts_with` catches this first; the helper refuses it
        // too rather than silently letting one win.
        let mut empty: &[u8] = b"";
        assert!(resolve_zero_superblocks(true, true, false, true, &mut empty).is_err());
    }

    #[test]
    fn destroy_flags_parse_and_are_mutually_exclusive() {
        let cli = Cli::try_parse_from(["shr-rs", "destroy", "--name", "shr1", "--zero-superblocks"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Destroy {
                zero_superblocks: true,
                no_zero_superblocks: false,
                ..
            }
        ));

        let cli =
            Cli::try_parse_from(["shr-rs", "destroy", "--name", "shr1", "--no-zero-superblocks"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Destroy {
                zero_superblocks: false,
                no_zero_superblocks: true,
                ..
            }
        ));

        assert!(Cli::try_parse_from([
            "shr-rs",
            "destroy",
            "--name",
            "shr1",
            "--zero-superblocks",
            "--no-zero-superblocks",
        ])
        .is_err());
    }

    #[test]
    fn require_typed_confirmation_treats_a_trailing_crlf_as_insignificant_but_not_inner_whitespace() {
        // Only the line terminator is stripped -- a leading/trailing space
        // the operator actually typed must still fail to match, the same
        // exact-match semantics `shr-tui`'s `wizard::can_execute` uses.
        let mut input: &[u8] = b"shr1\r\n";
        require_typed_confirmation("shr1", &[], false, "create", true, &mut input).unwrap();

        let mut padded: &[u8] = b" shr1\n";
        let err = require_typed_confirmation("shr1", &[], false, "create", true, &mut padded).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    // -- `status --detail`/`--watch`, `fs df` CLI wiring.

    fn resolved_disk(id: &str, kernel: &str, size: u64) -> ResolvedDisk {
        ResolvedDisk {
            reference: DiskRef::Path(kernel.to_string()),
            kernel_name: kernel.to_string(),
            id: id.into(),
            size_bytes: size,
            serial: format!("SN-{kernel}"),
            model: "Test Disk".to_string(),
            system_mounts: vec![],
            has_content: false,
        }
    }

    #[test]
    fn status_detail_and_watch_flags_parse_with_their_defaults() {
        let cli = Cli::try_parse_from(["shr-rs", "status"]).unwrap();
        match cli.command {
            Command::Status {
                detail,
                watch,
                interval_secs,
            } => {
                assert!(!detail);
                assert!(!watch);
                assert_eq!(interval_secs, 2);
            }
            _ => panic!("expected Command::Status"),
        }

        let cli = Cli::try_parse_from(["shr-rs", "status", "--detail"]).unwrap();
        match cli.command {
            Command::Status { detail, watch, .. } => {
                assert!(detail);
                assert!(!watch);
            }
            _ => panic!("expected Command::Status"),
        }

        let cli = Cli::try_parse_from(["shr-rs", "status", "--watch", "--interval-secs", "5"]).unwrap();
        match cli.command {
            Command::Status {
                watch, interval_secs, ..
            } => {
                assert!(watch);
                assert_eq!(interval_secs, 5);
            }
            _ => panic!("expected Command::Status"),
        }
    }

    #[test]
    fn fs_df_subcommand_parses_alone_and_combined_with_global_json() {
        let cli = Cli::try_parse_from(["shr-rs", "fs", "df"]).unwrap();
        assert!(!cli.json);
        assert!(matches!(cli.command, Command::Fs { command: FsCmd::Df }));

        let cli = Cli::try_parse_from(["shr-rs", "--json", "fs", "df"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Fs { command: FsCmd::Df }));
    }

    /// `disk list` must exist and parse -- it was specified from the start
    /// but went unimplemented for a long time.
    /// `dispatch` itself needs a real host (lsblk/smartctl/state.toml under
    /// `/var/lib/shr-rs`) this Windows dev host doesn't have, so -- same as
    /// every other CLI-surface regression test in this file (see
    /// `status_detail_and_watch_flags_parse_with_their_defaults`,
    /// `fs_df_subcommand_parses_alone_and_combined_with_global_json`) --
    /// this proves the command actually reaches `DiskCmd::List` through
    /// `clap`, combined with the global `--json` flag.
    #[test]
    fn disk_list_subcommand_parses_alone_and_combined_with_global_json() {
        let cli = Cli::try_parse_from(["shr-rs", "disk", "list"]).unwrap();
        assert!(!cli.json);
        assert!(matches!(
            cli.command,
            Command::Disk {
                command: DiskCmd::List
            }
        ));

        let cli = Cli::try_parse_from(["shr-rs", "--json", "disk", "list"]).unwrap();
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Command::Disk {
                command: DiskCmd::List
            }
        ));
    }

    /// `internal snapshot-auto-tick` (the `shr-rs-snapshot-auto.timer`
    /// entrypoint) must parse -- same "prove it reaches the right `Command`
    /// variant via clap" shape as every other CLI-surface regression test
    /// in this file; `dispatch` itself needs a real host `/var/lib/shr-rs`/
    /// `policy.toml` this Windows dev host doesn't have.
    #[test]
    fn internal_snapshot_auto_tick_subcommand_parses() {
        let cli = Cli::try_parse_from(["shr-rs", "internal", "snapshot-auto-tick"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Internal {
                command: InternalCmd::SnapshotAutoRun
            }
        ));
    }

    #[test]
    fn status_watch_with_json_is_rejected_before_touching_the_system() {
        // No `--json` streaming contract is defined for `--watch` --
        // this must bail immediately, before `run_status_watch` ever calls
        // `crossterm::terminal::size()` or touches state.toml/the inspector,
        // which is what makes this reachable as a plain unit test on a
        // Windows dev host with no real `/var/lib/shr-rs` or `lsblk`.
        let cli = Cli {
            json: true,
            command: Command::Status {
                detail: false,
                watch: true,
                interval_secs: 2,
            },
        };
        let err = dispatch(cli).unwrap_err();
        assert!(err.to_string().contains("--json"), "{err}");
        assert!(err.to_string().contains("--watch"), "{err}");
    }

    #[test]
    fn status_watch_without_json_rejects_a_non_terminal_stdout_instead_of_looping_forever() {
        // A `cargo test` runner's stdout is never a real terminal, so
        // `run_status_watch`'s `is_terminal()` gate deterministically fires
        // here -- proving the "never fill a pipe with an infinite loop"
        // decision this wave had to make (brief) without needing a real
        // TTY, and without ever reaching `crossterm::terminal::size()`,
        // `StateStore`, or the inspector.
        let cli = Cli {
            json: false,
            command: Command::Status {
                detail: false,
                watch: true,
                interval_secs: 2,
            },
        };
        let err = dispatch(cli).unwrap_err();
        assert!(err.to_string().contains("real terminal"), "{err}");
    }

    #[test]
    fn watch_frame_meta_reserves_one_row_so_the_frames_trailing_newline_cannot_scroll() {
        // See `run_status_watch`'s doc comment: drawing into every row would
        // scroll the terminal by one line on the frame's own trailing
        // newline, permanently desyncing the "move cursor up N" redraw math.
        let meta = watch_frame_meta_from(100, 24);
        assert_eq!(meta.width, 100);
        assert_eq!(meta.max_height, 23);
    }

    #[test]
    fn watch_frame_meta_never_reports_a_zero_height_even_at_a_one_row_terminal() {
        let meta = watch_frame_meta_from(80, 1);
        assert_eq!(meta.max_height, 1);
    }

    #[test]
    fn dry_run_layout_diagram_text_renders_bands_for_the_exact_disks_passed_in() {
        let disks = vec![
            resolved_disk("ata-DISK1", "sdb", 4_000_000_000_000),
            resolved_disk("ata-DISK2", "sdc", 4_000_000_000_000),
            resolved_disk("ata-DISK3", "sdd", 4_000_000_000_000),
        ];
        let text = dry_run_layout_diagram_text(&disks, RedundancyMode::Shr).unwrap();
        assert!(text.contains("Layout diagram (DRY RUN)"), "{text}");
        assert!(text.contains("band0"), "{text}");
    }

    fn expand_state_fixture(mode: &str) -> ArrayState {
        ArrayState {
            name: "shr1".to_string(),
            mode: mode.to_string(),
            created_at: "2026-07-26T00:00:00Z".to_string(),
            layout_version: 1,
            disks: vec![
                shr_state::StateDisk {
                    id: "ata-DISK1".to_string(),
                    size_bytes: 4_000_000_000_000,
                    serial: None,
                    model: None,
                    added_at: "2026-07-26T00:00:00Z".to_string(),
                    partitions: vec![],
                },
                shr_state::StateDisk {
                    id: "ata-DISK2".to_string(),
                    size_bytes: 4_000_000_000_000,
                    serial: None,
                    model: None,
                    added_at: "2026-07-26T00:00:00Z".to_string(),
                    partitions: vec![],
                },
            ],
            bands: vec![],
            filesystem: shr_state::StateFilesystem {
                fs_uuid: None,
                mount_point: "/mnt/shr_data".to_string(),
                vg_name: "shr_vg".to_string(),
                lv_name: "data".to_string(),
                compression: "zstd:3".to_string(),
            },
            expansion: shr_state::StateExpansion::default(),
        }
    }

    #[test]
    fn expand_dry_run_layout_diagram_text_covers_every_disk_state_already_has() {
        let state = expand_state_fixture("shr");
        let text = expand_dry_run_layout_diagram_text(&state).unwrap();
        assert!(text.contains("Layout diagram (DRY RUN)"), "{text}");
        assert!(text.contains("band0"), "{text}");
    }

    #[test]
    fn expand_dry_run_layout_diagram_text_rejects_an_unknown_mode_string_instead_of_guessing() {
        let state = expand_state_fixture("bogus");
        let err = expand_dry_run_layout_diagram_text(&state).unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn fs_usage_map_never_fabricates_a_figure_when_the_underlying_command_fails() {
        // `SystemRunner` really spawns `btrfs`/`df` -- on this project's own
        // Windows dev host (and any host without a mounted Btrfs group at
        // this path) that spawn/exit fails, which must surface as an
        // honest "don't know", not a panic or a guessed number.
        let groups = vec![shr_command::GroupStatus {
            name: "default".to_string(),
            mode: "shr".to_string(),
            layout_version: 1,
            mount_point: "/mnt/shr_data".to_string(),
            fs_uuid: None,
            vg_name: "shr_vg".to_string(),
            lv_name: "data".to_string(),
            compression: "zstd:3".to_string(),
            usable_bytes: 8_000_000_000_000,
            resize_pending: false,
            disks: vec![],
            bands: vec![],
        }];

        let runner = SystemRunner::new();
        let usage = fs_usage_map(&runner, &groups);
        let df = build_fs_df(&groups, &usage);
        assert_eq!(df.groups.len(), 1);
        assert_eq!(df.groups[0].usable_bytes, 8_000_000_000_000);
        assert_eq!(df.groups[0].data_used_bytes, None);
        assert_eq!(df.groups[0].unallocated_bytes, None);
        assert_eq!(df.groups[0].statvfs_avail_bytes, None);

        let rendered = render::render_fs_df(&df);
        assert!(rendered.contains("default"), "{rendered}");
        assert!(rendered.contains('?'), "{rendered}");
    }

    fn empty_status_report() -> shr_command::StatusReport {
        shr_command::StatusReport {
            schema_version: shr_command::report::SCHEMA_VERSION,
            health: shr_command::Health::Unknown,
            disks: vec![],
            arrays: vec![],
            groups: vec![],
            state_path: None,
        }
    }

    /// `status --json` must report the real `state.toml` path this
    /// invocation resolved, not a placeholder. `dispatch`'s own
    /// `SystemInspector`-backed `build_status` calls can't run in a unit
    /// test on this non-Linux dev host, so the actual defect -- stamping the
    /// resolved path onto an already-built report -- is pulled out into
    /// `attach_state_path`, a pure seam that can be tested without touching
    /// a filesystem at all.
    #[test]
    fn attach_state_path_stamps_the_real_cli_constant_not_a_placeholder() {
        let report = attach_state_path(empty_status_report());
        assert_eq!(report.state_path.as_deref(), Some(STATE_PATH));
        // Guards against the exact "adjacent but not the same" defect class
        // this project's ledger tracks: the stamped value must be the one
        // constant every `StateStore::new(STATE_PATH)` call in this file
        // actually loads from, not a second, independently-typed copy of
        // the same string that could silently drift from it.
        assert_eq!(STATE_PATH, "/var/lib/shr-rs/state.toml");
    }

    #[test]
    fn attach_state_path_never_touches_the_rest_of_the_report() {
        let mut report = empty_status_report();
        report.health = shr_command::Health::Degraded;
        let stamped = attach_state_path(report);
        assert_eq!(stamped.health, shr_command::Health::Degraded);
    }
}
