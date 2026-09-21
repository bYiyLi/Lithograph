//! Storage integrity verification.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use super::integrity_graph::graph_integrity_issues;
use super::integrity_history::history_integrity_issues;
use super::schema::{
    FORMAT2_SCHEMA_STATEMENTS, FORMAT3_SCHEMA_STATEMENTS, STORAGE_SCHEMA_STATEMENTS,
};
use super::{
    HashId, IndexDefinition, STANDARD_INDEX_ENCODING_VERSION, SchemaState, StorageError,
    StorageResult, commit_exists,
};

/// One deterministic integrity finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityIssue {
    /// Stable machine-readable finding code.
    pub code: String,
    /// Stable human-readable finding.
    pub message: String,
}

impl IntegrityIssue {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

pub(super) fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Checks the full canonical/derived storage graph.
pub fn integrity_check(connection: &Connection) -> StorageResult<Vec<IntegrityIssue>> {
    let mut issues = structural_integrity_issues(connection)?;
    if !issues.is_empty() {
        return Ok(issues);
    }
    let (history_issues, references) = history_integrity_issues(connection)?;
    issues.extend(history_issues);
    issues.extend(graph_integrity_issues(connection, &references)?);
    if issues.is_empty() {
        issues.extend(persistent_index_integrity_issues(connection)?);
    }
    issues.sort_by(|left, right| (&left.code, &left.message).cmp(&(&right.code, &right.message)));
    issues.dedup();
    Ok(issues)
}

fn persistent_index_integrity_issues(
    connection: &Connection,
) -> StorageResult<Vec<IntegrityIssue>> {
    if integrity_storage_format(connection)? < 3 {
        return Ok(Vec::new());
    }
    let mut issues = Vec::new();
    for generation in load_persistent_generations(connection)? {
        validate_generation_identity(connection, &generation, &mut issues)?;
        validate_generation_counts(connection, &generation, &mut issues)?;
    }
    let orphan_entries = orphan_persistent_index_entries(connection)?;
    if orphan_entries != 0 {
        issues.push(IntegrityIssue::new(
            "index_generation.orphan_entries",
            format!("persistent Index storage contains {orphan_entries} orphan entries"),
        ));
    }
    Ok(issues)
}

struct PersistentGenerationIntegrity {
    id: i64,
    anchor: HashId,
    definition_hash: HashId,
    definition_blob: Vec<u8>,
    encoding_version: i64,
    complete: i64,
    indexed_entities: i64,
    entry_count: i64,
}

fn load_persistent_generations(
    connection: &Connection,
) -> StorageResult<Vec<PersistentGenerationIntegrity>> {
    let mut statement = connection.prepare(
        "SELECT generation_id, anchor_commit, definition_hash, definition_blob, encoding_version, \
                complete, indexed_entities, entry_count \
         FROM main._lithograph_index_generations ORDER BY generation_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut generations = Vec::new();
    for row in rows {
        let (
            id,
            anchor,
            definition_hash,
            definition_blob,
            encoding_version,
            complete,
            indexed_entities,
            entry_count,
        ) = row?;
        generations.push(PersistentGenerationIntegrity {
            id,
            anchor: HashId::from_slice(&anchor)?,
            definition_hash: HashId::from_slice(&definition_hash)?,
            definition_blob,
            encoding_version,
            complete,
            indexed_entities,
            entry_count,
        });
    }
    Ok(generations)
}

fn validate_generation_identity(
    connection: &Connection,
    generation: &PersistentGenerationIntegrity,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let anchor_exists = commit_exists(connection, generation.anchor)?;
    if !anchor_exists {
        issues.push(IntegrityIssue::new(
            "index_generation.missing_anchor",
            format!(
                "persistent Index generation {} references missing Commit {}",
                generation.id,
                generation.anchor.to_hex()
            ),
        ));
    }
    validate_generation_definition(connection, generation, anchor_exists, issues)?;
    if generation.encoding_version != STANDARD_INDEX_ENCODING_VERSION {
        issues.push(IntegrityIssue::new(
            "index_generation.encoding_version",
            format!(
                "persistent Index generation {} uses unsupported encoding version {}",
                generation.id, generation.encoding_version
            ),
        ));
    }
    if generation.complete != 1 {
        issues.push(IntegrityIssue::new(
            "index_generation.incomplete",
            format!(
                "persistent Index generation {} is incomplete",
                generation.id
            ),
        ));
    }
    Ok(())
}

fn validate_generation_definition(
    connection: &Connection,
    generation: &PersistentGenerationIntegrity,
    anchor_exists: bool,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_INDEX_DEFINITION_V1");
    hasher.update(&generation.definition_blob);
    let actual_hash = HashId::from_bytes(*hasher.finalize().as_bytes());
    if actual_hash != generation.definition_hash {
        issues.push(IntegrityIssue::new(
            "index_generation.definition_hash",
            format!(
                "persistent Index generation {} definition hash does not match its canonical blob",
                generation.id
            ),
        ));
    }
    let definition =
        match IndexDefinition::from_canonical_identity_blob(&generation.definition_blob) {
            Ok(definition) => definition,
            Err(_) => {
                issues.push(IntegrityIssue::new(
                    "index_generation.definition_blob",
                    format!(
                        "persistent Index generation {} has a non-canonical Index definition blob",
                        generation.id
                    ),
                ));
                return Ok(());
            }
        };
    if anchor_exists {
        let schema = SchemaState::load(connection, generation.anchor)?;
        let matches_anchor = schema
            .indexes
            .get(&definition.name)
            .map(IndexDefinition::canonical_identity_blob)
            .transpose()?
            .is_some_and(|blob| blob == generation.definition_blob);
        if !matches_anchor {
            issues.push(IntegrityIssue::new(
                "index_generation.definition_anchor_mismatch",
                format!(
                    "persistent Index generation {} definition does not match anchor Commit Schema",
                    generation.id
                ),
            ));
        }
    }
    Ok(())
}

fn validate_generation_counts(
    connection: &Connection,
    generation: &PersistentGenerationIntegrity,
    issues: &mut Vec<IntegrityIssue>,
) -> StorageResult<()> {
    let actual_entry_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_index_entries WHERE generation_id = ?1",
        [generation.id],
        |row| row.get(0),
    )?;
    if actual_entry_count != generation.entry_count {
        issues.push(IntegrityIssue::new(
            "index_generation.entry_count",
            format!(
                "persistent Index generation {} records {} entries but stores {actual_entry_count}",
                generation.id, generation.entry_count
            ),
        ));
    }
    let actual_entities: i64 = connection.query_row(
        "SELECT count(*) FROM (\
             SELECT owner_kind, owner_id FROM main._lithograph_index_entries \
             WHERE generation_id = ?1 GROUP BY owner_kind, owner_id\
         )",
        [generation.id],
        |row| row.get(0),
    )?;
    if actual_entities != generation.indexed_entities {
        issues.push(IntegrityIssue::new(
            "index_generation.entity_count",
            format!(
                "persistent Index generation {} records {} indexed entities but stores {actual_entities}",
                generation.id, generation.indexed_entities
            ),
        ));
    }
    Ok(())
}

fn orphan_persistent_index_entries(connection: &Connection) -> StorageResult<i64> {
    connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_entries AS entries \
             WHERE NOT EXISTS(\
                 SELECT 1 FROM main._lithograph_index_generations AS generations \
                 WHERE generations.generation_id = entries.generation_id\
             )",
            [],
            |row| row.get(0),
        )
        .map_err(StorageError::from)
}

