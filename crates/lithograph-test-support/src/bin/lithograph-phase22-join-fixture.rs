#![forbid(unsafe_code)]

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use lithograph_core::storage::{
    CommitMetadata, HashId, LayerBuilder, RelationshipRecord, STORAGE_FORMAT,
    allocate_node_id_range, allocate_relationship_id_range, branch_head, commit_layer,
    create_checkpoint, create_storage_schema, initialize_connection_state, initialize_root,
    integrity_check, intern_label, intern_relationship_type,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const MAX_NODES: i64 = 1_000_000;
const SEED: i64 = 0;

#[derive(Clone, Copy)]
struct Ids {
    first_node: i64,
    first_relationship: i64,
    chain: i64,
    rare: i64,
    next: i64,
}

struct Seeded {
    root: HashId,
    commit: HashId,
    ids: Ids,
    layer_micros: u128,
    commit_micros: u128,
}

fn main() -> Result<(), Box<dyn Error>> {
    let (path, nodes) = arguments()?;
    let started = Instant::now();
    reserve_new_path(&path)?;
    let mut connection = Connection::open(&path)?;
    let setup = Instant::now();
    initialize(&mut connection)?;
    let setup_micros = setup.elapsed().as_micros();
    let seeded = seed(&mut connection, nodes)?;
    let checkpoint = Instant::now();
    create_checkpoint(&connection, seeded.commit)?;
    let checkpoint_micros = checkpoint.elapsed().as_micros();
    let verify = Instant::now();
    let counts = verify_fixture(&connection, nodes, &seeded)?;
    let verify_micros = verify.elapsed().as_micros();
    connection.close().map_err(|(_, error)| error)?;
    let hashing = Instant::now();
    let hash = file_blake3(&path)?;
    let hash_micros = hashing.elapsed().as_micros();
    let report = json!({
        "kind": "phase22_join_cycle_fixture", "generator": "s4-directed-cycle-v1", "seed": SEED,
        "path": fs::canonicalize(&path)?.display().to_string(), "sqlite": rusqlite::version(), "storage_format": STORAGE_FORMAT,
        "node_count": nodes, "relationship_count": nodes, "properties": 0,
        "root": format!("commit/{}", seeded.root.to_hex()),
        "commit": format!("commit/{}", seeded.commit.to_hex()),
        "checkpoint_commit": format!("commit/{}", seeded.commit.to_hex()),
        "first_node": seeded.ids.first_node, "last_node": seeded.ids.first_node + nodes - 1,
        "rare_node": seeded.ids.first_node,
        "first_relationship": seeded.ids.first_relationship,
        "last_relationship": seeded.ids.first_relationship + nodes - 1,
        "label_ids": {"S4Chain": seeded.ids.chain, "S4Rare": seeded.ids.rare},
        "type_ids": {"S4Next": seeded.ids.next}, "counts": counts,
        "database_bytes": fs::metadata(&path)?.len(), "database_blake3": hash,
        "oracle_basis": "complete checkpoint topology and canonical integrity checks",
        "oracles": oracles(nodes, seeded.ids),
        "micros": {"setup": setup_micros, "layer": seeded.layer_micros,
            "commit": seeded.commit_micros, "checkpoint": checkpoint_micros,
            "verify": verify_micros, "hash": hash_micros,
            "total": started.elapsed().as_micros()}
    });
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn arguments() -> Result<(PathBuf, i64), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let usage = "usage: lithograph-phase22-join-fixture <new-database-path> <nodes:8..1000000>";
    let path = args.next().map(PathBuf::from).ok_or(usage)?;
    let nodes = args
        .next()
        .ok_or(usage)?
        .into_string()
        .map_err(|_| usage)?
        .parse()?;
    if args.next().is_some() || !(8..=MAX_NODES).contains(&nodes) {
        return Err(usage.into());
    }
    Ok((path, nodes))
}

fn reserve_new_path(path: &Path) -> Result<(), Box<dyn Error>> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        if Path::new(&sidecar).exists() {
            return Err(format!("refusing an existing SQLite sidecar: {}", path.display()).into());
        }
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(())
}

