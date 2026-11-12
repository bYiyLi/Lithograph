use lithograph_core::cypher::Value;
use serde_json::{Value as JsonValue, json};

use crate::Result;
use crate::fixture::{BILATERAL_HOT_ROWS, BILATERAL_NODES, Fixture};

/// Host-side exact bag verification. The engine never receives this bitset.
pub(super) struct Oracle<'a> {
    fixture: &'a Fixture,
    seen: Vec<u64>,
    rows: usize,
    ordered: blake3::Hasher,
    unordered: [u8; 32],
}

impl<'a> Oracle<'a> {
    pub(super) fn new(fixture: &'a Fixture) -> Self {
        Self {
            fixture,
            seen: vec![0; fixture.expected.div_ceil(64)],
            rows: 0,
            ordered: blake3::Hasher::new(),
            unordered: [0; 32],
        }
    }

    pub(super) fn row(&mut self, row: &[Value]) -> Result<()> {
        let [
            Value::Integer(0),
            Value::String(left),
            Value::String(right),
            Value::Integer(left_key),
            Value::Integer(right_key),
        ] = row
        else {
            return Err(format!("unexpected typed result row {row:?}").into());
        };
        let left_offset = self.node_offset(left)?;
        let right_offset = self.node_offset(right)?;
        let (slot, key) = self.pair_slot(left_offset, right_offset)?;
        if *left_key != key || *right_key != key {
            return Err(format!("ID/value oracle differs at pair slot {slot}: {row:?}").into());
        }
        let bit = 1_u64 << (slot % 64);
        let word = &mut self.seen[slot / 64];
        if *word & bit != 0 {
            return Err(format!("duplicate result row at oracle pair slot {slot}").into());
        }
        *word |= bit;
        self.rows += 1;
        let bytes = format!("{row:?}\n");
        self.ordered.update(bytes.as_bytes());
        for (total, byte) in self
            .unordered
            .iter_mut()
            .zip(blake3::hash(bytes.as_bytes()).as_bytes())
        {
            *total ^= byte;
        }
        Ok(())
    }

    fn node_offset(&self, value: &str) -> Result<usize> {
        let offset = usize::try_from(
            node_id(value)?
                .checked_sub(self.fixture.first_node)
                .ok_or("Node ID underflow")?,
        )?;
        if offset >= self.fixture.nodes {
            return Err("Node ID is outside the actual fixture domain".into());
        }
        Ok(offset)
    }

    fn pair_slot(&self, left: usize, right: usize) -> Result<(usize, i64)> {
        if self.fixture.distribution == "bilateral-skew" {
            return self.bilateral_slot(left, right);
        }
        if left >= self.fixture.expected {
            return Err("left ID is outside this case's actual oracle domain".into());
        }
        let key = if self.fixture.distribution == "skew" && left < self.fixture.hot_rows {
            1
        } else {
            i64::try_from(left)? + 1
        };
        if right != usize::try_from(key - 1)? {
            return Err("right ID differs from the offset equality oracle".into());
        }
        Ok((left, key))
    }

    fn bilateral_slot(&self, left: usize, right: usize) -> Result<(usize, i64)> {
        if self.fixture.expected == 24 {
            return match (left, right) {
                (0, 0) => Ok((0, 1)),
                (2..=24, 0) => Ok((left - 1, 1)),
                _ => Err("pair is outside the bilateral small24 oracle".into()),
            };
        }
        if left < BILATERAL_HOT_ROWS && right < 2 {
            return Ok((2 * left + right, 1));
        }
        if left == BILATERAL_HOT_ROWS && (2..=BILATERAL_HOT_ROWS + 1).contains(&right) {
            return Ok((BILATERAL_NODES + right - 2, 2));
        }
        if (BILATERAL_HOT_ROWS + 1..BILATERAL_NODES - 1).contains(&left) && right == left + 1 {
            return Ok((
                BILATERAL_NODES + left - 1,
                i64::try_from(left - BILATERAL_HOT_ROWS + 2)?,
            ));
        }
        Err("pair is outside the complete bilateral equality oracle".into())
    }

    pub(super) fn finish(self) -> Result<JsonValue> {
        if self.rows != self.fixture.expected {
            return Err(
                format!("incomplete bag: {} / {}", self.rows, self.fixture.expected).into(),
            );
        }
        let unordered = self
            .unordered
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(
            json!({"rows": self.rows, "complete_typed_id_value_bag_verified": true,
            "duplicate_rows_rejected": true, "host_bitset_bytes": self.seen.len() * 8,
            "host_bitset_scope": "oracle only; outside engine QueryResources; no retained result rows",
            "actual_row_order_blake3": self.ordered.finalize().to_hex().to_string(),
            "actual_row_hash_xor_hex": unordered,
            "bag_proof": "each exact typed ID/key row maps to its expected pair slot; bitset uniqueness plus exact complete cardinality"}),
        )
    }
}

fn node_id(value: &str) -> Result<i64> {
    let id = value
        .strip_prefix("n:")
        .ok_or("expected a Node elementId")?
        .parse::<i64>()?;
    if id <= 0 || value != format!("n:{id}") {
        return Err("expected canonical positive Node elementId".into());
    }
    Ok(id)
}
