use crate::cmd::{CommandRunner, ExecError};

pub struct BtrfsExecutor<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> BtrfsExecutor<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }

    /// Verify Btrfs userspace and kernel support before any destructive step.
    /// A dry-run must remain runnable on hosts that intentionally lack Btrfs.
    ///
    /// Reads `/proc/filesystems` through the runner (`cat`, not
    /// `std::fs::read_to_string`) so this check goes through the same
    /// mockable abstraction as every other prerequisite/destructive call --
    /// a raw `std::fs` read bypasses `CommandRunner` entirely, which made
    /// this unreachable from a `FailingRunner`-style test double and, on
    /// Windows (this project's native test host), unconditionally failed
    /// with an IO error for any non-dry-run test.
    pub fn ensure_supported(&self) -> Result<(), ExecError> {
        if self.runner.is_dry_run() {
            return Ok(());
        }

        let output = self.runner.run("cat", &["/proc/filesystems"])?;
        if !kernel_supports_btrfs(&output.stdout) {
            return Err(ExecError::Prerequisite(
                "the running kernel does not support btrfs; install/load a compatible btrfs module first"
                    .into(),
            ));
        }

        self.runner.run("mkfs.btrfs", &["--version"])?;
        Ok(())
    }

    /// Unmount a Btrfs filesystem, for rollback of a partially-created array (D10).
    pub fn unmount(&self, mount_point: &str) -> Result<(), ExecError> {
        self.runner.run("umount", &[mount_point])?;
        Ok(())
    }

    /// Format device with Btrfs
    pub fn mkfs(&self, dev_path: &str, label: Option<&str>) -> Result<(), ExecError> {
        let mut args = vec!["-f", "-d", "single", "-m", "single"];
        if let Some(lbl) = label {
            args.push("-L");
            args.push(lbl);
        }
        args.push(dev_path);

        self.runner.run("mkfs.btrfs", &args)?;
        Ok(())
    }

    /// Mount Btrfs filesystem with options (e.g. compress=zstd:3). `subvol`
    /// selects which subvolume to mount -- `None` mounts the
    /// filesystem's default (top-level, subvolid=5) subvolume, which is
    /// what `create()` needs the FIRST time it mounts a brand-new
    /// filesystem (before `@`/`@snapshots` exist, there is nothing else to
    /// mount); `Some("@")` is every real, ongoing mount of an array's data
    /// after that.
    pub fn mount(
        &self,
        dev_path: &str,
        mount_point: &str,
        compression: Option<&str>,
        subvol: Option<&str>,
    ) -> Result<(), ExecError> {
        let mut opts = format!("compress={}", compression.unwrap_or("zstd:3"));
        if let Some(sv) = subvol {
            opts.push_str(&format!(",subvol={sv}"));
        }
        self.runner.run("mount", &["-o", &opts, dev_path, mount_point])?;
        Ok(())
    }

    /// Create a subvolume at `<mount_point>/<name>`. Must be called
    /// against a mount of the filesystem's DEFAULT (top-level) subvolume --
    /// `@`/`@snapshots` have to exist before anything can be mounted with
    /// `subvol=@`/`subvol=@snapshots`, and once something IS mounted with
    /// one of those, the other subvolumes aren't visible under it to
    /// create more inside.
    pub fn create_subvolume(&self, mount_point: &str, name: &str) -> Result<(), ExecError> {
        let path = format!("{}/{name}", mount_point.trim_end_matches('/'));
        self.runner.run("btrfs", &["subvolume", "create", &path])?;
        Ok(())
    }

    /// Create a read-only snapshot of `source_subvol_path` at
    /// `dest_snapshot_path` (the design's `fs snapshot create`).
    /// `-r`: snapshots are point-in-time backups here, not another writable
    /// working copy -- an accidentally-writable snapshot defeats the
    /// "restore to a known-good point" purpose `@snapshots` exists for.
    pub fn create_snapshot(
        &self,
        source_subvol_path: &str,
        dest_snapshot_path: &str,
    ) -> Result<(), ExecError> {
        self.runner.run(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                "-r",
                source_subvol_path,
                dest_snapshot_path,
            ],
        )?;
        Ok(())
    }

    /// List the entry names directly under `snapshots_dir` (snapshot
    /// automation's pruning step) -- a plain directory listing, not `btrfs
    /// subvolume list` (which enumerates EVERY subvolume on the whole
    /// filesystem, `@`/`@snapshots` included, keyed by subvolume ID rather
    /// than name, and would need extra filtering to narrow back down to
    /// just this one directory's children). `@snapshots` is never anything
    /// but a flat directory of snapshot subvolumes (its own layout), so a
    /// listing is both simpler and sufficient. A directory that doesn't
    /// exist yet (e.g. no snapshot has ever been created for this group)
    /// reports no entries rather than an error -- "nothing to prune" is the
    /// normal case, not a failure.
    pub fn list_snapshot_names(&self, snapshots_dir: &str) -> Result<Vec<String>, ExecError> {
        match self.runner.run("ls", &["-1", snapshots_dir]) {
            Ok(output) => Ok(output
                .stdout
                .lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect()),
            Err(ExecError::NonZeroExit { .. }) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Delete a snapshot subvolume (pruning) -- the inverse of
    /// `create_snapshot`. `-c`: commit the transaction before returning, so
    /// a caller that immediately re-lists (`list_snapshot_names`) or reads
    /// free space right after doesn't race Btrfs's async subvolume-delete
    /// worker.
    pub fn delete_subvolume(&self, path: &str) -> Result<(), ExecError> {
        self.runner.run("btrfs", &["subvolume", "delete", "-c", path])?;
        Ok(())
    }

    /// Resize filesystem to max available size
    pub fn resize_max(&self, mount_point: &str) -> Result<(), ExecError> {
        self.runner
            .run("btrfs", &["filesystem", "resize", "max", mount_point])?;
        Ok(())
    }

    /// Recompress every file under `mount_point` at `compression` via
    /// `btrfs filesystem defragment` -- Btrfs only applies a NEW compression
    /// setting to newly-written data; existing extents keep whatever
    /// compression (or none) they were written with until something
    /// rewrites them. `-r` recurses.
    ///
    /// `defragment -c` only accepts a bare algorithm name (`defragment
    /// --help`'s `-c[zlib,lzo,zstd]`) -- passing the mount-option form
    /// (`zstd:3`) straight through as `-czstd:3` made real btrfs-progs
    /// v6.12 reject it as an unknown compression type, so every recompress
    /// failed. The level component (if any) is dropped here; the level
    /// Btrfs actually rewrites extents at comes from the mount option in
    /// effect at rewrite time (see `remount_compress`), not this flag.
    pub fn recompress(&self, mount_point: &str, compression: &str) -> Result<(), ExecError> {
        let (algorithm, _level) = split_compression(compression)?;
        let clevel = format!("-c{algorithm}");
        self.runner
            .run("btrfs", &["filesystem", "defragment", "-r", &clevel, mount_point])?;
        Ok(())
    }

    /// Remount `mount_point` with a new `compress=` option, so that
    /// the level component of `compression` (which `defragment -c` cannot
    /// take, see `recompress`) is in effect for the `defragment` call that
    /// must follow -- Btrfs applies rewritten extents' compression level
    /// from the CURRENT mount option, not from any command-line flag.
    pub fn remount_compress(&self, mount_point: &str, compression: &str) -> Result<(), ExecError> {
        split_compression(compression)?;
        let opts = format!("remount,compress={compression}");
        self.runner.run("mount", &["-o", &opts, mount_point])?;
        Ok(())
    }

    /// Start a Btrfs scrub. Deliberately NOT `-B` (foreground/blocking)
    /// -- like `mdadm --grow`, this must return promptly so it never freezes
    /// the CLI/TUI/Cockpit-spawned process for however long the scrub takes
    /// (same "no blocking loop" rule `start_reshape_throttle`'s doc comment
    /// documents for reshape). The scrub runs in the kernel; `scrub_status`
    /// is how a caller checks on it afterward.
    pub fn scrub_start(&self, mount_point: &str) -> Result<(), ExecError> {
        self.runner.run("btrfs", &["scrub", "start", mount_point])?;
        Ok(())
    }

    /// Cancel a running scrub. A "not running" error from `btrfs` is
    /// surfaced as-is -- this executor reports exactly what the command
    /// said, never swallowing it itself.
    ///
    /// Correction: an earlier version of this comment claimed "not
    /// running" is always caller misuse. That is false for this method's
    /// actual caller (`OrchestrationEngine::scrub_cancel`), which starts an
    /// mdadm `check` and a Btrfs scrub together and cancels both -- the two
    /// have very different durations, so Btrfs finishing (and reporting
    /// "not running" when cancelled afterward) is ordinary operation, not
    /// misuse. Deciding that is the ENGINE's job, since only it knows it
    /// started two things with different lifetimes; this executor stays the
    /// honest, unopinionated layer and reports the real command result.
    pub fn scrub_cancel(&self, mount_point: &str) -> Result<(), ExecError> {
        self.runner.run("btrfs", &["scrub", "cancel", mount_point])?;
        Ok(())
    }

    /// Parsed `btrfs scrub status`. The headline signal this project's
    /// the design cares about is NOT "did the scrub finish" but "how many
    /// errors did it find" -- see `parse_btrfs_scrub_status`.
    pub fn scrub_status(&self, mount_point: &str) -> Result<BtrfsScrubStatus, ExecError> {
        if self.runner.is_dry_run() {
            return Ok(BtrfsScrubStatus::default());
        }
        let output = self.runner.run("btrfs", &["scrub", "status", mount_point])?;
        Ok(parse_btrfs_scrub_status(&output.stdout))
    }

    /// Live Btrfs chunk-allocation figures for `mount_point` (D1), parsed
    /// from `btrfs filesystem usage --raw` -- `--raw` reports every figure
    /// as a plain byte count (no `KiB`/`GiB` suffix), which is what makes
    /// this parseable without a unit table. Btrfs's own output is
    /// authoritative: a field this parser can't find in it comes back
    /// `None`, never a computed substitute (see [`FsUsageInput`]'s doc
    /// comment in `shr-command`, which this feeds).
    pub fn usage(&self, mount_point: &str) -> Result<BtrfsUsage, ExecError> {
        if self.runner.is_dry_run() {
            return Ok(BtrfsUsage::default());
        }
        let output = self
            .runner
            .run("btrfs", &["filesystem", "usage", "--raw", mount_point])?;
        Ok(parse_btrfs_usage(&output.stdout))
    }

    /// The `statvfs`-style "available" figure a plain `df` would show for
    /// `mount_point`, via `df -B1` (byte-exact, no unit table needed).
    /// `None` if `df`'s output isn't in the expected shape -- never guessed.
    ///
    /// `df` on a path that is NOT currently a mount point (e.g. the
    /// array is stopped and `mount_point` reverted to a plain directory)
    /// silently reports the filesystem the path happens to live on --
    /// exit 0, no error to catch. Measured on the guest: array stopped,
    /// `df -B1 /mnt/shr_data` returned `/dev/vda4`'s (root disk) 22.4 GB
    /// free, "Mounted on" column reading `/`, not `/mnt/shr_data`; with the
    /// array assembled the same command against the same path correctly
    /// reported `/dev/dm-0`'s 17.1 GB via `/mnt/shr_data`. So the row's
    /// trailing "Mounted on" column has to match the requested
    /// `mount_point`, or this returns `None` -- same "unknown, never
    /// guessed" contract `usage()`'s fields already follow. This reuses the
    /// one `df -B1` call already made (its last column already carries the
    /// real mount target) instead of a second `mountpoint`/`findmnt` call,
    /// which would leave a TOCTOU gap between checking and reading.
    pub fn free_bytes(&self, mount_point: &str) -> Result<Option<u64>, ExecError> {
        if self.runner.is_dry_run() {
            return Ok(None);
        }
        let output = self.runner.run("df", &["-B1", mount_point])?;
        Ok(parse_df_avail(&output.stdout, mount_point))
    }

    /// Read the real Btrfs filesystem UUID via `blkid -s UUID -o value`.
    ///
    /// Wrapped in [`crate::retry::retry_identity_read`] -- same udev/blkid
    /// settle race under I/O load as `PartedExecutor::read_partuuid` (see
    /// its doc comment for the real-guest reproduction this covers). Never
    /// retries under dry-run (returns above, before the helper is called),
    /// and an empty read is treated as "not ready yet" rather than a real
    /// (empty) UUID.
    pub fn read_uuid(&self, dev_path: &str) -> Result<String, ExecError> {
        if self.runner.is_dry_run() {
            // Same rationale as PartedExecutor::read_partuuid: keep the
            // simulated state structurally valid without consulting a
            // filesystem that dry-run intentionally never creates.
            return Ok(dry_run_fs_uuid(dev_path));
        }

        crate::retry::retry_identity_read(&format!("btrfs UUID for {dev_path}"), || {
            // `-c /dev/null` disables blkid's cache: without it, a device that
            // previously held a different filesystem (e.g. a reused LV name in
            // testing, or a disk that had a prior array on it) can report a
            // stale cached UUID instead of the one just created by `mkfs.btrfs`.
            let output = self.runner.run(
                "blkid",
                &["-c", "/dev/null", "-s", "UUID", "-o", "value", dev_path],
            )?;
            Ok(output.stdout.trim().to_string())
        })
    }
}

/// Deterministic, structurally-valid-looking filesystem UUID for dry-run
/// simulation (RFC4122-shaped: 8-4-4-4-12 hex groups). Never persisted.
///
/// Uses a distinct seed from `parted.rs`'s `dry_run_partuuid` so a partition
/// and the filesystem created on top of it don't hash to coincidentally
/// related-looking values.
fn dry_run_fs_uuid(dev_path: &str) -> String {
    let hash = dev_path.bytes().fold(0x9e37_79b9_7f4a_7c15_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hash >> 32) as u32,
        (hash >> 16) as u16,
        hash as u16,
        ((hash >> 32) as u16) ^ ((hash >> 8) as u16),
        hash & 0x0000_ffff_ffff_ffff,
    )
}

