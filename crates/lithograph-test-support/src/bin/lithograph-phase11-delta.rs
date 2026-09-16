use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, branch_head, commit_layer,
    find_property_key,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeltaReport {
    database: String,
    node_count: u64,
    changed_owners: u64,
    old_lower: u64,
    old_upper: u64,
    new_lower: u64,
    new_upper: u64,
    mutation_micros: u128,
    properties_set: u64,
    commit: String,
}

struct DeltaConfig {
    database: PathBuf,
    node_count: u64,
    changed_owners: u64,
    old_lower: u64,
    old_upper: u64,
    new_lower: u64,
    new_upper: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = DeltaConfig::from_args()?;
    let connection = Connection::open(&config.database)?;
    let (mutation_micros, properties_set, commit) = apply_delta(&connection, &config)?;
    let report = DeltaReport {
        database: config.database.display().to_string(),
        node_count: config.node_count,
        changed_owners: config.changed_owners,
        old_lower: config.old_lower,
        old_upper: config.old_upper,
        new_lower: config.new_lower,
        new_upper: config.new_upper,
        mutation_micros,
        properties_set,
        commit,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

impl DeltaConfig {
    fn from_args() -> Result<Self, Box<dyn Error>> {
        let database = std::env::args()
            .nth(1)
            .map(PathBuf::from)
            .ok_or("usage: lithograph-phase11-delta <database> [node-count]")?;
        let node_count = std::env::args()
            .nth(2)
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(10_000_000_u64);
        let changed_owners = 1_000_u64;
        let old_lower = node_count / 2 + 1;
        let old_upper = old_lower + changed_owners;
        let new_lower = old_lower + node_count;
        let new_upper = old_upper + node_count;
        Ok(Self {
            database,
            node_count,
            changed_owners,
            old_lower,
            old_upper,
            new_lower,
            new_upper,
        })
    }
}

fn apply_delta(
    connection: &Connection,
    config: &DeltaConfig,
) -> Result<(u128, u64, String), Box<dyn Error>> {
    let head = branch_head(connection, "main")?;
    let key_id = find_property_key(connection, "scaleId")?
        .ok_or("scale fixture is missing scaleId property key")?;
    let first_node: i64 = connection.query_row(
        "SELECT min(node_id) FROM main._lithograph_cp_nodes \
         WHERE commit_id = (SELECT commit_id FROM main._lithograph_checkpoints LIMIT 1)",
        [],
        |row| row.get(0),
    )?;
    let committed_at: i64 = connection.query_row(
        "SELECT coalesce(max(committed_at), 0) + 1 FROM main._lithograph_commits",
        [],
        |row| row.get(0),
    )?;
    let mut layer = LayerBuilder::default();
    for offset in 0..config.changed_owners {
        let old_value = config.old_lower + offset;
        let owner_id = first_node
            .checked_add(i64::try_from(old_value.saturating_sub(1))?)
            .ok_or("delta fixture NodeId overflow")?;
        let new_value = i64::try_from(old_value + config.node_count)?;
        layer.set_property(
            OwnerKind::Node,
            owner_id,
            key_id,
            PropertyValue::Integer(new_value),
        )?;
    }
    let started = Instant::now();
    let commit = commit_layer(
        connection,
        "main",
        head,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase11-performance".to_owned()),
            message: Some("1000-owner indexed delta fixture".to_owned()),
            committed_at,
        },
    )?;
    let mutation_micros = started.elapsed().as_micros();
    let properties_set = layer.delta_counts().properties_set;
    if properties_set != config.changed_owners {
        return Err(format!(
            "delta fixture changed {properties_set} properties, expected {}",
            config.changed_owners
        )
        .into());
    }
    Ok((
        mutation_micros,
        properties_set,
        format!("commit/{}", commit.to_hex()),
    ))
}
