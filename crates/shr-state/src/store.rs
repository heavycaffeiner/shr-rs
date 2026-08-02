use crate::error::StateError;
use crate::schema::{ArrayState, LegacyArrayStateV1, StateFile};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Load every group this host manages. Handles two on-disk shapes:
    ///
    /// 1. The current `StateFile { schema_version, groups: [..] }` wrapper.
    /// 2. The pre-multi-group shape (a bare `ArrayState`-like table at the
    ///    file's top level, no `groups`/`schema_version`/`name` keys at
    ///    all) -- migrated in memory to a single group named
    ///    `DEFAULT_GROUP_NAME`. This is required backward compatibility:
    ///    an operator's existing `state.toml` from before this change must
    ///    keep working, and losing its content on the next `save()` (which
    ///    always writes the current shape) would be a silent data-loss bug.
    ///
    /// A file that matches NEITHER shape is genuinely corrupt (or garbage),
    /// and reports the error from attempt 1 (the current, primary shape) --
    /// not attempt 2's, which would read something misleading like "missing
    /// field `groups`" for a file that was never trying to be the legacy
    /// shape in the first place.
    pub fn load(&self) -> Result<Option<StateFile>, StateError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&self.path)?;
        match toml::from_str::<StateFile>(&content) {
            Ok(state) => Ok(Some(state)),
            Err(current_shape_err) => match toml::from_str::<LegacyArrayStateV1>(&content) {
                Ok(legacy) => Ok(Some(StateFile::migrate_single(ArrayState::from(legacy)))),
                Err(_) => Err(StateError::Deserialize(current_shape_err)),
            },
        }
    }

    pub fn save(&self, state: &StateFile) -> Result<(), StateError> {
        state.validate_unique_group_names()?;
        state.validate_no_placeholder_identifiers()?;

        let content = toml::to_string_pretty(state)?;
        // state.toml holds real by-id/PARTUUID/md-UUID/Btrfs-UUID identity
        // data (D7) -- 0600 keeps it from being group/world readable.
        crate::atomic_write(&self.path, content.as_bytes(), Some(0o600))?;
        Ok(())
    }

    pub fn delete(&self) -> Result<(), StateError> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}