#[cfg(test)]
mod dry_run_uuid_tests {
    use super::dry_run_fs_uuid;

    #[test]
    fn dry_run_fs_uuid_is_36_chars_with_dashes_at_fixed_positions_and_hex_elsewhere() {
        let value = dry_run_fs_uuid("/dev/shr_vg/data");
        assert_eq!(value.len(), 36);
        for index in [8usize, 13, 18, 23] {
            assert_eq!(
                value.as_bytes()[index],
                b'-',
                "expected dash at {index} in {value}"
            );
        }
        for (index, byte) in value.bytes().enumerate() {
            if ![8, 13, 18, 23].contains(&index) {
                assert!(
                    byte.is_ascii_hexdigit(),
                    "expected hex digit at {index} in {value}"
                );
            }
        }
    }
}

/// Parsed `btrfs filesystem usage --raw <mount>` figures (D1) -- every field
/// independently optional, mirroring `shr_command::report::FsUsageInput`
/// (which this is converted into by the CLI): a field Btrfs's own output
/// doesn't carry is `None`, never computed from the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BtrfsUsage {
    pub device_size_bytes: Option<u64>,
    pub device_allocated_bytes: Option<u64>,
    pub data_used_bytes: Option<u64>,
    pub data_total_bytes: Option<u64>,
    pub metadata_used_bytes: Option<u64>,
    pub metadata_total_bytes: Option<u64>,
    pub unallocated_bytes: Option<u64>,
}

