#[derive(thiserror::Error, Debug)]
pub enum GraphError {
    #[error("Node not found: {0}")]
    NodeNotFound(String),
    #[error("Edge not found: {0}")]
    EdgeNotFound(String),
    #[error("Schema error: {0}")]
    SchemaError(String),
    #[error("Storage corrupted at page {0}")]
    StorageCorrupted(u32),
    #[error("Unsupported file format version: found {found}, supported {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("Topology segment capacity exceeded: {kind} needs {needed} pages but only {available} are available")]
    CapacityExceeded {
        kind: &'static str,
        needed: usize,
        available: usize,
    },
    #[error("Invalid traversal command: {0}")]
    InvalidCommand(String),
    #[error("Type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
    #[error("Validation error: {0}")]
    ValidationError(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
