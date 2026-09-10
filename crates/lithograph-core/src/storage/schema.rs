use rusqlite::{Connection, OptionalExtension, params};

use super::encoding::{hash, i64_bytes, optional_bytes, optional_string, record};
use super::layer::{LayerBuilder, persist_layer};
use super::{CommitMetadata, HashId, RootInfo, STORAGE_FORMAT, StorageError, StorageResult};

pub(crate) const STORAGE_SCHEMA_STATEMENTS: &[&str] = &[
    "CREATE TABLE main._lithograph_sequences(kind INTEGER PRIMARY KEY CHECK(kind BETWEEN 1 AND 6), next_id INTEGER NOT NULL CHECK(next_id > 0))",
    "CREATE TABLE main._lithograph_labels(id INTEGER PRIMARY KEY CHECK(id > 0), name TEXT NOT NULL)",
    "CREATE UNIQUE INDEX main._lithograph_labels_name ON _lithograph_labels(name)",
    "CREATE TABLE main._lithograph_rel_types(id INTEGER PRIMARY KEY CHECK(id > 0), name TEXT NOT NULL)",
    "CREATE UNIQUE INDEX main._lithograph_rel_types_name ON _lithograph_rel_types(name)",
    "CREATE TABLE main._lithograph_prop_keys(id INTEGER PRIMARY KEY CHECK(id > 0), name TEXT NOT NULL)",
    "CREATE UNIQUE INDEX main._lithograph_prop_keys_name ON _lithograph_prop_keys(name)",
    "CREATE TABLE main._lithograph_layers(id INTEGER PRIMARY KEY CHECK(id > 0), hash BLOB NOT NULL CHECK(length(hash) = 32))",
    "CREATE UNIQUE INDEX main._lithograph_layers_hash ON _lithograph_layers(hash)",
    "CREATE TABLE main._lithograph_schema_objects(hash BLOB NOT NULL CHECK(length(hash) = 32), canonical_blob BLOB NOT NULL, PRIMARY KEY(hash)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_commits(id BLOB NOT NULL CHECK(length(id) = 32), format_version INTEGER NOT NULL, parent1 BLOB NULL CHECK(parent1 IS NULL OR length(parent1) = 32), parent2 BLOB NULL CHECK(parent2 IS NULL OR length(parent2) = 32), layer_id INTEGER NOT NULL CHECK(layer_id > 0), schema_hash BLOB NOT NULL CHECK(length(schema_hash) = 32), author TEXT NULL, message TEXT NULL, committed_at INTEGER NOT NULL, PRIMARY KEY(id)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_branches(name TEXT NOT NULL, commit_id BLOB NOT NULL CHECK(length(commit_id) = 32), PRIMARY KEY(name)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_node_delta(layer_id INTEGER NOT NULL CHECK(layer_id > 0), node_id INTEGER NOT NULL CHECK(node_id > 0), op INTEGER NOT NULL CHECK(op IN (1, 2)), PRIMARY KEY(layer_id, node_id)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_label_delta(layer_id INTEGER NOT NULL CHECK(layer_id > 0), node_id INTEGER NOT NULL CHECK(node_id > 0), label_id INTEGER NOT NULL CHECK(label_id > 0), op INTEGER NOT NULL CHECK(op IN (1, 2)), PRIMARY KEY(layer_id, node_id, label_id)) WITHOUT ROWID",
    "CREATE INDEX main._lithograph_label_delta_by_label ON _lithograph_label_delta(layer_id, label_id, node_id)",
    "CREATE TABLE main._lithograph_rel_delta(layer_id INTEGER NOT NULL CHECK(layer_id > 0), relationship_id INTEGER NOT NULL CHECK(relationship_id > 0), source_id INTEGER NOT NULL CHECK(source_id > 0), type_id INTEGER NOT NULL CHECK(type_id > 0), target_id INTEGER NOT NULL CHECK(target_id > 0), op INTEGER NOT NULL CHECK(op IN (1, 2)), PRIMARY KEY(layer_id, relationship_id)) WITHOUT ROWID",
    "CREATE INDEX main._lithograph_rel_delta_out ON _lithograph_rel_delta(layer_id, source_id, type_id, target_id, relationship_id)",
    "CREATE INDEX main._lithograph_rel_delta_in ON _lithograph_rel_delta(layer_id, target_id, type_id, source_id, relationship_id)",
    "CREATE INDEX main._lithograph_rel_delta_identity ON _lithograph_rel_delta(relationship_id, layer_id)",
    "CREATE TABLE main._lithograph_property_delta(layer_id INTEGER NOT NULL CHECK(layer_id > 0), owner_kind INTEGER NOT NULL CHECK(owner_kind IN (1, 2)), owner_id INTEGER NOT NULL CHECK(owner_id > 0), key_id INTEGER NOT NULL CHECK(key_id > 0), op INTEGER NOT NULL CHECK(op IN (1, 2)), type_tag INTEGER NULL CHECK(type_tag IS NULL OR type_tag BETWEEN 1 AND 14), int_value INTEGER NULL, real_value REAL NULL, text_value TEXT NULL, blob_value BLOB NULL, aux_value BLOB NULL, PRIMARY KEY(layer_id, owner_kind, owner_id, key_id)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_checkpoints(commit_id BLOB NOT NULL CHECK(length(commit_id) = 32), created_at INTEGER NOT NULL, metadata BLOB NULL, PRIMARY KEY(commit_id)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_cp_nodes(commit_id BLOB NOT NULL CHECK(length(commit_id) = 32), node_id INTEGER NOT NULL CHECK(node_id > 0), PRIMARY KEY(commit_id, node_id)) WITHOUT ROWID",
    "CREATE TABLE main._lithograph_cp_labels(commit_id BLOB NOT NULL CHECK(length(commit_id) = 32), node_id INTEGER NOT NULL CHECK(node_id > 0), label_id INTEGER NOT NULL CHECK(label_id > 0), PRIMARY KEY(commit_id, node_id, label_id)) WITHOUT ROWID",
    "CREATE INDEX main._lithograph_cp_labels_by_label ON _lithograph_cp_labels(commit_id, label_id, node_id)",
    "CREATE TABLE main._lithograph_cp_relationships(commit_id BLOB NOT NULL CHECK(length(commit_id) = 32), relationship_id INTEGER NOT NULL CHECK(relationship_id > 0), source_id INTEGER NOT NULL CHECK(source_id > 0), type_id INTEGER NOT NULL CHECK(type_id > 0), target_id INTEGER NOT NULL CHECK(target_id > 0), PRIMARY KEY(commit_id, relationship_id)) WITHOUT ROWID",
    "CREATE INDEX main._lithograph_cp_rel_out ON _lithograph_cp_relationships(commit_id, source_id, type_id, target_id, relationship_id)",
    "CREATE INDEX main._lithograph_cp_rel_in ON _lithograph_cp_relationships(commit_id, target_id, type_id, source_id, relationship_id)",
    "CREATE TABLE main._lithograph_cp_properties(commit_id BLOB NOT NULL CHECK(length(commit_id) = 32), owner_kind INTEGER NOT NULL CHECK(owner_kind IN (1, 2)), owner_id INTEGER NOT NULL CHECK(owner_id > 0), key_id INTEGER NOT NULL CHECK(key_id > 0), type_tag INTEGER NOT NULL CHECK(type_tag BETWEEN 1 AND 14), int_value INTEGER NULL, real_value REAL NULL, text_value TEXT NULL, blob_value BLOB NULL, aux_value BLOB NULL, PRIMARY KEY(commit_id, owner_kind, owner_id, key_id)) WITHOUT ROWID",
];

