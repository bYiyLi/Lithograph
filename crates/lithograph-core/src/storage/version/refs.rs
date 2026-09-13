use rusqlite::{Connection, OptionalExtension, params};

use super::super::{HashId, StorageError, StorageResult};

const CONNECTION_STATE_TABLE: &str = "_lithograph_connection_state";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRef {
    pub name: String,
    pub commit: HashId,
}

pub fn validate_ref_name(name: &str) -> StorageResult<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 {
        return Err(StorageError::corrupt(
            "ref name must contain between 1 and 255 UTF-8 bytes",
        ));
    }
    if bytes
        .iter()
        .any(|byte| *byte == 0 || *byte < 0x20 || *byte == 0x7f)
    {
        return Err(StorageError::corrupt(
            "ref name must not contain NUL or ASCII control characters",
        ));
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(StorageError::corrupt(
            "ref name must not start or end with '/'",
        ));
    }
    if name
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(StorageError::corrupt(
            "ref name contains an invalid path segment",
        ));
    }
    Ok(())
}

pub fn initialize_connection_state(connection: &Connection) -> StorageResult<()> {
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE IF NOT EXISTS {CONNECTION_STATE_TABLE}(id INTEGER PRIMARY KEY CHECK(id = 1), active_branch TEXT NOT NULL);\
         INSERT OR IGNORE INTO temp.{CONNECTION_STATE_TABLE}(id, active_branch) VALUES(1, 'main')"
    ))?;
    Ok(())
}

pub fn active_branch(connection: &Connection) -> StorageResult<String> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp.sqlite_schema WHERE type = 'table' AND name = ?1)",
        [CONNECTION_STATE_TABLE],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok("main".to_owned());
    }
    connection
        .query_row(
            &format!("SELECT active_branch FROM temp.{CONNECTION_STATE_TABLE} WHERE id = 1"),
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StorageError::corrupt("connection state has no active Branch row"))
}

pub fn set_active_branch(connection: &Connection, branch: &str) -> StorageResult<HashId> {
    validate_ref_name(branch)?;
    let commit = super::super::branch_head(connection, branch)?;
    initialize_connection_state(connection)?;
    connection.execute(
        &format!("UPDATE temp.{CONNECTION_STATE_TABLE} SET active_branch = ?1 WHERE id = 1"),
        [branch],
    )?;
    Ok(commit)
}

pub fn create_branch_ref(connection: &Connection, name: &str, from: HashId) -> StorageResult<()> {
    validate_ref_name(name)?;
    super::super::create_branch(connection, name, from)
}

pub fn delete_branch_ref(connection: &Connection, name: &str) -> StorageResult<Option<HashId>> {
    validate_ref_name(name)?;
    if name == "main" {
        return Err(StorageError::corrupt("the main Branch cannot be deleted"));
    }
    if active_branch(connection)? == name {
        return Err(StorageError::corrupt("the active Branch cannot be deleted"));
    }
    let previous = lookup_named_ref(connection, "_lithograph_branches", name)?;
    if previous.is_none() {
        return Ok(None);
    }
    connection.execute(
        "DELETE FROM main._lithograph_branches WHERE name = ?1",
        [name],
    )?;
    Ok(previous)
}

pub fn move_branch_ref(
    connection: &Connection,
    name: &str,
    expected: Option<HashId>,
    target: HashId,
) -> StorageResult<Option<HashId>> {
    validate_ref_name(name)?;
    require_commit(connection, target)?;
    let previous = lookup_named_ref(connection, "_lithograph_branches", name)?;
    let Some(previous) = previous else {
        return Ok(None);
    };
    if expected.is_some_and(|value| value != previous) {
        return Err(StorageError::BranchHeadMoved);
    }
    let changed = connection.execute(
        "UPDATE main._lithograph_branches SET commit_id = ?2 WHERE name = ?1 AND commit_id = ?3",
        params![
            name,
            target.as_bytes().as_slice(),
            previous.as_bytes().as_slice()
        ],
    )?;
    if changed == 1 {
        Ok(Some(previous))
    } else {
        Err(StorageError::BranchHeadMoved)
    }
}

