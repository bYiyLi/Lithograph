use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use lithograph_core::cypher::Value;
use lithograph_core::performance;
use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{
    branch_head, collect_garbage, create_branch, create_tag, delete_branch_ref, delete_tag,
    load_commit, resolve_version_descriptor,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ExtraScaleReport {
    database: String,
    conflict_count: u64,
    page_size: usize,
    merge_start_millis: u128,
    conflict_scan_millis: u128,
    resolution_millis: u128,
    candidate_inspection_millis: u128,
    finalize_millis: u128,
    finalize_prepare_micros: u64,
    finalize_writer_wait_micros: u64,
    finalize_writer_hold_micros: u64,
    conflict_pages_seen: u64,
    resolution_rounds: u64,
    finalized_commit: String,
    gc_root_millis: u128,
    gc_root_preserved: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let database = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: lithograph-phase10-scale-extra <database-path>")?;
    let conflict_count = env_u64("LITHOGRAPH_SCALE_CONFLICTS", 10_000)?;
    let page_size = usize::try_from(env_u64("LITHOGRAPH_SCALE_CONFLICT_PAGE", 256)?)?;
    if conflict_count == 0 || page_size == 0 {
        return Err("conflict count and page size must both be positive".into());
    }
    let connection = Connection::open(&database)?;
    let report = run_large_merge_and_gc(&connection, &database, conflict_count, page_size)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_large_merge_and_gc(
    connection: &Connection,
    database: &std::path::Path,
    conflict_count: u64,
    page_size: usize,
) -> Result<ExtraScaleReport, Box<dyn Error>> {
    seed_merge_conflicts(connection, conflict_count)?;
    let (session, revision, merge_start_millis) =
        start_conflicted_merge(connection, conflict_count)?;
    let (conflict_pages_seen, conflict_scan_millis) =
        verify_conflict_pagination(connection, &session, page_size, conflict_count)?;
    let (revision, resolution_rounds, resolution_millis) =
        resolve_all_conflicts(connection, &session, revision, page_size)?;
    let candidate_inspection_millis = inspect_candidate(connection, &session, revision)?;
    let (finalized_commit, finalize_millis, finalize_counters) =
        finalize_merge(connection, &session, revision)?;
    let (gc_root_millis, gc_root_preserved) = verify_tag_gc_root(connection)?;

    Ok(ExtraScaleReport {
        database: database.display().to_string(),
        conflict_count,
        page_size,
        merge_start_millis,
        conflict_scan_millis,
        resolution_millis,
        candidate_inspection_millis,
        finalize_millis,
        finalize_prepare_micros: finalize_counters.merge_finalize_prepare_micros,
        finalize_writer_wait_micros: finalize_counters.merge_finalize_writer_wait_micros,
        finalize_writer_hold_micros: finalize_counters.merge_finalize_writer_hold_micros,
        conflict_pages_seen,
        resolution_rounds,
        finalized_commit,
        gc_root_millis,
        gc_root_preserved,
    })
}

fn seed_merge_conflicts(
    connection: &Connection,
    conflict_count: u64,
) -> Result<(), Box<dyn Error>> {
    let count = i64::try_from(conflict_count)?;
    execute(
        connection,
        &format!("UNWIND range(1,{count}) AS id CREATE (:ScaleMerge {{id:id, value:0}}) FINISH"),
        ExecutionOptions::default(),
    )?;
    let base = branch_head(connection, "main")?;
    let _ = delete_branch_ref(connection, "phase10-scale-merge-source");
    create_branch(connection, "phase10-scale-merge-source", base)?;

    execute(
        connection,
        "MATCH (n:ScaleMerge) SET n.value=1 FINISH",
        ExecutionOptions::default(),
    )?;
    execute(
        connection,
        "MATCH (n:ScaleMerge) SET n.value=2 FINISH",
        ExecutionOptions::parse_text(r#"{"branch":"phase10-scale-merge-source"}"#)?,
    )?;
    Ok(())
}

fn start_conflicted_merge(
    connection: &Connection,
    conflict_count: u64,
) -> Result<(String, i64, u128), Box<dyn Error>> {
    let count = i64::try_from(conflict_count)?;
    let started = Instant::now();
    let rows = execute(
        connection,
        "CALL lithograph.merge.start('branch/phase10-scale-merge-source') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved",
        ExecutionOptions::default(),
    )?;
    let merge_start_millis = started.elapsed().as_millis();
    let row = rows.first().ok_or("merge.start returned no row")?;
    let session = string(row.first(), "session")?.to_owned();
    let revision = integer(row.get(1), "revision")?;
    if string(row.get(2), "status")? != "conflicted" {
        return Err(format!("merge.start did not report conflicted: {row:?}").into());
    }
    if integer(row.get(3), "unresolved")? != count {
        return Err(format!("merge.start unresolved count differs: {row:?}").into());
    }
    Ok((session, revision, merge_start_millis))
}

fn verify_conflict_pagination(
    connection: &Connection,
    session: &str,
    page_size: usize,
    conflict_count: u64,
) -> Result<(u64, u128), Box<dyn Error>> {
    let started = Instant::now();
    let (pages, conflicts) = scan_conflicts(connection, session, page_size)?;
    let conflict_scan_millis = started.elapsed().as_millis();
    if conflicts != conflict_count {
        return Err(
            format!("conflict pagination returned {conflicts}, expected {conflict_count}").into(),
        );
    }
    Ok((pages, conflict_scan_millis))
}

fn resolve_all_conflicts(
    connection: &Connection,
    session: &str,
    mut revision: i64,
    page_size: usize,
) -> Result<(i64, u64, u128), Box<dyn Error>> {
    let started = Instant::now();
    let mut resolution_rounds = 0_u64;
    loop {
        let unresolved = unresolved_conflict_page(connection, session, page_size)?;
        if unresolved.is_empty() {
            break;
        }
        let resolutions = unresolved
            .iter()
            .map(|conflict| format!("{{conflictId:'{conflict}', choice:'ours'}}"))
            .collect::<Vec<_>>()
            .join(",");
        let rows = execute(
            connection,
            &format!(
                "CALL lithograph.merge.resolve('{session}', {revision}, [{resolutions}]) YIELD revision, unresolved RETURN revision, unresolved"
            ),
            ExecutionOptions::default(),
        )?;
        revision = integer(
            rows.first().and_then(|row| row.first()),
            "resolved revision",
        )?;
        resolution_rounds += 1;
    }
    let resolution_millis = started.elapsed().as_millis();
    if resolution_rounds < 2 {
        return Err("large Merge Session must require multiple resolution rounds".into());
    }
    Ok((revision, resolution_rounds, resolution_millis))
}

fn inspect_candidate(
    connection: &Connection,
    session: &str,
    revision: i64,
) -> Result<u128, Box<dyn Error>> {
    let started = Instant::now();
    let candidate = execute(
        connection,
        "MATCH (n:ScaleMerge) WHERE n.id <= 10 RETURN n.value ORDER BY n.id",
        ExecutionOptions::parse_text(&format!(
            r#"{{"mergeSession":{{"id":"{session}","revision":{revision}}}}}"#
        ))?,
    )?;
    let candidate_inspection_millis = started.elapsed().as_millis();
    if candidate.len() != 10
        || candidate
            .iter()
            .any(|row| row.first() != Some(&Value::Integer(1)))
    {
        return Err(
            format!("candidate inspection returned unexpected values: {candidate:?}").into(),
        );
    }
    Ok(candidate_inspection_millis)
}

fn finalize_merge(
    connection: &Connection,
    session: &str,
    revision: i64,
) -> Result<(String, u128, performance::PerformanceCounters), Box<dyn Error>> {
    let commit_count_before: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    performance::reset();
    performance::set_enabled(true);
    let started = Instant::now();
    let finalized = execute(
        connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
        ),
        ExecutionOptions::default(),
    )?;
    let finalize_millis = started.elapsed().as_millis();
    let counters = performance::snapshot();
    performance::set_enabled(false);
    let finalized_row = finalized.first().ok_or("merge.finalize returned no row")?;
    if string(finalized_row.first(), "finalize status")? != "merged" {
        return Err(format!("merge.finalize status differs: {finalized_row:?}").into());
    }
    let finalized_commit = string(finalized_row.get(1), "finalized commit")?.to_owned();
    let commit_count_after: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    if commit_count_after != commit_count_before + 1 {
        return Err(format!(
            "merge.finalize created {} commits instead of one",
            commit_count_after - commit_count_before
        )
        .into());
    }
    Ok((finalized_commit, finalize_millis, counters))
}

fn verify_tag_gc_root(connection: &Connection) -> Result<(u128, bool), Box<dyn Error>> {
    let started = Instant::now();
    let gc_base = branch_head(connection, "main")?;
    let _ = delete_branch_ref(connection, "phase10-scale-gc");
    let _ = delete_tag(connection, "phase10-scale-gc-root");
    create_branch(connection, "phase10-scale-gc", gc_base)?;
    execute(
        connection,
        "CREATE (:ScaleGcRoot {kept:true}) FINISH",
        ExecutionOptions::parse_text(r#"{"branch":"phase10-scale-gc"}"#)?,
    )?;
    let pinned = branch_head(connection, "phase10-scale-gc")?;
    create_tag(connection, "phase10-scale-gc-root", pinned)?;
    delete_branch_ref(connection, "phase10-scale-gc")?;
    collect_garbage(connection)?;
    let resolved = resolve_version_descriptor(connection, "tag/phase10-scale-gc-root")?;
    let gc_root_preserved = resolved == pinned && load_commit(connection, pinned).is_ok();
    let gc_root_millis = started.elapsed().as_millis();
    if !gc_root_preserved {
        return Err("Tag did not preserve an otherwise unreachable Commit as a GC root".into());
    }
    Ok((gc_root_millis, gc_root_preserved))
}

fn scan_conflicts(
    connection: &Connection,
    session: &str,
    page_size: usize,
) -> Result<(u64, u64), Box<dyn Error>> {
    let mut cursor: Option<String> = None;
    let mut pages = 0_u64;
    let mut conflicts = 0_u64;
    loop {
        let query = match cursor.as_deref() {
            Some(cursor) => format!(
                "CALL lithograph.merge.conflicts('{session}', {page_size}, '{cursor}') YIELD conflictId, cursor RETURN conflictId, cursor"
            ),
            None => format!(
                "CALL lithograph.merge.conflicts('{session}', {page_size}) YIELD conflictId, cursor RETURN conflictId, cursor"
            ),
        };
        let rows = execute(connection, &query, ExecutionOptions::default())?;
        if rows.len() > page_size {
            return Err("merge.conflicts exceeded the requested page size".into());
        }
        if rows.is_empty() {
            break;
        }
        pages += 1;
        conflicts += rows.len() as u64;
        cursor = rows
            .last()
            .and_then(|row| row.get(1))
            .and_then(value_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok((pages, conflicts))
}

fn unresolved_conflict_page(
    connection: &Connection,
    session: &str,
    page_size: usize,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut cursor: Option<String> = None;
    loop {
        let query = match cursor.as_deref() {
            Some(cursor) => format!(
                "CALL lithograph.merge.conflicts('{session}', {page_size}, '{cursor}') YIELD conflictId, resolution, cursor RETURN conflictId, resolution, cursor"
            ),
            None => format!(
                "CALL lithograph.merge.conflicts('{session}', {page_size}) YIELD conflictId, resolution, cursor RETURN conflictId, resolution, cursor"
            ),
        };
        let rows = execute(connection, &query, ExecutionOptions::default())?;
        let unresolved = rows
            .iter()
            .filter(|row| matches!(row.get(1), Some(Value::Null)))
            .map(|row| string(row.first(), "conflictId").map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
        if !unresolved.is_empty() {
            return Ok(unresolved);
        }
        cursor = rows
            .last()
            .and_then(|row| row.get(2))
            .and_then(value_string);
        if cursor.is_none() {
            return Ok(Vec::new());
        }
    }
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, lithograph_core::query::QueryError> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 256)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        _ => None,
    }
}

fn string<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str, Box<dyn Error>> {
    match value {
        Some(Value::String(value)) => Ok(value),
        other => Err(format!("{field} is not a String: {other:?}").into()),
    }
}

fn integer(value: Option<&Value>, field: &str) -> Result<i64, Box<dyn Error>> {
    match value {
        Some(Value::Integer(value)) => Ok(*value),
        other => Err(format!("{field} is not an Integer: {other:?}").into()),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}
