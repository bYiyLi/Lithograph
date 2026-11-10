use std::error::Error;
use std::path::PathBuf;

use lithograph_core::storage::{branch_head, create_checkpoint};
use rusqlite::Connection;
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
    create_checkpoint(&connection, commit)?;
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
        })
    );
    Ok(())
}