/// Checks the canonical internal schema inventory and table/index shape.
pub fn structural_integrity_issues(connection: &Connection) -> StorageResult<Vec<IntegrityIssue>> {
    let storage_format = integrity_storage_format(connection)?;
    let expected = expected_objects(storage_format);
    let actual = actual_objects(connection)?;
    let mut issues = Vec::new();

    for (name, spec) in &expected {
        match actual.get(name) {
            None => issues.push(IntegrityIssue::new(
                "schema.missing_object",
                format!("missing internal {} {name}", spec.kind),
            )),
            Some(object) => compare_object(spec, object, &mut issues),
        }
    }
    for (name, object) in &actual {
        if !expected.contains_key(name) && !is_sqlite_autoindex(object) {
            issues.push(IntegrityIssue::new(
                "schema.unexpected_object",
                format!(
                    "unexpected internal {} {name} attached to {}",
                    object.kind, object.table
                ),
            ));
        }
    }
    issues.sort_by(|left, right| (&left.code, &left.message).cmp(&(&right.code, &right.message)));
    Ok(issues)
}

fn integrity_storage_format(connection: &Connection) -> StorageResult<i64> {
    let marker = connection
        .query_row(
            "SELECT storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(format) = marker {
        return Ok(format);
    }
    let format3: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='_lithograph_index_generations')",
        [],
        |row| row.get(0),
    )?;
    if format3 == 1 {
        return Ok(3);
    }
    let format2: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='_lithograph_merge_sessions')",
        [],
        |row| row.get(0),
    )?;
    Ok(if format2 == 1 { 2 } else { 1 })
}

