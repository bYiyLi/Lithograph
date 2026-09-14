//! Storage integrity verification.

use std::collections::BTreeMap;

use rusqlite::Connection;

use super::StorageResult;
use super::integrity_graph::graph_integrity_issues;
use super::integrity_history::history_integrity_issues;
use super::schema::STORAGE_SCHEMA_STATEMENTS;

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
    issues.sort_by(|left, right| (&left.code, &left.message).cmp(&(&right.code, &right.message)));
    issues.dedup();
    Ok(issues)
}

/// Checks the canonical internal schema inventory and table/index shape.
pub fn structural_integrity_issues(connection: &Connection) -> StorageResult<Vec<IntegrityIssue>> {
    let expected = expected_objects();
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

#[derive(Debug)]
struct ObjectSpec {
    name: String,
    kind: String,
    table: String,
    sql: Option<String>,
}

fn expected_objects() -> BTreeMap<String, ObjectSpec> {
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
    for sql in STORAGE_SCHEMA_STATEMENTS {
        let spec = parse_expected_object(sql);
        let name = object_name(sql);
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
