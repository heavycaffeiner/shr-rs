//! Idempotent writers for the two system config files that let the OS's own
//! boot machinery (mdadm's initramfs assembly, systemd's fstab mount) bring
//! shr-rs's arrays back up after a reboot, with no shr-rs process needed at
//! boot time (D8). Both writers regenerate their managed block from the
//! CURRENT `StateFile` -- i.e. EVERY group, not just whichever one was just
//! touched -- in full on every call, and splice it back into whatever else
//! already exists in the file, so a stale entry from a removed band never
//! lingers and content the operator or another tool owns is never touched.
//!
//! Multi-group correctness trap (Phase 4 multi-group support): these used
//! to take a single `ArrayState`. If they still did, creating group B would
//! regenerate the managed block from ONLY group B's bands/filesystem --
//! silently deleting group A's `ARRAY` line and fstab mount. After a
//! reboot, group A simply would not come back, with no error anywhere to
//! point at. Taking the whole `StateFile` and flattening across every
//! group is what prevents that.

use crate::error::StateError;
use crate::schema::StateFile;
use std::path::{Path, PathBuf};

const BEGIN_MARKER: &str = "# >>> shr-rs managed >>>";
const END_MARKER: &str = "# <<< shr-rs managed <<<";

/// Ownership marker embedded as a leading comment line in every
/// systemd unit file this project generates -- deliberately the SAME
/// marker text `write_mdadm_conf`/`write_fstab` already splice around
/// their managed block (`BEGIN_MARKER`), not a second concept: "how do we
/// know shr-rs owns this" already has an answer in this file, unit files
/// just apply it as a single leading line instead of a splice region
/// (they're written whole, never spliced -- see each writer's own doc
/// comment). systemd ignores `#`-prefixed lines anywhere in a unit file,
/// including before the first section header, so this is invisible to
/// systemd itself and only meaningful to shr-rs's own cleanup code
/// (`is_shr_rs_owned_unit`) -- real-guest evidence showed a
/// `destroy()`d group's scrub timer left enabled forever with nothing able
/// to tell it apart from an operator's own hand-written unit of the same
/// name, which is exactly what this marker exists to make possible.
const UNIT_OWNERSHIP_MARKER: &str = BEGIN_MARKER;

/// Write (or idempotently replace) shr-rs's `ARRAY ... UUID=` lines in
/// `/etc/mdadm.conf`, one per band across EVERY group. Bands without a real
/// `md_uuid` yet (best-effort reads can fail without aborting an
/// already-committed step -- see `shr-orchestrate`'s an earlier review finding
/// F1) are skipped rather than emitting a line mdadm couldn't use to
/// reassemble anything.
pub fn write_mdadm_conf(path: &Path, state: &StateFile) -> Result<(), StateError> {
    let mut lines: Vec<String> = state
        .groups
        .iter()
        .flat_map(|group| group.bands.iter())
        .filter_map(|band| {
            band.md_uuid
                .as_ref()
                .map(|uuid| format!("ARRAY /dev/{} UUID={}", band.md_name, uuid))
        })
        .collect();

    // Destroyed arrays whose superblocks are still on the disks. mdadm.conf(5):
    // the device name `<ignore>` (angle brackets included) means "any array
    // which matches the rest of the line will never be automatically
    // assembled". Without this, the kernel's incremental (udev) assembly
    // finds those leftover members at the next boot and resurrects the dead
    // array -- observed on a real guest, where a destroyed group came back as
    // `/dev/md6`, claimed a device number, and appeared in `shr-rs status`
    // belonging to no group at all.
    //
    // Emitted INSIDE the same managed block as the live arrays, on purpose:
    // these lines are regenerated from `StateFile` exactly like the others,
    // so an entry pruned from state disappears from the file on the next
    // write instead of lingering as something nothing owns.
    if !state.retired_arrays.is_empty() {
        lines.push(
            "# destroyed by shr-rs; superblocks deliberately left on the disks for manual \
             recovery -- never auto-assemble these"
                .to_string(),
        );
        lines.extend(
            state
                .retired_arrays
                .iter()
                .map(|r| format!("ARRAY <ignore> UUID={} # was `{}`", r.md_uuid, r.group_name)),
        );
    }

    write_managed_block(path, &lines.join("\n"))
}

