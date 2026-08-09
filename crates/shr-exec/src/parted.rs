use crate::cmd::{CommandRunner, ExecError};

pub struct PartedExecutor<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> PartedExecutor<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }

    /// Create GPT partition table on disk
    pub fn create_gpt(&self, dev_path: &str) -> Result<(), ExecError> {
        self.runner.run("parted", &["-s", dev_path, "mklabel", "gpt"])?;
        Ok(())
    }

    /// Add partition given start and end byte offsets
    pub fn add_partition(&self, dev_path: &str, start_bytes: u64, end_bytes: u64) -> Result<(), ExecError> {
        let start = format!("{}B", start_bytes);
        let end = format!("{}B", end_bytes);
        self.runner
            .run("parted", &["-s", dev_path, "mkpart", "primary", &start, &end])?;
        Ok(())
    }

    /// Verify `parted` is available before any destructive step (D11).
    pub fn ensure_supported(&self) -> Result<(), ExecError> {
        if self.runner.is_dry_run() {
            return Ok(());
        }
        self.runner.run("parted", &["--version"])?;
        Ok(())
    }

    /// Remove a partition, for rollback of a partially-created array (D10).
    pub fn remove_partition(&self, dev_path: &str, part_num: u32) -> Result<(), ExecError> {
        let part_str = part_num.to_string();
        self.runner.run("parted", &["-s", dev_path, "rm", &part_str])?;
        Ok(())
    }

    /// Set partition flag/type to linux raid
    pub fn set_raid_flag(&self, dev_path: &str, part_num: u32) -> Result<(), ExecError> {
        let part_str = part_num.to_string();
        self.runner
            .run("parted", &["-s", dev_path, "set", &part_str, "raid", "on"])?;
        Ok(())
    }

    /// Get PARTUUID for a partition device path using blkid.
    ///
    /// Wrapped in [`crate::retry::retry_identity_read`]: `blkid` reading a
    /// partition's identity right after it was created/changed is exactly
    /// the settle race that helper exists for -- see its doc comment for
    /// the real-guest reproduction. Never retries under dry-run (returns
    /// above, before the retry helper is even called), and never yields an
    /// empty string as if it were a real PARTUUID (empty output is treated
    /// as "not ready yet" and retried, then surfaces as a clear error).
    pub fn read_partuuid(&self, part_path: &str) -> Result<String, ExecError> {
        if self.runner.is_dry_run() {
            // Keep the simulated state structurally valid without consulting a
            // device that dry-run intentionally never creates.
            return Ok(dry_run_partuuid(part_path));
        }

        crate::retry::retry_identity_read(&format!("PARTUUID for {part_path}"), || {
            // `-c /dev/null` disables blkid's cache -- see BtrfsExecutor::read_uuid
            // for why a stale cached value would otherwise be possible here too.
            let output = self.runner.run(
                "blkid",
                &["-c", "/dev/null", "-s", "PARTUUID", "-o", "value", part_path],
            )?;
            Ok(output.stdout.trim().to_string())
        })
    }

    /// Resolve the partition device path to use for reading identity info
    /// (`read_partuuid`) right after creating partition `part_num` on
    /// `disk_path`.
    ///
    /// [`partition_dev_path`]'s `-partN` suffix scheme is only valid for
    /// real udev-managed by-id names (`ata-*`/`nvme-*`/`wwn-*`/`scsi-*`) --
    /// found while running Step 3's real-VM smoke test, where a hand-made
    /// fixture symlink like `ata-LOOP_DISK_10` has no matching
    /// `ata-LOOP_DISK_10-part1` link, because nothing generates one for a
    /// synthetic name. Even for genuinely udev-managed names, that link's
    /// creation is asynchronous relative to the kernel's own partition-table
    /// re-read. Resolving to the canonical kernel path first (e.g.
    /// `/dev/loop10`, which the kernel creates `/dev/loop10p1` under
    /// synchronously) avoids depending on udev timing at all.
    ///
    /// A no-op passthrough under dry-run: there is nothing on disk to
    /// canonicalize (a fixture's by-id symlink may not even exist on the
    /// host running the test).
    pub fn partition_path_for_read(&self, disk_path: &str, part_num: u32) -> String {
        if self.runner.is_dry_run() {
            return partition_dev_path(disk_path, part_num);
        }
        let kernel_path = std::fs::canonicalize(disk_path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| disk_path.to_string());
        partition_dev_path(&kernel_path, part_num)
    }

    /// Wait for udev to finish processing partition-table-change events
    /// (populating `/dev/disk/by-partuuid/*` and by-id partition links)
    /// before code that depends on those symlinks runs -- specifically,
    /// `mdadm --create`'s member paths are constructed as
    /// `/dev/disk/by-partuuid/<uuid>`. A no-op under dry-run.
    pub fn settle_udev(&self) -> Result<(), ExecError> {
        if self.runner.is_dry_run() {
            return Ok(());
        }
        self.runner.run("udevadm", &["settle", "--timeout=10"])?;
        Ok(())
    }
}

/// A partition that doesn't
/// exist yet has no real PARTUUID for ANYTHING to know in advance -- blkid
/// only assigns one for real once the partition actually exists. The
/// previous placeholder (`00000000-0000-4000-8000-<hash>`) was shaped
/// EXACTLY like a real UUID, so a preview's `mdadm --create .../by-partuuid/
/// 00000000-...` line gave no way to tell it apart from a command that
/// would run verbatim -- the defect's core complaint (an operator must be
/// able to trust that what the preview shows is what will really run).
/// This format fails a UUID-shape check on sight (no dashes at UUID
/// positions, a non-hex `pending-` prefix) specifically so it reads as
/// "not a real identifier" instead of a plausible one. Still stable per
/// input (same simulated partition always gets the same placeholder within
/// one preview) so a preview's plan output stays internally consistent
/// across repeated reads.
fn dry_run_partuuid(part_path: &str) -> String {
    // FNV-1a gives a stable, dependency-free identifier for a simulated path.
    let hash = part_path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("pending-{:012x}", hash & 0x0000_ffff_ffff_ffff)
}

/// Format standard partition device path for a given disk path and partition number.
pub fn partition_dev_path(dev_path: &str, part_num: u32) -> String {
    let path = std::path::Path::new(dev_path);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(dev_path);

    if name.starts_with("ata-")
        || name.starts_with("wwn-")
        || name.starts_with("nvme-")
        || name.starts_with("scsi-")
    {
        format!("{}-part{}", dev_path, part_num)
    } else if name.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        format!("{}p{}", dev_path, part_num)
    } else {
        format!("{}{}", dev_path, part_num)
    }
}
