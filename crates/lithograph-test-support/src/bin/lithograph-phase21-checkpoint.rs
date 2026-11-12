use std::cell::Cell;
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use lithograph_core::performance;
use lithograph_core::storage::{branch_head, create_checkpoint};
use rusqlite::{
    Connection, StatementStatus,
    trace::{TraceEvent, TraceEventCodes},
};
use serde_json::json;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: lithograph-phase21-checkpoint <database> [branch]")?;
    let branch = args
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "main".to_owned());
    if args.next().is_some() {
        return Err("usage: lithograph-phase21-checkpoint <database> [branch]".into());
    }

    let connection = Connection::open(&path)?;
    let commit = branch_head(&connection, &branch)?;
    performance::set_enabled(true);
    performance::reset();
    let (build_micros, sqlite_work) = measured_build(&connection, commit)?;
    let work = performance::snapshot();
    let commit_bytes = commit.as_bytes();
    let checkpoint_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_checkpoints WHERE commit_id = ?1",
        [commit_bytes.as_slice()],
        |row| row.get(0),
    )?;
    let node_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_nodes WHERE commit_id = ?1",
        [commit_bytes.as_slice()],
        |row| row.get(0),
    )?;
    let relationship_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_relationships WHERE commit_id = ?1",
        [commit_bytes.as_slice()],
        |row| row.get(0),
    )?;
    if checkpoint_count != 1 {
        return Err(format!("expected one checkpoint for {branch}, got {checkpoint_count}").into());
    }

    println!(
        "{}",
        json!({
            "branch": branch,
            "commit": format!("commit/{}", commit.to_hex()),
            "checkpoint_count": checkpoint_count,
            "node_count": node_count,
            "relationship_count": relationship_count,
            "build_micros": build_micros,
            "builds": work.checkpoint_builds,
            "layers_replayed": work.checkpoint_layers_replayed,
            "ancestor_copies": work.checkpoint_ancestor_copies,
            "rows_copied": work.checkpoint_rows_copied,
            "sqlite": rusqlite::version(),
            "build_vm_steps": sqlite_work.vm_steps,
            "build_fullscan_steps": sqlite_work.fullscan_steps,
            "build_sorts": sqlite_work.sorts,
            "build_statements": sqlite_work.statements,
        })
    );
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct SqliteWork {
    statements: u64,
    vm_steps: u64,
    fullscan_steps: u64,
    sorts: u64,
    invalid: bool,
}

thread_local! {
    // The maintenance tool runs one synchronous connection. Nested execution
    // is rejected as an invalid measurement instead of guessing its VM work.
    static SQL_WORK: Cell<SqliteWork> = Cell::default();
    static SQL_BASELINE: Cell<Option<[i32; 3]>> = const { Cell::new(None) };
}

fn measured_build(
    connection: &Connection,
    commit: lithograph_core::storage::HashId,
) -> Result<(u128, SqliteWork), Box<dyn Error>> {
    SQL_WORK.set(SqliteWork::default());
    SQL_BASELINE.set(None);
    connection.trace_v2(
        TraceEventCodes::SQLITE_TRACE_STMT | TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(trace_work),
    );
    let started = Instant::now();
    let result = create_checkpoint(connection, commit);
    let elapsed = started.elapsed().as_micros();
    connection.trace_v2(TraceEventCodes::empty(), None);
    result?;
    let work = SQL_WORK.get();
    if work.invalid || SQL_BASELINE.get().is_some() {
        return Err("invalid checkpoint VM trace".into());
    }
    Ok((elapsed, work))
}

fn trace_work(event: TraceEvent<'_>) {
    let (statement, starting) = match event {
        TraceEvent::Stmt(statement, _) => (statement, true),
        TraceEvent::Profile(statement, _) => (statement, false),
        _ => return,
    };
    let sample = [
        StatementStatus::VmStep,
        StatementStatus::FullscanStep,
        StatementStatus::Sort,
    ]
    .map(|status| statement.get_status(status));
    let mut work = SQL_WORK.get();
    if starting {
        work.invalid |= SQL_BASELINE.replace(Some(sample)).is_some();
    } else if let Some(before) = SQL_BASELINE.take() {
        work.statements = work.statements.saturating_add(1);
        for ((after, before), counter) in sample.into_iter().zip(before).zip([
            &mut work.vm_steps,
            &mut work.fullscan_steps,
            &mut work.sorts,
        ]) {
            match after
                .checked_sub(before)
                .and_then(|n| u64::try_from(n).ok())
                .and_then(|n| counter.checked_add(n))
            {
                Some(next) => *counter = next,
                None => work.invalid = true,
            }
        }
    } else {
        work.invalid = true;
    }
    SQL_WORK.set(work);
}
