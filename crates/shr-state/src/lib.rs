pub mod conf;
pub mod error;
pub mod policy;
pub mod schema;
pub mod store;

pub use error::StateError;
pub use policy::{NotifyPolicy, PolicyFile, PolicyStore};
pub use schema::{
    ArrayState, ScrubOutcome, StateBand, StateCheckpoint, StateDisk, StateExpansion, StateFile,
    StateFilesystem, StatePartition, StatePendingDisk, StateRetiredArray, StateScrubResult,
    CURRENT_SCHEMA_VERSION, DEFAULT_GROUP_NAME,
};
pub use store::StateStore;

use std::fs;
use std::io::Write;
use std::path::Path;

/// Write `content` to `path` via the tmp-write -> fsync -> rename -> parent-dir-fsync
/// sequence (D7): a crash between any two steps must never leave `path` holding a
/// partially-written file, and a crash right after `rename` must not lose the rename
/// itself to a volatile directory-entry cache (hence the trailing directory fsync).
/// `mode` sets the file's Unix permission bits before it becomes visible at `path`
/// (applied to the tmp file, so the rename never exposes a wrong-permission window);
/// `None` leaves the OS default (umask-based) permissions in place.
pub(crate) fn atomic_write(path: &Path, content: &[u8], mode: Option<u32>) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let tmp_path = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
    }

    #[cfg(unix)]
    {
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }

    fs::rename(&tmp_path, path)?;

    // Directory fsync only has real durability meaning on Unix filesystems,
    // and opening a directory as a `File` isn't portable to Windows (this
    // workspace's tests run natively on a Windows dev host -- see
    // the design Step 6 -- while the shipped binary only ever
    // runs on Linux).
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let dir_file = fs::File::open(dir)?;
        dir_file.sync_all()?;
    }

    Ok(())
}
