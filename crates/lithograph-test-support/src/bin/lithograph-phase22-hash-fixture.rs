#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{
    CommitMetadata, HashId, LayerBuilder, OwnerKind, PropertyValue, STORAGE_FORMAT,
    allocate_node_id_range, branch_head, commit_layer, create_checkpoint, create_storage_schema,
    initialize_connection_state, initialize_root, intern_label, intern_property_key,
};
use rusqlite::Connection;
use serde_json::json;

#[path = "phase22_hash_fixture/oracle.rs"]
mod oracle;
#[path = "phase22_hash_fixture/verify.rs"]
mod verify;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MAX_NODES: i64 = 1_000_000;
const LAYER_NODES: i64 = 16_384;
const SMALL_ROWS: i64 = 24;
const BILATERAL_NODES: i64 = 200_002;
const BILATERAL_HOT_ROWS: i64 = 100_001;
const SEED: i64 = 0;

#[derive(Clone, Copy)]
enum Distribution {
    Uniform,
    Skew,
    BilateralSkew,
}

impl Distribution {
    fn name(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::Skew => "skew",
            Self::BilateralSkew => "bilateral-skew",
        }
    }

    fn hot_rows(self, nodes: i64) -> i64 {
        match self {
            Self::Uniform => 0,
            Self::Skew => nodes.min(100_000),
            Self::BilateralSkew => BILATERAL_HOT_ROWS,
        }
    }

    fn left_key(self, offset: i64, nodes: i64) -> i64 {
        match self {
            Self::BilateralSkew => oracle::bilateral_left_key(offset),
            _ => oracle::left_key(offset, self.hot_rows(nodes)),
        }
    }

    fn right_key(self, offset: i64) -> i64 {
        match self {
            Self::BilateralSkew => oracle::bilateral_right_key(offset),
            _ => offset + 1,
        }
    }

    fn is_small(self, offset: i64) -> bool {
        match self {
            Self::BilateralSkew => offset == 0 || (2..=SMALL_ROWS).contains(&offset),
            _ => offset < SMALL_ROWS,
        }
    }

    fn small_count(self, start: i64, end: i64) -> i64 {
        match self {
            Self::BilateralSkew => {
                let first = if start == 0 && end > 0 { 1 } else { 0 };
                first + (end.min(SMALL_ROWS + 1) - start.max(2)).max(0)
            }
            _ => end.min(SMALL_ROWS) - start.min(SMALL_ROWS),
        }
    }
}

struct Config {
    path: PathBuf,
    nodes: i64,
    distribution: Distribution,
}

#[derive(Clone, Copy)]
struct Ids {
    first_node: i64,
    left_label: i64,
    right_label: i64,
    small_label: i64,
    left_key: i64,
    right_key: i64,
}

struct Seeded {
    commit: HashId,
    schema_commit: HashId,
    ids: Ids,
    graph_commits: i64,
    layer_micros: u128,
    commit_micros: u128,
}