/// Write (or idempotently replace) shr-rs's mount lines in `/etc/fstab`, one
/// per group, each keyed by that group's Btrfs filesystem UUID -- never a
/// `/dev/sdX`-style kernel path (the design: kernel device names are
/// not stable across reboots or disk reordering). A group whose filesystem
/// UUID isn't known yet (e.g. its `create()` hasn't finished `mkfs`) is
/// skipped, same reasoning as `write_mdadm_conf` skipping bands without a
/// UUID. If NO group has a known UUID yet AND the file doesn't already
/// exist, this is a no-op (doesn't create the file) -- there is nothing
/// valid to write yet. But if the file already exists, an empty
/// `lines` is written anyway, EMPTYING the managed block: an empty state
/// here doesn't only mean "fresh system, nothing yet" -- it's also what
/// `destroy()`'ing the LAST group looks like, and in that case a stale
/// mount line for a filesystem that no longer exists must not survive in
/// the file (real-guest repro: `findmnt --verify` reported it unreachable
/// on every subsequent boot). `write_mdadm_conf` has no equivalent guard at
/// all and already gets this right unconditionally; this mirrors that for
/// the one case (pre-create) where mdadm.conf has nothing analogous to
/// preserve.
///
/// `nofail,x-systemd.device-timeout=10`: an earlier review finding -- without
/// `nofail`, a boot where this array can't be found or assembled in time
/// (a genuinely stale entry left behind by a manually-destroyed array, a
/// disk failure, anything) makes systemd treat mounting it as required for
/// `local-fs.target`, which can hang the ENTIRE boot waiting on it instead
/// of just failing to bring up this one filesystem.
///
/// `subvol=@`: every group's Btrfs filesystem is now created
/// with an `@` (data) / `@snapshots` layout from the start, and `@` is the
/// one that's actually mounted for real use -- unconditional, not
/// conditional on some layout-version field, because fixes this
/// project's v1 baseline as "always `@`/`@snapshots`"; there is no
/// pre-`@` layout left to support (every array built with the old
/// single-root layout was a development artifact, deleted before this
/// decision was made -- see the design's notes).
pub fn write_fstab(path: &Path, state: &StateFile) -> Result<(), StateError> {
    let lines: Vec<String> = state
        .groups
        .iter()
        .filter_map(|group| {
            group.filesystem.fs_uuid.as_ref().map(|uuid| {
                format!(
                    "UUID={} {} btrfs compress={},subvol=@,nofail,x-systemd.device-timeout=10 0 0",
                    uuid, group.filesystem.mount_point, group.filesystem.compression
                )
            })
        })
        .collect();
    // Only skip creating the file when there's truly nothing to
    // reconcile against -- see the doc comment above for why `lines.is_empty()`
    // alone can't distinguish "pre-create" from "post-destroy".
    if lines.is_empty() && !path.exists() {
        return Ok(());
    }
    write_managed_block(path, &lines.join("\n"))
}

/// Splice `block_content` between shr-rs's marker comments inside the file
/// at `path` (creating it if absent), replacing a prior managed block if one
/// exists rather than appending a duplicate, and leaving everything else in
/// the file untouched.
fn write_managed_block(path: &Path, block_content: &str) -> Result<(), StateError> {
    // An earlier review finding: a bare `.unwrap_or_default()` here treated
    // EVERY read failure -- including a pre-existing file containing valid
    // content that just isn't valid UTF-8 (a Latin-1 comment is enough) --
    // as "the file is empty", so the very next write would replace the
    // whole file with only shr-rs's own managed block. Only a genuinely
    // absent file means "nothing here yet"; every other error must
    // propagate rather than risk destroying content shr-rs doesn't own.
    let existing = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(StateError::Io(e)),
    };
    let new_content = splice_managed_block(&existing, block_content, path)?;
    // mdadm.conf/fstab must stay world-readable (blkid, systemd, and other
    // non-root tooling read them) -- unlike state.toml, no restrictive mode.
    crate::atomic_write(path, new_content.as_bytes(), None)
}