/// Parse `btrfs filesystem usage --raw` output. Real shape (raw = plain byte
/// counts, no unit suffix):
///
/// ```text
/// Overall:
///     Device size:                  21474836480
///     Device allocated:              6442450944
///     Device unallocated:           15032385536
///     ...
///
/// Data,single: Size:5368709120, Used:3187671040 (59.38%)
///    /dev/sdb1        5368709120
///
/// Metadata,DUP: Size:1073741824, Used:33554432 (3.13%)
///    /dev/sdb1        2147483648
/// ```
///
/// `Device unallocated:` (Overall section) is used directly rather than
/// summed from the per-device `Unallocated:` section below it -- both name
/// the same total, and the Overall figure needs no summing across devices.
/// A single-device or multi-profile filesystem where a section this looks
/// for (e.g. no `Metadata,` line at all) is genuinely absent leaves that
/// pair `None`, not a guess.
fn parse_btrfs_usage(text: &str) -> BtrfsUsage {
    let (data_used_bytes, data_total_bytes) = size_used_line(text, "Data,");
    let (metadata_used_bytes, metadata_total_bytes) = size_used_line(text, "Metadata,");
    BtrfsUsage {
        device_size_bytes: overall_field(text, "Device size:"),
        device_allocated_bytes: overall_field(text, "Device allocated:"),
        unallocated_bytes: overall_field(text, "Device unallocated:"),
        data_used_bytes,
        data_total_bytes,
        metadata_used_bytes,
        metadata_total_bytes,
    }
}

