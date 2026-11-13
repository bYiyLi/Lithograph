//! Exact bag witness: linear input-arm storage and one bit per output tuple.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use serde::Deserialize;

use crate::Result;

#[derive(Deserialize)]
pub(super) struct Recipe {
    hub: String,
    left: Vec<Vec<String>>,
    right: Vec<Vec<String>>,
    asp_selected_node: Option<String>,
}

pub(super) struct Witness<'a> {
    recipe: &'a Recipe,
    left_index: BTreeMap<Vec<String>, usize>,
    right_index: BTreeMap<Vec<String>, usize>,
    left_width: usize,
    right_width: usize,
    bits: Vec<u64>,
    expected: usize,
    observed: usize,
}

impl<'a> Witness<'a> {
    pub(super) fn new(recipe: &'a Recipe, expected: usize) -> Result<Self> {
        let left = recipe.left.iter().filter(|row| {
            recipe
                .asp_selected_node
                .as_ref()
                .is_none_or(|selected| row.last() == Some(selected))
        });
        let (left_index, left_width) = index_arm(left)?;
        let (right_index, right_width) = index_arm(recipe.right.iter())?;
        let product = left_index
            .len()
            .checked_mul(right_index.len())
            .ok_or("oracle overflow")?;
        if product == 0 || expected != product || expected > 8 * 1024 * 1024 {
            return Err("invalid or excessive oracle cardinality".into());
        }
        Ok(Self {
            recipe,
            left_index,
            right_index,
            left_width,
            right_width,
            bits: vec![0; expected.div_ceil(64)],
            expected,
            observed: 0,
        })
    }

    pub(super) fn accept(&mut self, row: &[Value]) -> Result<()> {
        let split = 1 + self.left_width;
        let end = split + self.right_width;
        if row.len() != end + usize::from(self.recipe.asp_selected_node.is_some())
            || string(&row[0])? != self.recipe.hub
        {
            return Err("output columns or hub differ".into());
        }
        let left = *self
            .left_index
            .get(&tuple(&row[1..split])?)
            .ok_or("unknown complete left tuple")?;
        let right = *self
            .right_index
            .get(&tuple(&row[split..end])?)
            .ok_or("unknown complete right tuple")?;
        if let Some(selected) = &self.recipe.asp_selected_node
            && string(&row[end])? != selected
        {
            return Err("ASP build identity differs".into());
        }
        let index = left * self.right_index.len() + right;
        let mask = 1_u64 << (index % 64);
        let word = &mut self.bits[index / 64];
        if *word & mask != 0 {
            return Err("duplicate identity tuple".into());
        }
        *word |= mask;
        self.observed += 1;
        Ok(())
    }

    pub(super) fn finish(&self) -> Result<()> {
        if self.observed != self.expected {
            return Err(format!("incomplete bag: {} / {}", self.observed, self.expected).into());
        }
        Ok(())
    }
}

type ArmIndex = (BTreeMap<Vec<String>, usize>, usize);

fn index_arm<'a>(rows: impl Iterator<Item = &'a Vec<String>>) -> Result<ArmIndex> {
    let mut result = BTreeMap::new();
    let mut width = 0;
    for (index, row) in rows.enumerate() {
        if !matches!(row.len(), 2 | 4) || (width != 0 && row.len() != width) {
            return Err("malformed or ragged arm identity recipe".into());
        }
        width = row.len();
        if result.insert(row.clone(), index).is_some() {
            return Err("duplicate full identity tuple in arm recipe".into());
        }
    }
    Ok((result, width))
}

fn tuple(row: &[Value]) -> Result<Vec<String>> {
    row.iter()
        .map(|value| Ok(string(value)?.to_owned()))
        .collect()
}

fn string(value: &Value) -> Result<&str> {
    match value {
        Value::String(value) => Ok(value.as_str()),
        _ => Err("elementId output is not a string".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe() -> Recipe {
        Recipe {
            hub: "n:1".into(),
            left: vec![
                vec!["r:1".into(), "n:2".into()],
                vec!["r:2".into(), "n:2".into()],
            ],
            right: vec![vec!["r:3".into(), "n:3".into()]],
            asp_selected_node: None,
        }
    }

    fn row(relationship: &str, node: &str) -> Vec<Value> {
        ["n:1", relationship, node, "r:3", "n:3"]
            .map(|s| Value::String(s.into()))
            .to_vec()
    }

    #[test]
    fn complete_parallel_bag_rejects_duplicate_and_wrong_endpoint() {
        let recipe = recipe();
        let mut witness = Witness::new(&recipe, 2).unwrap();
        assert!(witness.finish().is_err());
        assert!(witness.accept(&row("r:1", "n:99")).is_err());
        witness.accept(&row("r:1", "n:2")).unwrap();
        assert!(witness.accept(&row("r:1", "n:2")).is_err());
        witness.accept(&row("r:2", "n:2")).unwrap();
        witness.finish().unwrap();
    }
}