/// Shr-rs generates systemd timer units for scrub scheduling rather
/// than running its own daemon scheduler -- if a self-run scheduler process
/// dies, every scheduled scrub silently dies with it; a systemd timer
/// survives a reboot and a crashed prior run alike, and its logs land in
/// `journalctl` alongside everything else instead of a bespoke log file.
///
/// One `.service`/`.timer` PAIR PER GROUP, each its OWN file named after
/// that group (`shr-rs-scrub-<group>.service`/`.timer`) -- NOT one shared
/// file listing every group's schedule. This is the structural fix for the
/// exact mistake `write_mdadm_conf`/`write_fstab` had to correct (Phase 4):
/// a shared file regenerated from only the group just touched would delete
/// every OTHER group's entry. Giving each group a dedicated file makes that
/// class of bug impossible here -- writing group B's units never even reads
/// group A's, let alone overwrites it. Returns every path written, across
/// every group with at least one band (a group with none has nothing
/// meaningful to scrub).
/// `exe_path`: the generated unit's `ExecStart=` uses THIS path,
/// never a hardcoded `/usr/bin/shr-rs` or `/usr/local/bin/shr-rs` -- this
/// project has seen both directions of that mistake go wrong (a
/// `/usr/bin`-only install's timer pointing at a nonexistent binary;
/// separately, sudo's `secure_path` excluding `/usr/local/bin` so Cockpit
/// ran a stale `/usr/bin/shr-rs`). Neither install location is more
/// "correct" than the other, so this function stays pure and takes
/// whatever path the caller resolved (`std::env::current_exe()`, which
/// knows where THIS running binary actually lives) rather than guessing.
pub fn write_scrub_timer_units(
    dir: &Path,
    state: &StateFile,
    exe_path: &Path,
) -> Result<Vec<std::path::PathBuf>, StateError> {
    let exe = exe_path.display();
    let mut written = Vec::new();
    for group in &state.groups {
        if group.bands.is_empty() {
            continue;
        }
        let (service_path, timer_path) = scrub_unit_paths(dir, &group.name);

        let service = format!(
            "{UNIT_OWNERSHIP_MARKER}\n\
             [Unit]\n\
             Description=shr-rs scheduled scrub for group {name}\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={exe} fs scrub start --name {name}\n",
            name = group.name
        );
        let timer = format!(
            "{UNIT_OWNERSHIP_MARKER}\n\
             [Unit]\n\
             Description=shr-rs scheduled scrub timer for group {name}\n\
             \n\
             [Timer]\n\
             OnCalendar=weekly\n\
             Persistent=true\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            name = group.name
        );

        // Unit files are wholly owned by shr-rs (unlike mdadm.conf/fstab,
        // which an operator may hand-edit around a managed block) -- a
        // plain idempotent overwrite is enough, no marker-splice needed.
        crate::atomic_write(&service_path, service.as_bytes(), None)?;
        crate::atomic_write(&timer_path, timer.as_bytes(), None)?;
        written.push(service_path);
        written.push(timer_path);
    }
    Ok(written)
}

/// `shr-rs-scrub-<group>` unit stem, shared by `write_scrub_timer_units`
/// (creation) and the cleanup (`destroy()`'s own-group removal,
/// `find_orphaned_scrub_units`'s prune scan) so the two can never derive a
/// different name for the same group.
fn scrub_unit_stem(group_name: &str) -> String {
    format!("shr-rs-scrub-{}", sanitize_unit_name(group_name))
}

/// `(service_path, timer_path)` for `group_name`'s scrub unit pair, in
/// `dir` -- regardless of whether either file currently exists.
pub fn scrub_unit_paths(dir: &Path, group_name: &str) -> (PathBuf, PathBuf) {
    let stem = scrub_unit_stem(group_name);
    (
        dir.join(format!("{stem}.service")),
        dir.join(format!("{stem}.timer")),
    )
}

/// Whether `path` is a unit file THIS PROJECT generated -- i.e. safe
/// to delete during `destroy()`'s per-group cleanup or `schedule install`'s
/// orphan pruning. Fails closed in every direction: a missing file, a read
/// error, or existing content that simply lacks the marker (an operator's
/// own hand-written unit that happens to share the same name) all return
/// `false`. Never claim ownership of something this couldn't actually
/// verify.
pub fn is_shr_rs_owned_unit(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| content.starts_with(UNIT_OWNERSHIP_MARKER))
        .unwrap_or(false)
}