/// Value of an `Overall:` section field like `    Device size:  <N>`.
fn overall_field(text: &str, label: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        l.trim_start()
            .strip_prefix(label)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// `(used, total)` out of a `"<prefix><profile>: Size:<total>, Used:<used> (<pct>%)"`
/// line (e.g. `prefix = "Data,"` matches `"Data,single: Size:... "`).
fn size_used_line(text: &str, prefix: &str) -> (Option<u64>, Option<u64>) {
    let Some(after_size) = text
        .lines()
        .find(|l| l.trim_start().starts_with(prefix))
        .and_then(|l| l.split_once("Size:"))
        .map(|(_, rest)| rest)
    else {
        return (None, None);
    };
    let total = after_size.split(',').next().and_then(|s| s.trim().parse().ok());
    let used = after_size
        .split_once("Used:")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|s| s.parse().ok());
    (used, total)
}

/// Parse `df -B1 <mount>`'s "available" column, but only from a row whose
/// trailing "Mounted on" column matches `mount_point` -- `df` on a
/// path that isn't currently mounted silently reports the underlying
/// filesystem (e.g. the OS root disk) instead of erroring, so "Available"
/// alone can't be trusted without checking what it was actually measured
/// against.
///
/// Locates the `Use%` token (ends with `%`) and reads the field immediately
/// before it as "Available" and every field after it (rejoined with a
/// single space) as "Mounted on" -- not fixed column positions, since a
/// long device/filesystem name makes `df` wrap onto a second line, which
/// shifts every fixed index but leaves the `Use%`-relative ordering intact.
///
/// Mount points containing spaces: every `mount_point` this project passes
/// here is one it generated itself (`/mnt/shr_<group>`, see `mount()`'s
/// callers) and never contains whitespace, so rejoining with a single space
/// is exact in practice -- and a path with irregular internal whitespace
/// would already be ambiguous for `df`'s own space-delimited text to
/// round-trip, independent of this parser.
fn parse_df_avail(text: &str, mount_point: &str) -> Option<u64> {
    text.lines().skip(1).find_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let pct_idx = tokens.iter().position(|t| t.ends_with('%'))?;
        if pct_idx == 0 || pct_idx + 1 >= tokens.len() {
            return None;
        }
        if tokens[pct_idx + 1..].join(" ") != mount_point {
            return None;
        }
        tokens[pct_idx - 1].parse().ok()
    })
}