fn initialize(connection: &mut Connection) -> Result<(), Box<dyn Error>> {
    connection.execute_batch("PRAGMA cache_size=-2000")?;
    let transaction = connection.transaction()?;
    // Match the existing standalone storage fixtures' SQL metadata bootstrap.
    // Graph, Schema, Layer, Commit and Branch writes use storage APIs below.
    transaction.execute_batch(
        "CREATE TABLE main._lithograph_meta(\
            id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
            database_id TEXT NOT NULL, storage_format INTEGER NOT NULL)",
    )?;
    transaction.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
            '00000000-0000-4000-8000-000000002204',?1)",
        [STORAGE_FORMAT],
    )?;
    create_storage_schema(&transaction)?;
    initialize_root(&transaction)?;
    initialize_connection_state(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn seed(connection: &mut Connection, nodes: i64) -> Result<Seeded, Box<dyn Error>> {
    let transaction = connection.transaction()?;
    let root = branch_head(&transaction, "main")?;
    let ids = Ids {
        first_node: allocate_node_id_range(&transaction, nodes as u64)?,
        first_relationship: allocate_relationship_id_range(&transaction, nodes as u64)?,
        chain: intern_label(&transaction, "S4Chain")?,
        rare: intern_label(&transaction, "S4Rare")?,
        next: intern_relationship_type(&transaction, "S4Next")?,
    };
    let started = Instant::now();
    let layer = cycle_layer(nodes, ids)?;
    let layer_micros = started.elapsed().as_micros();
    let started = Instant::now();
    let commit = commit_layer(
        &transaction,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase22-join-fixture".into()),
            message: Some(format!("directed-cycle-v1 nodes={nodes} seed={SEED}")),
            committed_at: 1,
        },
    )?;
    drop(layer);
    transaction.commit()?;
    let commit_micros = started.elapsed().as_micros();
    Ok(Seeded {
        root,
        commit,
        ids,
        layer_micros,
        commit_micros,
    })
}

fn cycle_layer(nodes: i64, ids: Ids) -> Result<LayerBuilder, Box<dyn Error>> {
    let mut layer = LayerBuilder::default();
    for offset in 0..nodes {
        let node = ids.first_node + offset;
        layer.add_node(node)?;
        layer.add_label(node, ids.chain)?;
        layer.add_relationship(RelationshipRecord {
            id: ids.first_relationship + offset,
            source: node,
            type_id: ids.next,
            target: if offset + 1 == nodes {
                ids.first_node
            } else {
                node + 1
            },
        })?;
    }
    layer.add_label(ids.first_node, ids.rare)?;
    let counts = layer.delta_counts();
    if counts.nodes_created != nodes as u64
        || counts.relationships_created != nodes as u64
        || counts.labels_added != nodes as u64 + 1
        || counts.properties_set != 0
    {
        return Err("cycle Layer dimensions differ".into());
    }
    Ok(layer)
}

