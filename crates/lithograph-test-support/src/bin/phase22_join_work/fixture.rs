use std::path::{Path, PathBuf};

use serde_json::{Value as JsonValue, json};

use crate::{Result, read_json};

pub(super) struct Fixture {
    pub(super) database: PathBuf,
    pub(super) commit: String,
    pub(super) cases: Vec<Case>,
}

pub(super) struct Case {
    pub(super) name: String,
    pub(super) query: String,
    pub(super) columns: Vec<String>,
    pub(super) oracle: Vec<Vec<JsonValue>>,
}

impl Fixture {
    pub(super) fn load(path: &Path, selected: Option<&str>) -> Result<Self> {
        let manifest = read_json(path)?;
        let kind = string(&manifest, "kind")?;
        let (database, commit, mut cases) = match kind {
            "phase22_join_cycle_fixture" => (
                string(&manifest, "path")?,
                string(&manifest, "checkpoint_commit")?,
                cycle_cases(&manifest, selected)?,
            ),
            "inl" => (
                string(&manifest, "database")?,
                string(&manifest, "anchor")?,
                vec![inl_case(&manifest)?],
            ),
            _ => return Err("only S22.4 retained cycle and INL manifests are supported".into()),
        };
        let hash = commit.strip_prefix("commit/").unwrap_or(commit);
        lithograph_core::storage::HashId::from_hex(hash)?;
        if let Some(name) = selected {
            cases.retain(|case| case.name == name);
        }
        validate_cases(&cases)?;
        Ok(Self {
            database: Path::new(database).canonicalize()?,
            commit: format!("commit/{hash}"),
            cases,
        })
    }
}

fn validate_cases(cases: &[Case]) -> Result<()> {
    if cases.is_empty() || cases.len() > 64 {
        return Err("no selected case, or more than 64 cases".into());
    }
    for case in cases {
        if case.oracle.len() > 64
            || case.columns.len() > 64
            || case
                .oracle
                .iter()
                .any(|row| row.len() != case.columns.len())
        {
            return Err("fixture exceeds small complete-result oracle bounds".into());
        }
    }
    Ok(())
}

fn string<'a>(value: &'a JsonValue, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("manifest field {name} must be a string").into())
}

fn inl_case(manifest: &JsonValue) -> Result<Case> {
    let (label, property) = (string(manifest, "label")?, string(manifest, "property")?);
    for identifier in [label, property] {
        if identifier.is_empty()
            || !identifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("INL fixture identifier is outside the retained synthetic contract".into());
        }
    }
    Ok(Case {
        name: "inl".to_owned(),
        query: format!(
            "MATCH(o:S4JoinOuter),(i:{label}) WHERE o.key=i.{property} RETURN 0,elementId(o),elementId(i),o.key,i.{property}"
        ),
        columns: ["0", "elementId(o)", "elementId(i)", "o.key"]
            .into_iter()
            .map(str::to_owned)
            .chain(std::iter::once(format!("i.{property}")))
            .collect(),
        oracle: serde_json::from_value(manifest["oracle"].clone())?,
    })
}

fn cycle_cases(manifest: &JsonValue, selected: Option<&str>) -> Result<Vec<Case>> {
    let oracles = manifest["oracles"]
        .as_array()
        .ok_or("missing cycle oracles")?;
    if oracles.len() != 6 {
        return Err("cycle manifest must contain its six 3..8-Part oracles".into());
    }
    if let Some(case) = stress_case(manifest, selected)? {
        return Ok(vec![case]);
    }
    let mut cases = Vec::new();
    for (offset, oracle) in oracles.iter().enumerate() {
        let parts = oracle["parts"].as_u64().ok_or("missing Part count")? as usize;
        if parts != offset + 3 {
            return Err("cycle Part counts must be 3 through 8 in order".into());
        }
        let columns: Vec<String> = serde_json::from_value(oracle["columns"].clone())?;
        let rows: Vec<Vec<JsonValue>> = serde_json::from_value(oracle["rows"].clone())?;
        for rotation in 0..parts {
            cases.push(cycle_case(parts, rotation, &columns, &rows));
        }
    }
    Ok(cases)
}

fn stress_case(manifest: &JsonValue, selected: Option<&str>) -> Result<Option<Case>> {
    let (name, parts, rare_first) = match selected {
        Some(name @ "greedy_24_rare_last") => (name, 24, false),
        Some(name @ "budget_64_rare_first") => (name, 64, true),
        Some(name @ "budget_129_rare_first") => (name, 129, true),
        _ => return Ok(None),
    };
    let rare = manifest["rare_node"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or("cycle pressure case needs its actual positive rare_node ID")?;
    let rare_position = if rare_first { 0 } else { parts - 1 };
    let patterns = (0..parts)
        .map(|position| {
            let label = if position == rare_position {
                "S4Rare"
            } else {
                "S4Chain"
            };
            format!("(n:{label})")
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(Some(Case {
        name: name.to_owned(),
        query: format!("MATCH {patterns} RETURN 0,elementId(n)"),
        columns: vec!["0".to_owned(), "elementId(n)".to_owned()],
        oracle: vec![vec![json!(0), json!(format!("n:{rare}"))]],
    }))
}

fn cycle_case(parts: usize, rotation: usize, columns: &[String], rows: &[Vec<JsonValue>]) -> Case {
    let order = (rotation..parts).chain((0..rotation).rev());
    let patterns = order
        .map(|i| {
            let target = if i + 1 == parts { "S4Rare" } else { "S4Chain" };
            format!("(n{i}:S4Chain)-[r{i}:S4Next]->(n{}:{target})", i + 1)
        })
        .collect::<Vec<_>>()
        .join(",");
    let columns = std::iter::once("0".to_owned())
        .chain(columns.iter().cloned())
        .collect::<Vec<_>>();
    let oracle = rows
        .iter()
        .map(|row| {
            std::iter::once(json!(0))
                .chain(row.iter().cloned())
                .collect()
        })
        .collect();
    Case {
        name: format!("cycle_{parts}_rotation_{rotation}"),
        query: format!("MATCH {patterns} RETURN {}", columns.join(",")),
        columns,
        oracle,
    }
}