#[cfg(test)]
mod btrfs_usage_tests {
    use super::{parse_btrfs_usage, parse_df_avail, BtrfsUsage};

    const RAW_USAGE: &str = "Overall:\n\
        \x20\x20\x20\x20Device size:\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x2021474836480\n\
        \x20\x20\x20\x20Device allocated:\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x206442450944\n\
        \x20\x20\x20\x20Device unallocated:\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x2015032385536\n\
        \x20\x20\x20\x20Device missing:\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x200\n\
        \n\
        Data,single: Size:5368709120, Used:3187671040 (59.38%)\n\
        \x20\x20\x20/dev/sdb1\x20\x20\x20\x20\x20\x20\x205368709120\n\
        \n\
        Metadata,DUP: Size:1073741824, Used:33554432 (3.13%)\n\
        \x20\x20\x20/dev/sdb1\x20\x20\x20\x20\x20\x20\x202147483648\n\
        \n\
        Unallocated:\n\
        \x20\x20\x20/dev/sdb1\x20\x20\x20\x20\x20\x20\x2015032385536\n";

    #[test]
    fn parses_overall_and_data_metadata_figures_from_real_shaped_raw_output() {
        let usage = parse_btrfs_usage(RAW_USAGE);
        assert_eq!(
            usage,
            BtrfsUsage {
                device_size_bytes: Some(21474836480),
                device_allocated_bytes: Some(6442450944),
                data_used_bytes: Some(3187671040),
                data_total_bytes: Some(5368709120),
                metadata_used_bytes: Some(33554432),
                metadata_total_bytes: Some(1073741824),
                unallocated_bytes: Some(15032385536),
            }
        );
    }

