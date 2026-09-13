use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::super::{HashId, StorageError, StorageResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSessionRecord {
    pub id: String,
    pub target_branch: String,
    pub ours: HashId,
    pub theirs: HashId,
    pub revision: i64,
    pub created_at: i64,
}

pub fn create_merge_session(
    connection: &Connection,
    target_branch: &str,
    ours: HashId,
    theirs: HashId,
    expected_head: Option<HashId>,
    created_at: i64,
) -> StorageResult<MergeSessionRecord> {
    let id = generate_session_id()?;
    // The INSERT obtains SQLite writer ownership before the pinned Commit/ref
    // rechecks below. Any error is rolled back by the enclosing query savepoint.
    connection.execute(
        "INSERT INTO main._lithograph_merge_sessions(id, target_branch, ours_commit, theirs_commit, revision, created_at) VALUES(?1, ?2, ?3, ?4, 1, ?5)",
        params![
            id,
            target_branch,
            ours.as_bytes().as_slice(),
            theirs.as_bytes().as_slice(),
            created_at,
        ],
    )?;
    if !super::super::commit_exists(connection, ours)?
        || !super::super::commit_exists(connection, theirs)?
    {
        return Err(StorageError::not_found("pinned merge Commit"));
    }
    if let Some(expected) = expected_head
        && (super::super::branch_head(connection, target_branch)? != expected || expected != ours)
    {
        return Err(StorageError::BranchHeadMoved);
    }
    Ok(MergeSessionRecord {
        id,
        target_branch: target_branch.to_owned(),
        ours,
        theirs,
        revision: 1,
        created_at,
    })
}

pub fn load_merge_session(
    connection: &Connection,
    id: &str,
) -> StorageResult<Option<MergeSessionRecord>> {
    let row = connection
        .query_row(
            "SELECT target_branch, ours_commit, theirs_commit, revision, created_at FROM main._lithograph_merge_sessions WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        Ok(MergeSessionRecord {
            id: id.to_owned(),
            target_branch: row.0,
            ours: HashId::from_slice(&row.1)?,
            theirs: HashId::from_slice(&row.2)?,
            revision: row.3,
            created_at: row.4,
        })
    })
    .transpose()
}

pub fn list_merge_sessions_after(
    connection: &Connection,
    after: Option<&str>,
    limit: usize,
) -> StorageResult<Vec<MergeSessionRecord>> {
    let mut statement = connection.prepare(
        "SELECT id, target_branch, ours_commit, theirs_commit, revision, created_at FROM main._lithograph_merge_sessions WHERE (?1 IS NULL OR id > ?1) ORDER BY id COLLATE BINARY LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    )?;
    let mut sessions = Vec::new();
    for row in rows {
        let row = row?;
        sessions.push(MergeSessionRecord {
            id: row.0,
            target_branch: row.1,
            ours: HashId::from_slice(&row.2)?,
            theirs: HashId::from_slice(&row.3)?,
            revision: row.4,
            created_at: row.5,
        });
    }
    Ok(sessions)
}

pub fn load_merge_resolutions(
    connection: &Connection,
    session: &str,
) -> StorageResult<BTreeMap<HashId, String>> {
    let mut statement = connection.prepare(
        "SELECT conflict_id, resolution_json FROM main._lithograph_merge_resolutions WHERE session_id = ?1 ORDER BY conflict_id",
    )?;
    let rows = statement.query_map([session], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut resolutions = BTreeMap::new();
    for row in rows {
        let (id, resolution) = row?;
        resolutions.insert(HashId::from_slice(&id)?, resolution);
    }
    Ok(resolutions)
}

pub fn update_merge_resolutions(
    connection: &Connection,
    session: &str,
    expected_revision: i64,
    replacements: &BTreeMap<HashId, String>,
    changed: bool,
) -> StorageResult<i64> {
    // A no-op CAS obtains writer ownership and fixes the precedence/race
    // boundary before resolution rows are changed.
    let touched = connection.execute(
        "UPDATE main._lithograph_merge_sessions SET revision = revision WHERE id = ?1 AND revision = ?2",
        params![session, expected_revision],
    )?;
    if touched != 1 {
        if load_merge_session(connection, session)?.is_none() {
            return Err(StorageError::not_found(format!(
                "Merge Session {session:?}"
            )));
        }
        return Err(StorageError::BranchHeadMoved);
    }
    for (conflict_id, resolution) in replacements {
        connection.execute(
            "INSERT INTO main._lithograph_merge_resolutions(session_id, conflict_id, resolution_json) VALUES(?1, ?2, ?3) ON CONFLICT(session_id, conflict_id) DO UPDATE SET resolution_json = excluded.resolution_json",
            params![session, conflict_id.as_bytes().as_slice(), resolution],
        )?;
    }
    if !changed {
        return Ok(expected_revision);
    }
    let changed_rows = connection.execute(
        "UPDATE main._lithograph_merge_sessions SET revision = revision + 1 WHERE id = ?1 AND revision = ?2",
        params![session, expected_revision],
    )?;
    if changed_rows != 1 {
        return Err(StorageError::BranchHeadMoved);
    }
    Ok(expected_revision + 1)
}

pub fn delete_merge_session(
    connection: &Connection,
    session: &str,
    expected_revision: i64,
) -> StorageResult<bool> {
    let changed = connection.execute(
        "DELETE FROM main._lithograph_merge_sessions WHERE id = ?1 AND revision = ?2",
        params![session, expected_revision],
    )?;
    if changed == 0 {
        if load_merge_session(connection, session)?.is_none() {
            return Ok(false);
        }
        return Err(StorageError::BranchHeadMoved);
    }
    connection.execute(
        "DELETE FROM main._lithograph_merge_resolutions WHERE session_id = ?1",
        [session],
    )?;
    Ok(true)
}

pub fn merge_session_roots(connection: &Connection) -> StorageResult<Vec<HashId>> {
    let mut statement = connection.prepare(
        "SELECT ours_commit FROM main._lithograph_merge_sessions UNION SELECT theirs_commit FROM main._lithograph_merge_sessions",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    rows.map(|row| HashId::from_slice(&row?)).collect()
}

fn generate_session_id() -> StorageResult<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        StorageError::corrupt(format!("failed to generate Merge Session id: {error}"))
    })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let value = u128::from_be_bytes(bytes);
    Ok(format!(
        "merge-session/{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (value >> 96) as u32,
        ((value >> 80) & 0xffff) as u16,
        ((value >> 64) & 0xffff) as u16,
        ((value >> 48) & 0xffff) as u16,
        value & 0x0000_ffff_ffff_ffff,
    ))
}