pub fn list_branches(connection: &Connection) -> StorageResult<Vec<NamedRef>> {
    list_named_refs(connection, "_lithograph_branches")
}

pub fn create_tag(connection: &Connection, name: &str, target: HashId) -> StorageResult<()> {
    validate_ref_name(name)?;
    require_commit(connection, target)?;
    connection.execute(
        "INSERT INTO main._lithograph_tags(name, commit_id) VALUES(?1, ?2)",
        params![name, target.as_bytes().as_slice()],
    )?;
    Ok(())
}

pub fn move_tag(
    connection: &Connection,
    name: &str,
    target: HashId,
) -> StorageResult<Option<HashId>> {
    validate_ref_name(name)?;
    require_commit(connection, target)?;
    let previous = lookup_named_ref(connection, "_lithograph_tags", name)?;
    let Some(previous) = previous else {
        return Ok(None);
    };
    connection.execute(
        "UPDATE main._lithograph_tags SET commit_id = ?2 WHERE name = ?1",
        params![name, target.as_bytes().as_slice()],
    )?;
    Ok(Some(previous))
}

pub fn delete_tag(connection: &Connection, name: &str) -> StorageResult<Option<HashId>> {
    validate_ref_name(name)?;
    let previous = lookup_named_ref(connection, "_lithograph_tags", name)?;
    if previous.is_none() {
        return Ok(None);
    }
    connection.execute("DELETE FROM main._lithograph_tags WHERE name = ?1", [name])?;
    Ok(previous)
}

pub fn list_tags(connection: &Connection) -> StorageResult<Vec<NamedRef>> {
    list_named_refs(connection, "_lithograph_tags")
}

pub fn resolve_version_descriptor(
    connection: &Connection,
    descriptor: &str,
) -> StorageResult<HashId> {
    let Some((kind, value)) = descriptor.split_once('/') else {
        return Err(StorageError::not_found(format!(
            "version descriptor {descriptor:?}"
        )));
    };
    if value.is_empty() {
        return Err(StorageError::not_found(format!(
            "version descriptor {descriptor:?}"
        )));
    }
    match kind {
        "branch" => super::super::branch_head(connection, value),
        "tag" => lookup_named_ref(connection, "_lithograph_tags", value)?
            .ok_or_else(|| StorageError::not_found(format!("tag {value:?}"))),
        "commit" if !value.contains('/') => {
            let commit = HashId::from_hex(value)?;
            require_commit(connection, commit)?;
            Ok(commit)
        }
        _ => Err(StorageError::not_found(format!(
            "version descriptor {descriptor:?}"
        ))),
    }
}

fn list_named_refs(connection: &Connection, table: &str) -> StorageResult<Vec<NamedRef>> {
    let sql = format!("SELECT name, commit_id FROM main.{table} ORDER BY name COLLATE BINARY");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut refs = Vec::new();
    for row in rows {
        let (name, bytes) = row?;
        refs.push(NamedRef {
            name,
            commit: HashId::from_slice(&bytes)?,
        });
    }
    Ok(refs)
}

fn lookup_named_ref(
    connection: &Connection,
    table: &str,
    name: &str,
) -> StorageResult<Option<HashId>> {
    let sql = format!("SELECT commit_id FROM main.{table} WHERE name = ?1");
    let bytes = connection
        .query_row(&sql, [name], |row| row.get::<_, Vec<u8>>(0))
        .optional()?;
    bytes.as_deref().map(HashId::from_slice).transpose()
}

pub(super) fn require_commit(connection: &Connection, commit: HashId) -> StorageResult<()> {
    if super::super::commit_exists(connection, commit)? {
        Ok(())
    } else {
        Err(StorageError::not_found(format!(
            "Commit {}",
            commit.to_hex()
        )))
    }
}
