use crate::cmd::{write_sysfs, CommandRunner, ExecError};

pub struct MdadmExecutor<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> MdadmExecutor<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }

    /// Verify `mdadm` is available before any destructive step (D11).
    pub fn ensure_supported(&self) -> Result<(), ExecError> {
        if self.runner.is_dry_run() {
            return Ok(());
        }
        self.runner.run("mdadm", &["--version"])?;
        Ok(())
    }

    /// Create new mdadm array
    pub fn create_array(&self, md_name: &str, level: &str, members: &[&str]) -> Result<(), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        let level_arg = format!("--level={}", level);
        let raid_disks_arg = format!("--raid-devices={}", members.len());

        // D6: --metadata=1.2 is the the design confirmed value -- explicit
        // rather than left to mdadm's own (version-dependent) default.
        // --bitmap=internal -- a real guest's `mdadm --detail` showed
        // `Consistency Policy : resync` (no bitmap), meaning any unclean
        // shutdown or degraded recovery forces a FULL resync instead of only
        // the changed regions. Applies to every array this executor creates
        // (both `create()`'s initial bands and `expand()`'s CreateBand
        // steps -- both call this same function), never retrofitted onto an
        // existing array (: no pre-v1 arrays exist to retrofit).
        let mut args = vec![
            "--create",
            &md_path,
            &level_arg,
            &raid_disks_arg,
            "--metadata=1.2",
            "--bitmap=internal",
            "--spare-devices=0",
            "--run",
        ];
        args.extend_from_slice(members);

        self.runner.run("mdadm", &args)?;
        Ok(())
    }

    /// Add a member partition to an existing array (as a spare, until a
    /// subsequent `grow` consumes it -- see `grow`'s doc comment).
    pub fn add_member(&self, md_name: &str, member_path: &str) -> Result<(), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        self.runner.run("mdadm", &["--add", &md_path, member_path])?;
        Ok(())
    }

    /// Detach a spare member that was `add_member`-ed but never consumed by
    /// a `grow` -- used to roll back a failed expansion attempt (D10) before
    /// the point of no return. Never call this on a member that a `grow`
    /// has already promoted into the live array: at that point the reshape
    /// is in progress and this would degrade a working array instead of
    /// undoing an unstarted change.
    pub fn remove_member(&self, md_name: &str, member_path: &str) -> Result<(), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        self.runner.run("mdadm", &["--remove", &md_path, member_path])?;
        Ok(())
    }

    /// Grow an array's device count and/or promote its level (RAID1->5,
    /// 5->6), always with `--backup-file` (the design, D6): reshape
    /// crash safety. `new_level` is `None` for a same-level device-count
    /// grow (`ExpansionStep::GrowBand`) and `Some(level)` for a level
    /// promotion (`ExpansionStep::LevelUp`).
    ///
    /// Never pass a chunk-size argument here for a RAID1 target: RAID1 has
    /// no chunk concept and mdadm rejects `--chunk` for it (confirmed by
    /// direct testing against real mdadm -- see
    /// the design's guest gotchas).
    pub fn grow(
        &self,
        md_name: &str,
        new_level: Option<&str>,
        new_raid_devices: usize,
        backup_file: &str,
    ) -> Result<(), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        let level_arg = new_level.map(|l| format!("--level={}", l));
        let count_arg = format!("--raid-devices={}", new_raid_devices);
        let backup_arg = format!("--backup-file={}", backup_file);

        let mut args = vec!["--grow", md_path.as_str()];
        if let Some(l) = &level_arg {
            args.push(l);
        }
        args.push(&count_arg);
        args.push(&backup_arg);

        self.runner.run("mdadm", &args)?;
        Ok(())
    }

    /// Replace a live member (`old_path`) with `new_path` via `mdadm
    /// --replace ... --with ...`: unlike `remove_member` + `add_member`,
    /// this keeps the array at full redundancy throughout the copy where
    /// mdadm is able to (it only drops `old_path` once `new_path` has fully
    /// caught up), rather than degrading the array for the whole rebuild.
    pub fn replace_member(&self, md_name: &str, old_path: &str, new_path: &str) -> Result<(), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };
        self.runner
            .run("mdadm", &[&md_path, "--replace", old_path, "--with", new_path])?;
        Ok(())
    }

    /// Number of missing/failed devices in an array, read from
    /// `/sys/block/<md>/md/degraded` via the runner (not a raw `std::fs`
    /// read -- see `BtrfsExecutor::ensure_supported`'s doc comment for why
    /// that matters for testability). 0 means fully redundant.
    pub fn degraded_count(&self, md_name: &str) -> Result<u32, ExecError> {
        let name = md_name.trim_start_matches("/dev/");
        if self.runner.is_dry_run() {
            return Ok(0);
        }
        let path = format!("/sys/block/{name}/md/degraded");
        let output = self.runner.run("cat", &[path.as_str()])?;
        output.stdout.trim().parse().map_err(|_| {
            ExecError::Prerequisite(format!(
                "could not parse degraded count from {path}: {:?}",
                output.stdout
            ))
        })
    }

    /// The array's current background activity, read from
    /// `/sys/block/<md>/md/sync_action` via the runner (same testability
    /// rationale as `degraded_count`). Real values include `idle`,
    /// `resync` (initial sync after `create`), `reshape` (a `--grow`-driven
    /// device-count/level change in progress), and `recover`.
    ///
    /// Discovered by running a real `expand()` against real mdadm (Phase 4
    /// Step 8): `mdadm --grow` only STARTS a reshape -- the
    /// underlying block device's reported size does not increase until the
    /// reshape actually finishes, which for real disks can take a long
    /// time. Callers use this to tell "the grow command succeeded and
    /// capacity will show up later" apart from "the grow command succeeded
    /// and capacity is available right now" before attempting to resize
    /// anything layered on top (LVM, Btrfs).
    pub fn sync_action(&self, md_name: &str) -> Result<String, ExecError> {
        let name = md_name.trim_start_matches("/dev/");
        if self.runner.is_dry_run() {
            return Ok("idle".to_string());
        }
        let path = format!("/sys/block/{name}/md/sync_action");
        let output = self.runner.run("cat", &[path.as_str()])?;
        Ok(output.stdout.trim().to_string())
    }

    /// Start a scrub (`check`, non-destructive: reads and verifies parity
    /// without repairing mismatches -- the design's scrub means `check`, not
    /// `repair`) by writing to `/sys/block/<md>/md/sync_action`.
    /// `write_sysfs` (`sh -c echo ... > path`) is the SAME sysfs-write
    /// convention Stage B's reshape throttle already settled on -- see its
    /// doc comment -- reused verbatim rather than introducing a second one.
    pub fn scrub_start(&self, md_name: &str) -> Result<(), ExecError> {
        let name = md_name.trim_start_matches("/dev/");
        write_sysfs(self.runner, &format!("/sys/block/{name}/md/sync_action"), "check")
    }

    /// Cancel a running scrub (or any other background `sync_action`) by
    /// writing `idle` -- the same control file `scrub_start` writes `check`
    /// to; mdadm accepts `idle` as "stop whatever is currently running".
    pub fn scrub_cancel(&self, md_name: &str) -> Result<(), ExecError> {
        let name = md_name.trim_start_matches("/dev/");
        write_sysfs(self.runner, &format!("/sys/block/{name}/md/sync_action"), "idle")
    }

    /// Mismatch count found by the most recent (or currently running)
    /// `check`/`repair`, read from `/sys/block/<md>/md/mismatch_cnt` --
    /// the headline "how many errors" signal this project's scrub
    /// result persistence cares about, same rationale as
    /// `BtrfsExecutor::scrub_status`'s `error_count`.
    pub fn scrub_error_count(&self, md_name: &str) -> Result<u64, ExecError> {
        let name = md_name.trim_start_matches("/dev/");
        if self.runner.is_dry_run() {
            return Ok(0);
        }
        let path = format!("/sys/block/{name}/md/mismatch_cnt");
        let output = self.runner.run("cat", &[path.as_str()])?;
        output.stdout.trim().parse().map_err(|_| {
            ExecError::Prerequisite(format!(
                "could not parse mismatch_cnt from {path}: {:?}",
                output.stdout
            ))
        })
    }

    /// Stop array
    pub fn stop_array(&self, md_name: &str) -> Result<(), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        self.runner.run("mdadm", &["--stop", &md_path])?;
        Ok(())
    }

    /// Clear an mdadm superblock left on a member after a failed create.
    pub fn zero_superblock(&self, member_path: &str) -> Result<(), ExecError> {
        self.runner
            .run("mdadm", &["--zero-superblock", "--force", member_path])?;
        Ok(())
    }

    /// Read the real array UUID via `mdadm --detail --export`'s `MD_UUID=` line.
    /// Real MD_UUID format: 4 groups of 8 lowercase hex digits, colon-separated
    /// (e.g. `12345678:abcdef01:23456789:0abcdef1`).
    ///
    /// Wrapped in [`crate::retry::retry_identity_read`]: this runs right
    /// after `create_array`, the same class of "device/array just
    /// changed, identity metadata not queryable yet under I/O load" race
    /// that `PartedExecutor::read_partuuid`'s doc comment reproduces on the
    /// real guest. Never retries under dry-run (returns above, before the
    /// helper is called). `parse_export_field` returning `None` (line
    /// missing) or `Some("")` (line present but empty) are both treated as
    /// "not ready yet" by the helper, never as a real MD_UUID.
    pub fn read_uuid(&self, md_name: &str) -> Result<String, ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        if self.runner.is_dry_run() {
            // Same rationale as PartedExecutor::read_partuuid: keep the
            // simulated state structurally valid without consulting an array
            // that dry-run intentionally never creates.
            return Ok(dry_run_md_uuid(&md_path));
        }

        crate::retry::retry_identity_read(&format!("MD_UUID for {md_path}"), || {
            let output = self.runner.run("mdadm", &["--detail", "--export", &md_path])?;
            parse_export_field(&output.stdout, "MD_UUID").ok_or_else(|| {
                ExecError::Prerequisite(format!(
                    "mdadm --detail --export {md_path} did not report MD_UUID"
                ))
            })
        })
    }

    /// Read the array's REAL level (`MD_LEVEL`, e.g. `"raid5"`) and member
    /// count (`MD_DEVICES`) from `mdadm --detail --export`.
    ///
    /// An earlier review finding: `mdadm --grow` is not atomic -- a level
    /// takeover and a device-count change are separate internal operations,
    /// so a `grow` call can return nonzero after already having changed
    /// part of the array. Before assuming a failed grow left the array
    /// untouched (and so it's safe to detach the spares attached for it),
    /// callers must verify against the array's real state, not assume it
    /// from the exit code alone.
    pub fn level_and_device_count(&self, md_name: &str) -> Result<(String, usize), ExecError> {
        let md_path = if md_name.starts_with("/dev/") {
            md_name.to_string()
        } else {
            format!("/dev/{}", md_name)
        };

        if self.runner.is_dry_run() {
            return Ok((String::new(), 0));
        }

        let output = self.runner.run("mdadm", &["--detail", "--export", &md_path])?;
        let level = parse_export_field(&output.stdout, "MD_LEVEL").ok_or_else(|| {
            ExecError::Prerequisite(format!(
                "mdadm --detail --export {md_path} did not report MD_LEVEL"
            ))
        })?;
        let devices = parse_export_field(&output.stdout, "MD_DEVICES").ok_or_else(|| {
            ExecError::Prerequisite(format!(
                "mdadm --detail --export {md_path} did not report MD_DEVICES"
            ))
        })?;
        let devices: usize = devices.parse().map_err(|_| {
            ExecError::Prerequisite(format!("MD_DEVICES `{devices}` for {md_path} is not a number"))
        })?;
        Ok((level, devices))
    }

    /// Resolve `member_path` (typically `/dev/disk/by-partuuid/<uuid>`) to
    /// the real kernel device name (e.g. `sdb1`) currently backing it
    /// via `readlink -e` -- unlike `-f`, `-e` requires EVERY path
    /// component, including the last, to actually exist right now, so a
    /// disk that has already been physically removed (leaving a dangling
    /// by-partuuid symlink) makes this return `None` rather than the stale
    /// path the symlink used to point at. Callers use the returned kernel
    /// name to match against `/proc/mdstat`'s member list (`shr_inspect::
    /// parse_mdstat`'s `MdMember`), which is keyed by kernel device name,
    /// not by-partuuid.
    pub fn resolve_member_kernel_name(&self, member_path: &str) -> Result<Option<String>, ExecError> {
        match self.runner.run("readlink", &["-e", member_path]) {
            Ok(output) => {
                let resolved = output.stdout.trim();
                Ok(resolved
                    .rsplit('/')
                    .next()
                    .map(str::to_string)
                    .filter(|s| !s.is_empty()))
            }
            Err(ExecError::NonZeroExit { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Parse a `KEY=value` line out of `mdadm --detail --export` output.
fn parse_export_field(export_output: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    export_output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(prefix.as_str())
            .map(|v| v.trim().to_string())
    })
}

/// Deterministic, structurally-valid-looking MD_UUID for dry-run simulation.
/// Not a real array UUID -- never persisted (dry-run never calls
/// `StateStore::save`) -- but shaped so downstream format validation
/// (four 8-hex-digit groups) is exercised the same way a real value would be.
fn dry_run_md_uuid(md_path: &str) -> String {
    let h1 = fnv1a(md_path, 0xcbf2_9ce4_8422_2325_u64);
    let h2 = fnv1a(md_path, 0x1234_5678_9abc_def0_u64); // distinct seed -> distinct half
    format!(
        "{:08x}:{:08x}:{:08x}:{:08x}",
        (h1 >> 32) as u32,
        h1 as u32,
        (h2 >> 32) as u32,
        h2 as u32,
    )
}

fn fnv1a(s: &str, seed: u64) -> u64 {
    s.bytes().fold(seed, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::parse_export_field;

    #[test]
    fn parses_md_uuid_from_export_output() {
        let fixture = "MD_LEVEL=raid5\nMD_DEVICES=3\nMD_METADATA=1.2\nMD_UUID=12345678:abcdef01:23456789:0abcdef1\nMD_NAME=md0\n";
        assert_eq!(
            parse_export_field(fixture, "MD_UUID"),
            Some("12345678:abcdef01:23456789:0abcdef1".to_string())
        );
    }

    #[test]
    fn returns_none_when_md_uuid_line_is_missing() {
        let fixture = "MD_LEVEL=raid5\nMD_DEVICES=3\nMD_METADATA=1.2\nMD_NAME=md0\n";
        assert_eq!(parse_export_field(fixture, "MD_UUID"), None);
    }

    #[test]
    fn trims_trailing_carriage_return_from_windows_style_line_endings() {
        let fixture = "MD_LEVEL=raid5\r\nMD_UUID=12345678:abcdef01:23456789:0abcdef1\r\nMD_NAME=md0\r\n";
        assert_eq!(
            parse_export_field(fixture, "MD_UUID"),
            Some("12345678:abcdef01:23456789:0abcdef1".to_string())
        );
    }

    #[test]
    fn parses_level_and_devices_fields() {
        let fixture = "MD_LEVEL=raid5\nMD_DEVICES=3\nMD_METADATA=1.2\nMD_NAME=md0\n";
        assert_eq!(parse_export_field(fixture, "MD_LEVEL"), Some("raid5".to_string()));
        assert_eq!(parse_export_field(fixture, "MD_DEVICES"), Some("3".to_string()));
    }
}