    #[test]
    fn a_field_absent_from_the_tool_output_is_none_not_a_computed_substitute() {
        // No `Metadata,` line at all -- must not fall back to guessing it
        // from `Device allocated - Data`, or anything else.
        let text =
            "Overall:\n    Device size:            21474836480\n    Device allocated:        6442450944\n\
                     \n\
                     Data,single: Size:5368709120, Used:3187671040 (59.38%)\n   /dev/sdb1   5368709120\n";
        let usage = parse_btrfs_usage(text);
        assert_eq!(usage.metadata_used_bytes, None);
        assert_eq!(usage.metadata_total_bytes, None);
        assert_eq!(usage.data_used_bytes, Some(3187671040));
    }

    #[test]
    fn empty_output_is_all_none_not_a_parse_failure() {
        assert_eq!(parse_btrfs_usage(""), BtrfsUsage::default());
    }

    #[test]
    fn parses_df_available_bytes_from_a_single_line_row() {
        let text = "Filesystem     1B-blocks       Used   Available Use% Mounted on\n\
                     /dev/sdb1     21474836480 3221225472 17825792000  16% /mnt/shr_data\n";
        assert_eq!(parse_df_avail(text, "/mnt/shr_data"), Some(17825792000));
    }

    #[test]
    fn parses_df_available_bytes_when_the_row_wraps_onto_a_second_line() {
        // A long device/filesystem name makes real `df` put it alone on
        // its own line, with the numeric columns wrapped onto the next.
        let text = "Filesystem                                          1B-blocks       Used   Available Use% Mounted on\n\
                     /dev/mapper/a-very-long-logical-volume-name-here\n\
                     \x20\x2021474836480 3221225472 17825792000  16% /mnt/shr_data\n";
        assert_eq!(parse_df_avail(text, "/mnt/shr_data"), Some(17825792000));
    }

    #[test]
    fn unparseable_df_output_is_none() {
        assert_eq!(parse_df_avail("not df output at all\n", "/mnt/shr_data"), None);
    }

    // Real guest measurements -- array stopped vs. assembled, same
    // `mount_point`, same `df -B1` invocation.
    const UNMOUNTED_FALLTHROUGH_DF: &str =
        "Filesystem       1B-blocks       Used   Available Use% Mounted on\n\
         /dev/vda4      24546095104 2105409536 22440685568   9% /\n";
    const GENUINELY_MOUNTED_DF: &str = "Filesystem     1B-blocks     Used  Available Use% Mounted on\n\
         /dev/dm-0      17154703360 6029312 17111711744   1% /mnt/shr_data\n";

    #[test]
    fn unmounted_fallthrough_target_mismatch_is_none_not_the_root_disks_free_space() {
        // Array stopped, /mnt/shr_data reverted to a plain directory on /.
        // `df -B1 /mnt/shr_data` exits 0 and reports /dev/vda4 (root
        // disk)'s free space with "Mounted on" reading `/` -- must not be
        // mistaken for the group's own free space.
        assert_eq!(parse_df_avail(UNMOUNTED_FALLTHROUGH_DF, "/mnt/shr_data"), None);
    }

    #[test]
    fn genuinely_mounted_target_match_returns_its_own_available_bytes() {
        // Array assembled and mounted at the same path -- must still work
        // when the "Mounted on" column genuinely matches what was asked.
        assert_eq!(
            parse_df_avail(GENUINELY_MOUNTED_DF, "/mnt/shr_data"),
            Some(17111711744)
        );
    }
}

#[cfg(test)]
mod free_bytes_tests {
    use super::BtrfsExecutor;
    use crate::cmd::{CommandOutput, CommandRunner, ExecError};

