use crate::cmd::ExecError;

pub struct SafetyGuard;

impl SafetyGuard {
    /// Verify that `disk_path` is not a system/OS disk.
    ///
    /// Compares the *normalized* device name against `system_disks` for
    /// exact equality -- never substring containment. Substring matching
    /// previously produced both a false negative (a by-id path for a system
    /// disk does not contain its kernel name as a substring) and a false
    /// positive (`/dev/loop10` contains `loop1`).
    ///
    /// by-id / by-uuid / by-partuuid names cannot be reduced to a kernel
    /// device name by string manipulation alone (there is no algorithmic
    /// relationship between e.g. `ata-WDC_WD40EFRX_SYS` and `sda`). Callers
    /// that resolve identity via `shr-inspect` (see
    /// `shr_command::system_disk_aliases`) are expected to list *every*
    /// known alias of a protected disk -- kernel name and by-id name -- in
    /// `system_disks`, so exact matching still catches a by-id reference to
    /// a system disk.
    ///
    /// An empty `system_disks` list is rejected as an error: the absence of
    /// a confirmed exclusion list must never be silently treated as "safe".
    pub fn validate_disk_target(disk_path: &str, system_disks: &[String]) -> Result<(), ExecError> {
        if system_disks.is_empty() {
            return Err(ExecError::SafetyViolation(
                "system-disk exclusion list is empty; refusing to treat an unconfirmed \
                 environment as safe"
                    .to_string(),
            ));
        }

        let target = normalize_disk_ref(disk_path);
        for sys_disk in system_disks {
            if normalize_disk_ref(sys_disk) == target {
                return Err(ExecError::SafetyViolation(format!(
                    "disk {disk_path} is identified as system disk {sys_disk}; destructive \
                     operation blocked"
                )));
            }
        }

        Ok(())
    }
}

/// Normalize a disk/partition reference for exact comparison: strip `/dev/`
/// and known `/dev/disk/by-*/` directory prefixes, and -- for kernel-name
/// shaped references only -- strip a trailing partition suffix (`sda1` ->
/// `sda`, `loop10p1` -> `loop10`, `nvme0n1p1` -> `nvme0n1`).
///
/// by-id/by-uuid/by-partuuid/by-path names are left as-is after prefix
/// stripping: they carry no algorithmic whole-disk/partition distinction.
fn normalize_disk_ref(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("/dev/disk/by-id/")
        .or_else(|| trimmed.strip_prefix("/dev/disk/by-uuid/"))
        .or_else(|| trimmed.strip_prefix("/dev/disk/by-partuuid/"))
        .or_else(|| trimmed.strip_prefix("/dev/disk/by-path/"))
        .unwrap_or(trimmed)
        .trim_start_matches("/dev/");

    if looks_like_kernel_name(stripped) {
        strip_partition_suffix(stripped)
    } else {
        stripped.to_string()
    }
}

/// Mirrors `shr_inspect::diskref`'s heuristic (duplicated here so `shr-exec`
/// stays free of I/O-crate dependencies): recognizes `sdX`/`hdX`/`vdX`/
/// `xvdX`/`nvme*`/`mmcblk*`/`loop*`/`dm-*` kernel device name shapes.
fn looks_like_kernel_name(s: &str) -> bool {
    // Kernel device names are always ASCII. Bailing out here also avoids a
    // byte-index panic below (`&s[..2]`/`&s[2..]` assume single-byte chars;
    // a multi-byte UTF-8 lead byte at index 1 would slice mid-character).
    if s.is_empty() || s.contains('/') || !s.is_ascii() {
        return false;
    }
    if s.starts_with("nvme") || s.starts_with("mmcblk") || s.starts_with("loop") || s.starts_with("dm-") {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 3 {
        let prefix = &s[..2];
        if matches!(prefix, "sd" | "hd" | "vd") && s[2..].chars().all(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
    }
    s.starts_with("xvd") && s.len() > 3 && s[3..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Strip a trailing partition suffix from a kernel-name-shaped device name.
///
/// Two naming families exist in the kernel:
/// - `sdX`/`hdX`/`vdX`: the whole disk name has no trailing digit, so any
///   trailing digit run is a partition number (`sda1` -> `sda`).
/// - `nvme*`/`mmcblk*`/`loop*`/`dm-*`: the whole-disk name itself ends in a
///   digit (`nvme0n1`, `loop10`), so only a `p<digits>` suffix denotes a
///   partition (`loop10p1` -> `loop10`, `nvme0n1p1` -> `nvme0n1`). Blindly
///   trimming trailing digits here would collapse `loop10` into `loop`,
///   colliding with `loop1`.
fn strip_partition_suffix(name: &str) -> String {
    let p_separated = name.starts_with("nvme")
        || name.starts_with("mmcblk")
        || name.starts_with("loop")
        || name.starts_with("dm-");

    if p_separated {
        if let Some(pos) = name.rfind('p') {
            let (base, tail) = name.split_at(pos);
            let digits = &tail[1..];
            // The `p` must separate a disk-number digit from the partition
            // number (`loop10p1`, `nvme0n1p1`) -- not just any `p` that
            // happens to occur in the device-family word itself (`loop1`
            // contains a `p` in "loop", but has no partition suffix).
            let base_ends_in_digit = base.chars().last().is_some_and(|c| c.is_ascii_digit());
            if base_ends_in_digit && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return base.to_string();
            }
        }
        return name.to_string();
    }

    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() < name.len() && !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        name.to_string()
    }
}