pub fn create_storage_schema(connection: &Connection) -> StorageResult<()> {
    for statement in STORAGE_SCHEMA_STATEMENTS {
        connection.execute_batch(statement)?;
    }
    for kind in 1_i64..=6 {
        connection.execute(
            "INSERT INTO main._lithograph_sequences(kind, next_id) VALUES(?1, 1)",
            [kind],
        )?;
    }
    Ok(())
}

pub fn initialize_root(connection: &Connection) -> StorageResult<RootInfo> {
    let count: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    if count > 0 {
        return existing_root_info(connection);
    }

    let schema_blob = record("SCHEMA", &[]);
    let schema_hash = hash(&schema_blob);
    connection.execute(
        "INSERT INTO main._lithograph_schema_objects(hash, canonical_blob) VALUES(?1, ?2)",
        params![schema_hash.as_bytes().as_slice(), schema_blob],
    )?;

    let layer = LayerBuilder::default();
    let (layer_id, layer_hash) = persist_layer(connection, &layer)?;
    let metadata = CommitMetadata::root();
    let commit = commit_hash(None, None, layer_hash, schema_hash, &metadata);
    connection.execute(
        "INSERT INTO main._lithograph_commits(id, format_version, parent1, parent2, layer_id, schema_hash, author, message, committed_at) VALUES(?1, ?2, NULL, NULL, ?3, ?4, NULL, NULL, 0)",
        params![commit.as_bytes().as_slice(), STORAGE_FORMAT, layer_id, schema_hash.as_bytes().as_slice()],
    )?;
    connection.execute(
        "INSERT INTO main._lithograph_branches(name, commit_id) VALUES('main', ?1)",
        [commit.as_bytes().as_slice()],
    )?;
    Ok(RootInfo {
        root: commit,
        branch: "main".to_owned(),
    })
}

pub fn root_commit(connection: &Connection) -> StorageResult<HashId> {
    let mut statement = connection.prepare(
        "SELECT id FROM main._lithograph_commits WHERE parent1 IS NULL AND parent2 IS NULL ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    let first = rows
        .next()?
        .ok_or_else(|| StorageError::corrupt("storage has no Root Commit"))?;
    let bytes: Vec<u8> = first.get(0)?;
    let root = HashId::from_slice(&bytes)?;
    if rows.next()?.is_some() {
        return Err(StorageError::corrupt("storage has multiple Root Commits"));
    }
    Ok(root)
}

fn existing_root_info(connection: &Connection) -> StorageResult<RootInfo> {
    let root = root_commit(connection)?;
    let main: Option<Vec<u8>> = connection
        .query_row(
            "SELECT commit_id FROM main._lithograph_branches WHERE name = 'main'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if main.is_none() {
        return Err(StorageError::corrupt(
            "initialized storage is missing main branch",
        ));
    }
    Ok(RootInfo {
        root,
        branch: "main".to_owned(),
    })
}

pub(crate) fn commit_hash(
    parent1: Option<HashId>,
    parent2: Option<HashId>,
    layer_hash: HashId,
    schema_hash: HashId,
    metadata: &CommitMetadata,
) -> HashId {
    let fields = vec![
        i64_bytes(STORAGE_FORMAT),
        optional_bytes(
            parent1
                .as_ref()
                .map(HashId::as_bytes)
                .map(<[u8; 32]>::as_slice),
        ),
        optional_bytes(
            parent2
                .as_ref()
                .map(HashId::as_bytes)
                .map(<[u8; 32]>::as_slice),
        ),
        layer_hash.as_bytes().to_vec(),
        schema_hash.as_bytes().to_vec(),
        optional_string(metadata.author.as_deref()),
        optional_string(metadata.message.as_deref()),
        i64_bytes(metadata.committed_at),
    ];
    hash(&record("COMMIT", &fields))
}
