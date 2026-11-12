use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::config::Config;
use crate::{Result, read_json};

pub(super) const BILATERAL_NODES: usize = 200_002;
pub(super) const BILATERAL_HOT_ROWS: usize = 100_001;
pub(super) const BILATERAL_OUTPUT_ROWS: usize = 400_002;

pub(super) struct Fixture {
    pub(super) database: PathBuf,
    pub(super) commit: String,
    pub(super) nodes: usize,
    pub(super) first_node: i64,
    pub(super) hot_rows: usize,
    pub(super) distribution: String,
    pub(super) query: String,
    pub(super) columns: Vec<String>,
    pub(super) expected: usize,
    pub(super) database_blake3: String,
    pub(super) database_bytes: u64,
}

impl Fixture {
    pub(super) fn load(path: &Path, config: &Config) -> Result<Self> {
        let manifest = read_json(path)?;
        let nodes = validate_dimensions(&manifest, config.before)?;
        let commit = fixture_commit(&manifest)?;
        let distribution = string(&manifest, "distribution")?;
        let hot_rows = integer(&manifest, "hot_rows")?;
        validate_distribution(distribution, nodes, hot_rows)?;
        let (case, expected) = oracle_case(&manifest, &config.case, distribution, nodes)?;
        let first_node = node_range_start(&manifest, nodes)?;
        Ok(Self {
            database: Path::new(string(&manifest, "path")?).canonicalize()?,
            commit: commit.to_owned(),
            nodes,
            first_node,
            hot_rows,
            distribution: distribution.to_owned(),
            query: string(case, "query")?.to_owned(),
            columns: serde_json::from_value(case["columns"].clone())?,
            expected,
            database_blake3: string(&manifest, "database_blake3")?.to_owned(),
            database_bytes: manifest["database_bytes"]
                .as_u64()
                .ok_or("database_bytes must be unsigned")?,
        })
    }
}

fn validate_dimensions(manifest: &JsonValue, before: bool) -> Result<usize> {
    if manifest["kind"] != "phase22-hash-fixture-v1" {
        return Err("unsupported S22.5 fixture kind".into());
    }
    let nodes = integer(manifest, "node_count")?;
    if manifest["property_indexes"] != 0 {
        return Err("Hash fixture must contain no applicable Property Index".into());
    }
    if !(1_000..=1_000_000).contains(&nodes) || (before && nodes != 1_000) {
        return Err(
            "fixture needs 1k..1M nodes; Before is restricted to the safe 1k fixture".into(),
        );
    }
    Ok(nodes)
}

fn fixture_commit(manifest: &JsonValue) -> Result<&str> {
    let commit = string(manifest, "commit")?;
    let checkpoint = string(manifest, "checkpoint_commit")?;
    let hash = commit
        .strip_prefix("commit/")
        .ok_or("Commit must use commit/ prefix")?;
    lithograph_core::storage::HashId::from_hex(hash)?;
    if checkpoint != commit {
        return Err("fixture checkpoint must be at its exact current Commit".into());
    }
    Ok(commit)
}

fn oracle_case<'a>(
    manifest: &'a JsonValue,
    name: &str,
    distribution: &str,
    nodes: usize,
) -> Result<(&'a JsonValue, usize)> {
    let oracles = manifest["oracles"].as_array().ok_or("missing oracles")?;
    let case = oracles
        .iter()
        .find(|case| case["name"] == name)
        .ok_or("selected fixture case does not exist")?;
    let expected = if name == "small24" {
        24
    } else if distribution == "bilateral-skew" {
        BILATERAL_OUTPUT_ROWS
    } else {
        nodes
    };
    if integer(case, "expected_rows")? != expected {
        return Err("manifest cardinality differs from complete oracle recipe".into());
    }
    let recipe = if distribution == "bilateral-skew" {
        "bilateral-hot-equijoin-v1"
    } else {
        "offset-equijoin-v1"
    };
    if case["recipe"]["kind"] != recipe {
        return Err("manifest oracle recipe differs from the fixture distribution".into());
    }
    Ok((case, expected))
}

fn node_range_start(manifest: &JsonValue, nodes: usize) -> Result<i64> {
    let first_node = manifest["first_node"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or("first_node must be an actual positive NodeId")?;
    first_node
        .checked_add(i64::try_from(nodes)?)
        .ok_or("NodeId range overflow")?;
    Ok(first_node)
}

fn validate_distribution(distribution: &str, nodes: usize, hot_rows: usize) -> Result<()> {
    match distribution {
        "uniform" if hot_rows == 0 => Ok(()),
        "skew" if hot_rows == nodes.min(100_000) => Ok(()),
        "bilateral-skew" if nodes == BILATERAL_NODES && hot_rows == BILATERAL_HOT_ROWS => Ok(()),
        _ => Err("unsupported fixture key distribution".into()),
    }
}

fn integer(value: &JsonValue, field: &str) -> Result<usize> {
    Ok(usize::try_from(
        value[field]
            .as_u64()
            .ok_or_else(|| format!("{field} must be unsigned"))?,
    )?)
}

fn string<'a>(value: &'a JsonValue, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("{field} must be a string").into())
}