/// Delete `path` IF it exists AND `is_shr_rs_owned_unit` -- returns whether
/// a real deletion happened (`false` for "didn't exist" or "not ours",
/// which are both intentionally non-errors: `destroy()`'s cleanup and
/// `schedule install`'s pruning both need to tolerate "nothing to remove"
/// as the common case, not treat it as a failure).
pub fn remove_owned_unit_file(path: &Path) -> Result<bool, StateError> {
    if !path.exists() || !is_shr_rs_owned_unit(path) {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// Every `shr-rs-scrub-*` unit in `dir` that no longer corresponds to
/// a group `state` actually has -- i.e. left behind by a `destroy()` that
/// (on an older binary, or one that failed partway through this exact
/// cleanup) never removed it. Detection only, never touches the
/// filesystem: `owned` is what's actually safe to delete (this project's
/// own marker present); `unowned_lookalikes` merely LOOKS orphaned by
/// naming convention but carries no marker -- an operator's own same-named
/// unit -- and must only ever be reported, never removed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct OrphanedScrubUnits {
    pub owned: Vec<PathBuf>,
    pub unowned_lookalikes: Vec<PathBuf>,
}

pub fn find_orphaned_scrub_units(dir: &Path, state: &StateFile) -> Result<OrphanedScrubUnits, StateError> {
    let live_stems: std::collections::HashSet<String> =
        state.groups.iter().map(|g| scrub_unit_stem(&g.name)).collect();
    let mut result = OrphanedScrubUnits::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // No unit directory at all yet (e.g. `schedule install` has never
        // run on this host) -- nothing to prune, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(StateError::Io(e)),
    };
    for entry in entries {
        let path = entry?.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = file_name
            .strip_suffix(".service")
            .or_else(|| file_name.strip_suffix(".timer"))
        else {
            continue;
        };
        if !stem.starts_with("shr-rs-scrub-") || live_stems.contains(stem) {
            continue;
        }
        if is_shr_rs_owned_unit(&path) {
            result.owned.push(path);
        } else {
            result.unowned_lookalikes.push(path);
        }
    }
    Ok(result)
}