#[derive(Debug)]
struct ObjectSpec {
    name: String,
    kind: String,
    table: String,
    sql: Option<String>,
}

fn expected_objects(storage_format: i64) -> BTreeMap<String, ObjectSpec> {
    let mut objects = BTreeMap::new();
    objects.insert(
        "_lithograph_meta".to_owned(),
        ObjectSpec {
            name: "_lithograph_meta".to_owned(),
            kind: "table".to_owned(),
            table: "_lithograph_meta".to_owned(),
            sql: None,
        },
    );
    let format2 = FORMAT2_SCHEMA_STATEMENTS
        .iter()
        .map(|sql| object_name(sql))
        .collect::<std::collections::BTreeSet<_>>();
    let format3 = FORMAT3_SCHEMA_STATEMENTS
        .iter()
        .map(|sql| object_name(sql))
        .collect::<std::collections::BTreeSet<_>>();
    for sql in STORAGE_SCHEMA_STATEMENTS {
        let name = object_name(sql);
        if storage_format < 2 && format2.contains(&name) {
            continue;
        }
        if storage_format < 3 && format3.contains(&name) {
            continue;
        }
        let spec = parse_expected_object(sql);
        objects.insert(name, spec);
    }
    objects
}

fn parse_expected_object(sql: &str) -> ObjectSpec {
    let name = object_name(sql);
    let kind = if sql.starts_with("CREATE TABLE ") {
        "table"
    } else {
        "index"
    };
    let table = if kind == "table" {
        name.clone()
    } else {
        sql.split_once(" ON ")
            .map_or_else(|| name.clone(), |(_, after_on)| first_identifier(after_on))
    };
    ObjectSpec {
        name,
        kind: kind.to_owned(),
        table,
        sql: Some(normalize_sql(sql)),
    }
}

fn object_name(sql: &str) -> String {
    let rest = sql
        .strip_prefix("CREATE TABLE ")
        .or_else(|| sql.strip_prefix("CREATE UNIQUE INDEX "))
        .or_else(|| sql.strip_prefix("CREATE INDEX "))
        .unwrap_or(sql);
    first_identifier(rest)
}

fn first_identifier(text: &str) -> String {
    text.trim_start()
        .strip_prefix("main.")
        .unwrap_or(text.trim_start())
        .split(['(', ' ', '\t', '\n'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn normalize_sql(sql: &str) -> String {
    sql.to_ascii_lowercase()
        .replace("main.", "")
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
}

fn actual_objects(connection: &Connection) -> StorageResult<BTreeMap<String, ObjectSpec>> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql FROM main.sqlite_schema WHERE lower(name) GLOB '_lithograph_*' OR lower(tbl_name) GLOB '_lithograph_*' ORDER BY type, name",
    )?;
    let mut rows = statement.query([])?;
    let mut objects = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let name: String = row.get(1)?;
        let table: String = row.get(2)?;
        let sql: Option<String> = row.get(3)?;
        objects.insert(
            name.to_ascii_lowercase(),
            ObjectSpec {
                name,
                kind,
                table,
                sql: sql.as_deref().map(normalize_sql),
            },
        );
    }
    Ok(objects)
}

fn compare_object(expected: &ObjectSpec, actual: &ObjectSpec, issues: &mut Vec<IntegrityIssue>) {
    if expected.name != actual.name
        || expected.kind != actual.kind
        || expected.table != actual.table
    {
        issues.push(IntegrityIssue::new(
            "schema.object_shape",
            format!(
                "internal object {} has type/name/table ({}, {}, {}) instead of ({}, {}, {})",
                expected.name,
                actual.kind,
                actual.name,
                actual.table,
                expected.kind,
                expected.name,
                expected.table
            ),
        ));
        return;
    }
    if let Some(expected_sql) = &expected.sql
        && actual.sql.as_ref() != Some(expected_sql)
    {
        issues.push(IntegrityIssue::new(
            "schema.sql_mismatch",
            format!(
                "internal {} {} has a non-canonical SQL definition",
                expected.kind, expected.name
            ),
        ));
    }
}

fn is_sqlite_autoindex(object: &ObjectSpec) -> bool {
    object.name.starts_with("sqlite_autoindex_") && object.kind == "index" && object.sql.is_none()
}
