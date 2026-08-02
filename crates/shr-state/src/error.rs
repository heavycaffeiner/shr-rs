use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("Deserialization error: {0}")]
    Deserialize(#[from] toml::de::Error),

    #[error("State file not found at {0}")]
    NotFound(String),

    #[error("refusing to persist a placeholder identifier: {0}")]
    PlaceholderIdentifier(String),

    #[error("refusing to persist duplicate group name: {0}")]
    DuplicateGroupName(String),

    #[error("shr-rs managed block markers are inconsistent: {0}")]
    ManagedBlock(String),
}