/// ONE global timer (not per-group -- `OrchestrationEngine::
/// tick_active_reshapes` already iterates every group/band itself) that
/// periodically ticks the adaptive reshape throttle so a running reshape
/// keeps reacting to changing conditions for its whole (potentially
/// many-hour) duration, not just the one-shot tick `start_reshape_throttle`
/// makes when the reshape starts. Every 2 minutes -- frequent enough to
/// react to a worsening SMART/temperature signal promptly, infrequent
/// enough that a `shr-rs internal reshape-throttle-tick` invocation
/// (`smartctl` per member disk, `/proc` reads) is negligible overhead.
/// `exe_path`: see `write_scrub_timer_units`'s doc comment -- same
/// "no hardcoded install path" fix, same reason.
pub fn write_throttle_timer_unit(dir: &Path, exe_path: &Path) -> Result<Vec<std::path::PathBuf>, StateError> {
    let service_path = dir.join("shr-rs-throttle-tick.service");
    let timer_path = dir.join("shr-rs-throttle-tick.timer");

    let exe = exe_path.display();
    let service = format!(
        "{UNIT_OWNERSHIP_MARKER}\n\
         [Unit]\n\
         Description=shr-rs adaptive reshape throttle tick\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} internal reshape-throttle-tick\n"
    );
    let timer = format!(
        "{UNIT_OWNERSHIP_MARKER}\n\
         [Unit]\n\
         Description=shr-rs adaptive reshape throttle timer\n\
         \n\
         [Timer]\n\
         OnCalendar=*:0/2\n\
         Persistent=false\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    );

    crate::atomic_write(&service_path, service.as_bytes(), None)?;
    crate::atomic_write(&timer_path, timer.as_bytes(), None)?;
    Ok(vec![service_path, timer_path])
}

/// ONE global timer (same "one timer, iterate every group inside the
/// engine call" shape as `write_throttle_timer_unit` above) that
/// periodically polls every group for the three notification triggers --
/// a scrub finishing with errors, a band being degraded, worsening SMART
/// health -- and fires through whatever channels `policy.toml` enables.
/// Every 15 minutes: frequent enough that a degraded array or a finished
/// scheduled scrub is noticed promptly without an operator having to poll
/// by hand, infrequent enough (vs. the throttle tick's 2 minutes, which
/// only runs while something is actively reshaping) that the per-band
/// `smartctl` reads stay negligible overhead for a check that runs
/// UNCONDITIONALLY, reshape or not.
/// `exe_path`: see `write_scrub_timer_units`'s doc comment -- same
/// "no hardcoded install path" fix, same reason.
pub fn write_health_check_timer_unit(
    dir: &Path,
    exe_path: &Path,
) -> Result<Vec<std::path::PathBuf>, StateError> {
    let service_path = dir.join("shr-rs-health-check.service");
    let timer_path = dir.join("shr-rs-health-check.timer");

    let exe = exe_path.display();
    let service = format!(
        "{UNIT_OWNERSHIP_MARKER}\n\
         [Unit]\n\
         Description=shr-rs health check and notification\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} internal health-check-tick\n"
    );
    let timer = format!(
        "{UNIT_OWNERSHIP_MARKER}\n\
         [Unit]\n\
         Description=shr-rs health check timer\n\
         \n\
         [Timer]\n\
         OnCalendar=*:0/15\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    );

    crate::atomic_write(&service_path, service.as_bytes(), None)?;
    crate::atomic_write(&timer_path, timer.as_bytes(), None)?;
    Ok(vec![service_path, timer_path])
}

/// ONE global timer (same shape as `write_throttle_timer_unit`/
/// `write_health_check_timer_unit` above -- `OrchestrationEngine::
/// snapshot_auto_run` iterates every group itself) that periodically
/// creates a new automated snapshot per group and prunes old ones beyond
/// the operator's configured retention (`policy.toml`'s `[snapshot]`
/// table). NOT per-group like the scrub timer: retention/schedule
/// are single, host-wide policy values (same "one shared config, not one
/// per feature" reasoning `shr_state::policy`'s module doc comment already
/// gives for sharing `policy.toml` with `[notify]`), so one timer that
/// loops every group inside the engine call is the right shape, same as
/// the health-check timer.
///
/// `schedule` is `policy.toml`'s `[snapshot].schedule` value, passed
/// through verbatim as `OnCalendar=` -- systemd's calendar syntax already
/// accepts both shorthand (`daily`, `weekly`) and explicit expressions, so
/// this needs no separate validation/mapping layer here; an invalid value
/// surfaces as a real `systemctl enable` failure at install time rather
/// than being silently reinterpreted.
/// `exe_path`: see `write_scrub_timer_units`'s doc comment -- same
/// "no hardcoded install path" fix, same reason.
pub fn write_snapshot_timer_unit(
    dir: &Path,
    exe_path: &Path,
    schedule: &str,
) -> Result<Vec<std::path::PathBuf>, StateError> {
    let service_path = dir.join("shr-rs-snapshot-auto.service");
    let timer_path = dir.join("shr-rs-snapshot-auto.timer");

    let exe = exe_path.display();
    let service = format!(
        "{UNIT_OWNERSHIP_MARKER}\n\
         [Unit]\n\
         Description=shr-rs scheduled snapshot automation\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} internal snapshot-auto-tick\n"
    );
    let timer = format!(
        "{UNIT_OWNERSHIP_MARKER}\n\
         [Unit]\n\
         Description=shr-rs scheduled snapshot automation timer\n\
         \n\
         [Timer]\n\
         OnCalendar={schedule}\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    );

    crate::atomic_write(&service_path, service.as_bytes(), None)?;
    crate::atomic_write(&timer_path, timer.as_bytes(), None)?;
    Ok(vec![service_path, timer_path])
}

/// systemd unit names may not contain `/` or whitespace (and conventionally
/// avoid other shell-special characters) -- replace anything outside
/// `[A-Za-z0-9_.-]` with `_` rather than rejecting the group name outright,
/// since group names are operator-chosen free text elsewhere in this
/// project (the design never restricts `create --name` to a unit-name-safe
/// charset).
fn sanitize_unit_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn splice_managed_block(existing: &str, block_content: &str, path: &Path) -> Result<String, StateError> {
    let managed = format!("{BEGIN_MARKER}\n{block_content}\n{END_MARKER}");
    let start = existing.find(BEGIN_MARKER);
    let end = existing.find(END_MARKER);

    match (start, end) {
        (Some(s), Some(e)) if e > s => {
            let end_pos = e + END_MARKER.len();
            let mut result = String::with_capacity(existing.len() + managed.len());
            result.push_str(&existing[..s]);
            result.push_str(&managed);
            result.push('\n');
            result.push_str(existing[end_pos..].trim_start_matches('\n'));
            Ok(result)
        }
        (None, None) => {
            let mut result = existing.to_string();
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&managed);
            result.push('\n');
            Ok(result)
        }
        _ => Err(StateError::ManagedBlock(format!(
            "found only one of the two `shr-rs managed` marker comments in {} -- the file \
             may have been edited by hand; refusing to guess which content to keep. Remove \
             both `{BEGIN_MARKER}` / `{END_MARKER}` lines (and whatever they used to \
             enclose) manually before retrying",
            path.display()
        ))),
    }
}