fn verify_fixture(
    connection: &Connection,
    nodes: i64,
    seeded: &Seeded,
) -> Result<Value, Box<dyn Error>> {
    let head = branch_head(connection, "main")?;
    if head != seeded.commit {
        return Err("checkpoint moved the Branch head".into());
    }
    let node_ids: (i64, i64, i64) = connection.query_row(
        "SELECT count(*),min(node_id),max(node_id) FROM main._lithograph_cp_nodes WHERE commit_id=?1",
        [head.as_bytes().as_slice()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if node_ids
        != (
            nodes,
            seeded.ids.first_node,
            seeded.ids.first_node + nodes - 1,
        )
    {
        return Err("checkpoint Node IDs/count differ".into());
    }
    verify_labels(connection, nodes, seeded)?;
    let relationship_count = verify_relationships(connection, nodes, seeded)?;
    let mut counts = verify_history(connection)?;
    counts["nodes"] = json!(node_ids.0);
    counts["relationships"] = json!(relationship_count);
    counts["labels"] = json!(nodes + 1);
    Ok(counts)
}

fn verify_history(connection: &Connection) -> Result<Value, Box<dyn Error>> {
    let (commits, checkpoints, properties, user_version): (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM main._lithograph_commits),\
            (SELECT count(*) FROM main._lithograph_checkpoints),\
            (SELECT count(*) FROM main._lithograph_cp_properties),\
            (SELECT user_version FROM pragma_user_version)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    if (commits, checkpoints, properties, user_version) != (2, 1, 0, 0) {
        return Err("unexpected fixture history, checkpoint, Property or user_version".into());
    }
    let quick_check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    let issues = integrity_check(connection)?;
    if quick_check != "ok" || !issues.is_empty() {
        return Err(format!("fixture integrity failed: {quick_check}; {issues:?}").into());
    }
    Ok(
        json!({"commits":commits,"checkpoints":checkpoints,"properties":properties,
        "user_version":user_version,"quick_check":quick_check,"integrity_issues":issues.len()}),
    )
}

fn verify_labels(
    connection: &Connection,
    nodes: i64,
    seeded: &Seeded,
) -> Result<(), Box<dyn Error>> {
    let counts: (i64, i64, i64, i64) = connection.query_row(
        "SELECT count(*),sum(label_id=?2),sum(label_id=?3),\
            sum(label_id=?3 AND node_id=?4) FROM main._lithograph_cp_labels WHERE commit_id=?1",
        params![
            seeded.commit.as_bytes().as_slice(),
            seeded.ids.chain,
            seeded.ids.rare,
            seeded.ids.first_node
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if counts != (nodes + 1, nodes, 1, 1) {
        return Err("checkpoint S4Chain/S4Rare labels differ".into());
    }
    Ok(())
}

fn verify_relationships(
    connection: &Connection,
    nodes: i64,
    seeded: &Seeded,
) -> Result<i64, Box<dyn Error>> {
    let (count, wrong): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(NOT(\
            relationship_id BETWEEN ?2 AND ?2+?4-1 AND type_id=?5 \
            AND source_id=?3+relationship_id-?2 \
            AND target_id=CASE WHEN relationship_id=?2+?4-1 THEN ?3 ELSE source_id+1 END)),0) \
         FROM main._lithograph_cp_relationships WHERE commit_id=?1",
        params![
            seeded.commit.as_bytes().as_slice(),
            seeded.ids.first_relationship,
            seeded.ids.first_node,
            nodes,
            seeded.ids.next
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count != nodes || wrong != 0 {
        return Err("checkpoint Relationships do not form the complete directed cycle".into());
    }
    Ok(count)
}

fn oracles(nodes: i64, ids: Ids) -> Vec<Value> {
    (3..=8)
        .map(|parts| {
            let patterns = (0..parts)
                .map(|part| {
                    let end = if part + 1 == parts {
                        "S4Rare"
                    } else {
                        "S4Chain"
                    };
                    format!("(n{part}:S4Chain)-[r{part}:S4Next]->(n{}:{end})", part + 1)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let columns = (0..=parts)
                .map(|n| format!("elementId(n{n})"))
                .chain((0..parts).map(|r| format!("elementId(r{r})")))
                .collect::<Vec<_>>();
            let node_ids = (nodes - parts..nodes)
                .chain(std::iter::once(0))
                .map(|offset| format!("n:{}", ids.first_node + offset))
                .collect::<Vec<_>>();
            let relationship_ids = (nodes - parts..nodes)
                .map(|offset| format!("r:{}", ids.first_relationship + offset))
                .collect::<Vec<_>>();
            let row = node_ids
                .iter()
                .chain(&relationship_ids)
                .cloned()
                .collect::<Vec<_>>();
            json!({"parts":parts,"query":format!("MATCH {patterns} RETURN {}",columns.join(", ")),
            "columns":columns,"node_element_ids":node_ids,
            "relationship_element_ids":relationship_ids,"rows":[row]})
        })
        .collect()
}

fn file_blake3(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut file = fs::File::open(path)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        hash.update(&buffer[..bytes]);
    }
    Ok(hash.finalize().to_hex().to_string())
}