    /// Returns a fixed `df` transcript regardless of args -- `free_bytes`
    /// issues exactly one command, so no dispatch-by-program-name is needed.
    struct ScriptedDfRunner {
        stdout: &'static str,
    }
    impl CommandRunner for ScriptedDfRunner {
        fn run(&self, _program: &str, _args: &[&str]) -> Result<CommandOutput, ExecError> {
            Ok(CommandOutput {
                stdout: self.stdout.to_string(),
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    #[test]
    fn free_bytes_on_an_unassembled_array_is_none_not_the_root_disks_bytes() {
        // Verbatim real guest capture: array stopped, state.toml
        // intact, /mnt/shr_data reverted to a plain directory on /.
        let runner = ScriptedDfRunner {
            stdout: "Filesystem       1B-blocks       Used   Available Use% Mounted on\n\
                     /dev/vda4      24546095104 2105409536 22440685568   9% /\n",
        };
        let result = BtrfsExecutor::new(&runner).free_bytes("/mnt/shr_data").unwrap();
        assert_eq!(
            result, None,
            "must not surface the root disk's free bytes as the group's"
        );
    }

    #[test]
    fn free_bytes_on_a_mounted_array_returns_its_own_available_bytes() {
        // Verbatim real guest capture: array assembled and mounted.
        let runner = ScriptedDfRunner {
            stdout: "Filesystem     1B-blocks     Used  Available Use% Mounted on\n\
                     /dev/dm-0      17154703360 6029312 17111711744   1% /mnt/shr_data\n",
        };
        let result = BtrfsExecutor::new(&runner).free_bytes("/mnt/shr_data").unwrap();
        assert_eq!(result, Some(17111711744));
    }
}

/// `btrfs scrub status`'s outcome, as far as this project cares:
/// whether it's still running, and how many errors it has found so far --
/// "finished" alone is not useful information, "found N errors" is (design
/// doc, Stage C's earlier note).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BtrfsScrubStatus {
    pub running: bool,
    pub error_count: u64,
}

/// Parse the human-readable text `btrfs scrub status <mount>` prints (there
/// is no stable, universally-available JSON output across the btrfs-progs
/// versions this project targets). Real shapes handled:
///
/// ```text
/// UUID:             ...
/// Scrub started:    ...
/// Status:           running
/// ...
/// Error summary:    no errors found
/// ```
///
/// or, once errors are found:
///
/// ```text
/// Status:           finished
/// Error summary:    read=3 csum=2 verify=0
///   Corrected:      5
///   Uncorrectable:  0
/// ```
///
/// `running` is true only for a `Status:` value of exactly `running`
/// (`finished`/`aborted`/`no stats available`/absent all mean "not
/// running"). `error_count` sums every `key=N` pair on the `Error summary:`
/// line; `no errors found` (or no such line at all -- e.g. a scrub that
/// hasn't produced a report yet) is 0, never an error.
fn parse_btrfs_scrub_status(text: &str) -> BtrfsScrubStatus {
    let running = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Status:"))
        .map(|v| v.trim() == "running")
        .unwrap_or(false);

    let error_count = text
        .lines()
        .find(|l| l.trim().starts_with("Error summary:"))
        .map(|l| {
            l.split_whitespace()
                .filter_map(|token| token.split('=').nth(1))
                .filter_map(|n| n.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0);

    BtrfsScrubStatus { running, error_count }
}

#[cfg(test)]
mod scrub_status_tests {
    use super::{parse_btrfs_scrub_status, BtrfsScrubStatus};

    #[test]
    fn running_scrub_with_no_errors_yet() {
        let text = "UUID:             abc\nScrub started:    now\nStatus:           running\n\
                     Duration:         0:00:05\nError summary:    no errors found\n";
        assert_eq!(
            parse_btrfs_scrub_status(text),
            BtrfsScrubStatus {
                running: true,
                error_count: 0
            }
        );
    }

    #[test]
    fn finished_scrub_sums_every_error_category() {
        let text = "Status:           finished\nError summary:    read=3 csum=2 verify=0\n  \
                     Corrected:      5\n  Uncorrectable:  0\n";
        assert_eq!(
            parse_btrfs_scrub_status(text),
            BtrfsScrubStatus {
                running: false,
                error_count: 5
            }
        );
    }

    #[test]
    fn finished_scrub_with_no_errors_reports_zero_not_a_parse_failure() {
        let text = "Status:           finished\nError summary:    no errors found\n";
        assert_eq!(
            parse_btrfs_scrub_status(text),
            BtrfsScrubStatus {
                running: false,
                error_count: 0
            }
        );
    }

    #[test]
    fn a_scrub_that_never_ran_reports_not_running_with_no_errors() {
        assert_eq!(
            parse_btrfs_scrub_status("no stats available\n"),
            BtrfsScrubStatus::default()
        );
    }

    #[test]
    fn aborted_scrub_is_not_running() {
        let text = "Status:           aborted\nError summary:    read=1\n";
        assert_eq!(
            parse_btrfs_scrub_status(text),
            BtrfsScrubStatus {
                running: false,
                error_count: 1
            }
        );
    }
}

/// Split a Btrfs `compress=` mount-option value into its algorithm
/// and optional level, e.g. `"zstd:3"` -> `("zstd", Some("3"))`,
/// `"lzo"` -> `("lzo", None)`. Rejects an unrecognized algorithm or a
/// non-numeric level up front rather than letting a typo reach `mount`/
/// `btrfs` and fail with a less specific error.
fn split_compression(compression: &str) -> Result<(&str, Option<&str>), ExecError> {
    const KNOWN_ALGORITHMS: [&str; 4] = ["zstd", "lzo", "zlib", "none"];
    let (algorithm, level) = match compression.split_once(':') {
        Some((a, l)) => (a, Some(l)),
        None => (compression, None),
    };
    if !KNOWN_ALGORITHMS.contains(&algorithm) {
        return Err(ExecError::Prerequisite(format!(
            "unknown btrfs compression algorithm `{algorithm}` in `{compression}` -- expected \
             one of {KNOWN_ALGORITHMS:?}"
        )));
    }
    if let Some(l) = level {
        if l.is_empty() || !l.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ExecError::Prerequisite(format!(
                "invalid btrfs compression level `{l}` in `{compression}` -- expected a plain \
                 integer"
            )));
        }
    }
    Ok((algorithm, level))
}

#[cfg(test)]
mod split_compression_tests {
    use super::split_compression;

    #[test]
    fn splits_algorithm_and_level() {
        assert_eq!(split_compression("zstd:3").unwrap(), ("zstd", Some("3")));
    }

    #[test]
    fn accepts_bare_algorithm_with_no_level() {
        assert_eq!(split_compression("lzo").unwrap(), ("lzo", None));
    }

    #[test]
    fn rejects_unknown_algorithm() {
        assert!(split_compression("bogus").is_err());
    }

    #[test]
    fn rejects_non_numeric_level() {
        assert!(split_compression("zstd:abc").is_err());
    }
}

#[cfg(test)]
mod recompress_tests {
    use super::BtrfsExecutor;
    use crate::cmd::{CommandOutput, CommandRunner, ExecError};
    use std::sync::Mutex;

    #[derive(Default)]
    struct SpyRunner {
        commands: Mutex<Vec<String>>,
    }
    impl SpyRunner {
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }
    impl CommandRunner for SpyRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
            self.commands
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    #[test]
    fn recompress_passes_bare_algorithm_never_the_level() {
        let runner = SpyRunner::default();
        BtrfsExecutor::new(&runner)
            .recompress("/mnt/shr", "zstd:3")
            .unwrap();
        let commands = runner.commands();
        assert!(commands
            .iter()
            .any(|c| c.contains("-czstd") && !c.contains("-czstd:3")));
    }

    #[test]
    fn recompress_rejects_unknown_compression_before_running_anything() {
        let runner = SpyRunner::default();
        assert!(BtrfsExecutor::new(&runner)
            .recompress("/mnt/shr", "bogus")
            .is_err());
        assert!(runner.commands().is_empty());
    }

    #[test]
    fn remount_compress_keeps_the_level_in_the_mount_option() {
        let runner = SpyRunner::default();
        BtrfsExecutor::new(&runner)
            .remount_compress("/mnt/shr", "zstd:3")
            .unwrap();
        let commands = runner.commands();
        assert!(commands.iter().any(|c| c.contains("remount,compress=zstd:3")));
    }
}

fn kernel_supports_btrfs(filesystems: &str) -> bool {
    filesystems
        .lines()
        .any(|line| line.split_whitespace().last() == Some("btrfs"))
}

#[cfg(test)]
mod tests {
    use super::kernel_supports_btrfs;

    #[test]
    fn finds_btrfs_as_builtin_or_module() {
        assert!(kernel_supports_btrfs("nodev\tsysfs\nbtrfs\n"));
        assert!(kernel_supports_btrfs("nodev\tbtrfs\n"));
        assert!(!kernel_supports_btrfs("nodev\tsysfs\nxfs\n"));
    }
}
