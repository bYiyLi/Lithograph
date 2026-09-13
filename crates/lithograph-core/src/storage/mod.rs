//! Immutable version-aware graph storage for storage format 1.

mod checkpoint;
mod commit;
mod encoding;
mod identity;
mod integrity;
mod integrity_graph;
mod integrity_history;
mod layer;
mod layer_read;
mod property;
mod schema;
mod schema_state;
mod snapshot;
mod snapshot_labels;
mod snapshot_properties;
mod snapshot_scan;
mod snapshot_stream;
mod value;

use std::fmt;

use rusqlite::Error as SqliteError;

pub use checkpoint::{create_checkpoint, delete_checkpoint};
pub use commit::{branch_head, commit_exists, commit_layer, commit_schema, create_branch};
pub use identity::{
    allocate_node_id, allocate_relationship_id, find_label, find_property_key,
    find_relationship_type, intern_label, intern_property_key, intern_relationship_type,
    label_name, property_key_name, relationship_type_name,
};
pub use integrity::{IntegrityIssue, integrity_check, structural_integrity_issues};
pub use layer::{LayerBuilder, RelationshipRecord};
pub use schema::{
    create_storage_schema, initialize_root, load_schema_blob, persist_schema_blob, root_commit,
    schema_hash_for_commit,
};
pub use schema_state::{
    ConstraintDefinition, ConstraintDefinitionKind, GraphNodeType, GraphRelationshipType,
    IndexConfiguration, IndexDefinition, IndexTarget, PropertyRule, PropertyType, SchemaSlotChange,
    SchemaState, SchemaTarget, StandardIndexKind, constraint_slot, graph_node_slot,
    graph_relationship_slot, index_slot,
};
pub use snapshot::Snapshot;
pub use value::{PointValue, PropertyValue, VectorCoordinateType, VectorValue, ZonedDateTimeValue};

/// Current immutable storage format.
pub const STORAGE_FORMAT: i64 = 1;

/// Positive database-wide node identifier.
pub type NodeId = i64;
/// Positive database-wide relationship identifier.
pub type RelationshipId = i64;
/// Positive append-only label dictionary identifier.
pub type LabelId = i64;
/// Positive append-only relationship-type dictionary identifier.
pub type RelationshipTypeId = i64;
/// Positive append-only property-key dictionary identifier.
pub type PropertyKeyId = i64;

/// Bounded snapshot scan page. `next_after` is the last fully-consumed
/// database identity and is safe to use as the exclusive cursor for the next
/// page. `None` means the scan is exhausted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage<T> {
    pub items: Vec<T>,
    pub next_after: Option<i64>,
}

/// Stable 256-bit content identifier used by Layer, Schema, and Commit objects.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HashId([u8; 32]);

impl HashId {
    /// Creates a hash identifier from exactly 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses a persisted 32-byte digest.
    pub fn from_slice(bytes: &[u8]) -> StorageResult<Self> {
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| StorageError::corrupt("content hash must contain exactly 32 bytes"))?;
        Ok(Self(array))
    }

    /// Parses a public 64-character hexadecimal Commit identifier.
    pub fn from_hex(text: &str) -> StorageResult<Self> {
        if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StorageError::not_found(format!("Commit {text}")));
        }
        let mut bytes = [0_u8; 32];
        for (index, chunk) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let pair = std::str::from_utf8(chunk)
                .map_err(|_| StorageError::not_found(format!("Commit {text}")))?;
            bytes[index] = u8::from_str_radix(pair, 16)
                .map_err(|_| StorageError::not_found(format!("Commit {text}")))?;
        }
        Ok(Self(bytes))
    }

    /// Returns the raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the public lowercase hexadecimal representation.
    pub fn to_hex(self) -> String {
        use std::fmt::Write as _;

        let mut output = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(output, "{byte:02x}");
        }
        output
    }
}

impl fmt::Debug for HashId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("HashId")
            .field(&self.to_hex())
            .finish()
    }
}

/// Property owner kind persisted by storage format 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i64)]
pub enum OwnerKind {
    /// Node property.
    Node = 1,
    /// Relationship property.
    Relationship = 2,
}

impl OwnerKind {
    pub(crate) fn from_i64(value: i64) -> StorageResult<Self> {
        match value {
            1 => Ok(Self::Node),
            2 => Ok(Self::Relationship),
            _ => Err(StorageError::corrupt(format!(
                "invalid property owner kind {value}"
            ))),
        }
    }
}

/// Commit metadata included in the content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMetadata {
    /// Optional author string.
    pub author: Option<String>,
    /// Optional commit message.
    pub message: Option<String>,
    /// UTC Unix epoch microseconds. Root uses zero.
    pub committed_at: i64,
}

impl CommitMetadata {
    /// Deterministic metadata used only by the Root Commit.
    pub fn root() -> Self {
        Self {
            author: None,
            message: None,
            committed_at: 0,
        }
    }
}

/// Result returned by fresh/repeated storage initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    /// Deterministic Root Commit.
    pub root: HashId,
    /// Default branch name.
    pub branch: String,
}

/// Stable storage-layer error.
#[derive(Debug)]
pub enum StorageError {
    /// SQLite rejected or failed an operation.
    Sqlite(SqliteError),
    /// Canonical persisted state is invalid.
    Corrupt(String),
    /// Requested immutable object does not exist.
    NotFound(String),
    /// Compare-and-move detected a stale branch head.
    BranchHeadMoved,
}

impl StorageError {
    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite storage error: {error}"),
            Self::Corrupt(message) => write!(formatter, "corrupt Lithograph storage: {message}"),
            Self::NotFound(message) => write!(formatter, "Lithograph object not found: {message}"),
            Self::BranchHeadMoved => formatter.write_str("branch head moved during storage write"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<SqliteError> for StorageError {
    fn from(error: SqliteError) -> Self {
        Self::Sqlite(error)
    }
}

/// Result type for storage operations.
pub type StorageResult<T> = Result<T, StorageError>;