fn main() -> Result<()> {
    let config = arguments()?;
    let started = Instant::now();
    let cpu_started = process_cpu_nanos();
    reserve_new_path(&config.path)?;
    let mut connection = Connection::open(&config.path)?;
    let setup = Instant::now();
    let root = initialize(&mut connection)?;
    let setup_micros = setup.elapsed().as_micros();
    let schema = Instant::now();
    install_schema(&connection)?;
    let schema_micros = schema.elapsed().as_micros();
    let seeding = Instant::now();
    let seeded = seed(&mut connection, &config)?;
    let seed_micros = seeding.elapsed().as_micros();
    let checkpoint = Instant::now();
    create_checkpoint(&connection, seeded.commit)?;
    let checkpoint_micros = checkpoint.elapsed().as_micros();
    let verify = Instant::now();
    let counts = verify::fixture(&connection, &config, &seeded)?;
    let verify_micros = verify.elapsed().as_micros();
    let sqlite_source_id: String =
        connection.query_row("SELECT sqlite_source_id()", [], |row| row.get(0))?;
    connection.close().map_err(|(_, error)| error)?;
    let hashing = Instant::now();
    let hash = file_blake3(&config.path)?;
    let hash_micros = hashing.elapsed().as_micros();
    let report = json!({
        "kind":"phase22-hash-fixture-v1", "seed":SEED,
        "path":fs::canonicalize(&config.path)?.display().to_string(),
        "sqlite":rusqlite::version(), "sqlite_source_id":sqlite_source_id,
        "storage_format":STORAGE_FORMAT, "distribution":config.distribution.name(),
        "node_count":config.nodes, "relationship_count":0, "property_count":config.nodes*2,
        "first_node":seeded.ids.first_node, "last_node":seeded.ids.first_node+config.nodes-1,
        "hot_rows":config.distribution.hot_rows(config.nodes),
        "root":format!("commit/{}",root.to_hex()),
        "schema_commit":format!("commit/{}",seeded.schema_commit.to_hex()),
        "commit":format!("commit/{}",seeded.commit.to_hex()),
        "checkpoint_commit":format!("commit/{}",seeded.commit.to_hex()),
        "graph_commit_count":seeded.graph_commits, "graph_layer_nodes_max":LAYER_NODES,
        "label_ids":{"S5Left":seeded.ids.left_label,"S5Right":seeded.ids.right_label,"S5Small":seeded.ids.small_label},
        "property_ids":{"leftKey":seeded.ids.left_key,"rightKey":seeded.ids.right_key},
        "counts":counts, "property_indexes":0,
        "oracles":oracle::cases(config.nodes,seeded.ids.first_node,config.distribution),
        "oracle_basis":"all checkpoint node/label/property slots verified against deterministic offset recipe; canonical integrity verified",
        "database_bytes":fs::metadata(&config.path)?.len(), "database_blake3":hash,
        "micros":{"setup":setup_micros,"schema":schema_micros,"seed":seed_micros,
            "layer":seeded.layer_micros,"commit":seeded.commit_micros,
            "checkpoint":checkpoint_micros,"verify":verify_micros,"hash":hash_micros,
            "total":started.elapsed().as_micros(),"total_process_cpu":elapsed_cpu_micros(cpu_started)}
    });
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn arguments() -> Result<Config> {
    let mut args = std::env::args_os().skip(1);
    let usage = "usage: lithograph-phase22-hash-fixture <new-database-path> <nodes:1000..1000000> [--distribution uniform|skew|bilateral-skew]; bilateral-skew requires 200002 nodes";
    let path = args.next().map(PathBuf::from).ok_or(usage)?;
    let nodes = args
        .next()
        .ok_or(usage)?
        .into_string()
        .map_err(|_| usage)?
        .parse()?;
    let distribution = match args.next() {
        None => Distribution::Uniform,
        Some(flag) if flag == "--distribution" => {
            match args.next().as_deref().and_then(|s| s.to_str()) {
                Some("uniform") => Distribution::Uniform,
                Some("skew") => Distribution::Skew,
                Some("bilateral-skew") => Distribution::BilateralSkew,
                _ => return Err(usage.into()),
            }
        }
        Some(_) => return Err(usage.into()),
    };
    if args.next().is_some()
        || !(1000..=MAX_NODES).contains(&nodes)
        || (matches!(distribution, Distribution::BilateralSkew) && nodes != BILATERAL_NODES)
    {
        return Err(usage.into());
    }
    Ok(Config {
        path,
        nodes,
        distribution,
    })
}

fn reserve_new_path(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match fs::symlink_metadata(Path::new(&sidecar)) {
            Ok(_) => {
                return Err(
                    format!("refusing an existing SQLite sidecar: {}", path.display()).into(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
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

fn initialize(connection: &mut Connection) -> Result<HashId> {
    connection.execute_batch("PRAGMA cache_size=-2000")?;
    let transaction = connection.transaction()?;
    // Only standalone metadata bootstrap uses SQL. Graph, schema and ref changes
    // below use their real storage/query APIs and immutable Commit history.
    transaction.execute_batch(
        "CREATE TABLE main._lithograph_meta(\
         id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )?;
    transaction.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000002205',?1)",
        [STORAGE_FORMAT],
    )?;
    create_storage_schema(&transaction)?;
    let root = initialize_root(&transaction)?.root;
    initialize_connection_state(&transaction)?;
    transaction.commit()?;
    Ok(root)
}

fn install_schema(connection: &Connection) -> Result<()> {
    for (name, label, key) in [
        ("s5_left_integer", "S5Left", "leftKey"),
        ("s5_right_integer", "S5Right", "rightKey"),
    ] {
        let text =
            format!("CREATE CONSTRAINT {name} FOR (n:{label}) REQUIRE n.{key} IS :: INTEGER");
        let prepared = prepare(
            connection,
            &text,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )?;
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        loop {
            let batch = cursor.next_batch(connection, 16)?;
            if batch.done {
                drop(cursor.complete(connection)?);
                break;
            }
        }
        drop(cursor);
        if resources.used() != 0 {
            return Err("schema execution leaked QueryResources".into());
        }
    }
    Ok(())
}

fn seed(connection: &mut Connection, config: &Config) -> Result<Seeded> {
    let transaction = connection.transaction()?;
    let schema_commit = branch_head(&transaction, "main")?;
    let ids = Ids {
        first_node: allocate_node_id_range(&transaction, config.nodes as u64)?,
        left_label: intern_label(&transaction, "S5Left")?,
        right_label: intern_label(&transaction, "S5Right")?,
        small_label: intern_label(&transaction, "S5Small")?,
        left_key: intern_property_key(&transaction, "leftKey")?,
        right_key: intern_property_key(&transaction, "rightKey")?,
    };
    let mut parent = schema_commit;
    let mut layer_micros = 0;
    let mut commit_micros = 0;
    let mut graph_commits = 0;
    for start in (0..config.nodes).step_by(LAYER_NODES as usize) {
        let end = (start + LAYER_NODES).min(config.nodes);
        let started = Instant::now();
        let layer = graph_layer(config, ids, start, end)?;
        layer_micros += started.elapsed().as_micros();
        let started = Instant::now();
        parent = commit_layer(
            &transaction,
            "main",
            parent,
            None,
            &layer,
            &CommitMetadata {
                author: Some("phase22-hash-fixture".into()),
                message: Some(format!(
                    "dual-key-v1 {} nodes={}; offsets={start}..{end}; seed={SEED}",
                    config.distribution.name(),
                    config.nodes
                )),
                committed_at: graph_commits + 1,
            },
        )?;
        drop(layer);
        commit_micros += started.elapsed().as_micros();
        graph_commits += 1;
    }
    transaction.commit()?;
    Ok(Seeded {
        commit: parent,
        schema_commit,
        ids,
        graph_commits,
        layer_micros,
        commit_micros,
    })
}

fn graph_layer(config: &Config, ids: Ids, start: i64, end: i64) -> Result<LayerBuilder> {
    let mut layer = LayerBuilder::default();
    for offset in start..end {
        let node = ids.first_node + offset;
        layer.add_node(node)?;
        layer.add_label(node, ids.left_label)?;
        layer.add_label(node, ids.right_label)?;
        if config.distribution.is_small(offset) {
            layer.add_label(node, ids.small_label)?;
        }
        layer.set_property(
            OwnerKind::Node,
            node,
            ids.left_key,
            PropertyValue::Integer(config.distribution.left_key(offset, config.nodes)),
        )?;
        layer.set_property(
            OwnerKind::Node,
            node,
            ids.right_key,
            PropertyValue::Integer(config.distribution.right_key(offset)),
        )?;
    }
    let counts = layer.delta_counts();
    let nodes = (end - start) as u64;
    let small = config.distribution.small_count(start, end) as u64;
    if counts.nodes_created != nodes
        || counts.labels_added != nodes * 2 + small
        || counts.properties_set != nodes * 2
        || counts.relationships_created != 0
    {
        return Err("generated Layer dimensions differ".into());
    }
    Ok(layer)
}

fn file_blake3(path: &Path) -> Result<String> {
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_cpu_nanos() -> Option<u128> {
    let value = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    u128::try_from(value.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u128::try_from(value.tv_nsec).ok()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_cpu_nanos() -> Option<u128> {
    None
}

fn elapsed_cpu_micros(started: Option<u128>) -> Option<u128> {
    process_cpu_nanos()?
        .checked_sub(started?)
        .map(|nanos| nanos / 1000)
}
