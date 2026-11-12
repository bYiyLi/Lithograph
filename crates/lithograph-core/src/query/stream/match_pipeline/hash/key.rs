//! TRUE-equality keys for the five proven Hash families. A digest selects a
//! bucket; retained typed values still decide actual Cypher equality.

use crate::cypher::Value;
use crate::query::expression::{BindingRow, BindingValue, cypher_equals_controlled};
use crate::query::graph::{self, ResolvedGraphView};
use crate::query::plan::join::{HashFamily, HashInputKey};
use crate::query::resources::{ExecutionControl, MemoryReservation};
use crate::query::{QueryError, QueryMetrics, QueryResult};
use crate::storage::Snapshot;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(in crate::query::stream::match_pipeline) enum HashKeyValue {
    Scalar(Value),
    NodeId(i64),
    RelationshipId(i64),
}

#[derive(Debug)]
pub(in crate::query::stream::match_pipeline) struct EvaluatedHashKey {
    pub(in crate::query::stream::match_pipeline) digest: [u8; 32],
    pub(in crate::query::stream::match_pipeline) value: HashKeyValue,
    // Dropped after value. Scalar extraction initially owns its full Value
    // lease; the table may remove its inline header after reserving an entry.
    pub(in crate::query::stream::match_pipeline) memory: MemoryReservation,
}

pub(in crate::query::stream::match_pipeline) fn hash_key_inline_bytes(value: &HashKeyValue) -> u64 {
    match value {
        HashKeyValue::Scalar(_) => std::mem::size_of::<Value>() as u64,
        HashKeyValue::NodeId(_) | HashKeyValue::RelationshipId(_) => 0,
    }
}

pub(in crate::query::stream::match_pipeline) fn extract_hash_key(
    key: &HashInputKey,
    family: HashFamily,
    row: &BindingRow,
    snapshot: &Snapshot<'_>,
    view: &ResolvedGraphView,
    control: &ExecutionControl<'_>,
    metrics: &mut QueryMetrics,
) -> QueryResult<Option<EvaluatedHashKey>> {
    extract_hash_key_admitted(
        key,
        family,
        row,
        (snapshot, view),
        control,
        metrics,
        &mut |_| Ok(()),
    )
}

pub(in crate::query::stream::match_pipeline) fn extract_hash_key_admitted(
    key: &HashInputKey,
    family: HashFamily,
    row: &BindingRow,
    scope: (&Snapshot<'_>, &ResolvedGraphView),
    control: &ExecutionControl<'_>,
    metrics: &mut QueryMetrics,
    admission: &mut dyn FnMut(u64) -> QueryResult<()>,
) -> QueryResult<Option<EvaluatedHashKey>> {
    let (snapshot, view) = scope;
    control.check()?;
    let binding = row
        .values
        .get(&key.variable)
        .ok_or_else(|| QueryError::internal("Hash key provider did not bind its variable"))?;
    if matches!(
        binding,
        BindingValue::Null | BindingValue::Scalar(Value::Null)
    ) {
        return Ok(None);
    }
    let (value, memory) = match key.property.as_deref() {
        Some(property) => {
            if view.query_access.denies(property) {
                return Ok(None);
            }
            let (value, memory) =
                read_property(binding, property, snapshot, view, control, admission)?;
            metrics.record_db_hits(1);
            (HashKeyValue::Scalar(value), memory)
        }
        None => (identity(binding, family)?, control.reserve(0)?),
    };
    let mut evaluated = EvaluatedHashKey {
        digest: [0; 32],
        value,
        memory,
    };
    if let HashKeyValue::Scalar(value) = &evaluated.value
        && !matches!(value, Value::Null)
    {
        validate_family(value, family)?;
    }
    let Some(digest) = digest_key(&evaluated.value, control)? else {
        return Ok(None);
    };
    evaluated.digest = digest;
    Ok(Some(evaluated))
}

fn read_property(
    binding: &BindingValue,
    property: &str,
    snapshot: &Snapshot<'_>,
    view: &ResolvedGraphView,
    control: &ExecutionControl<'_>,
    admission: &mut dyn FnMut(u64) -> QueryResult<()>,
) -> QueryResult<(Value, MemoryReservation)> {
    match binding {
        BindingValue::Node(id) => graph::node_property_admitted_with_control(
            snapshot,
            *id,
            property,
            &view.query_access,
            control,
            admission,
        ),
        BindingValue::Relationship(record) => graph::relationship_property_admitted_with_control(
            snapshot,
            record.id,
            property,
            &view.query_access,
            control,
            admission,
        ),
        _ => Err(QueryError::internal(
            "Hash property key has no proven entity binding",
        )),
    }
}

fn identity(binding: &BindingValue, family: HashFamily) -> QueryResult<HashKeyValue> {
    match (binding, family) {
        (BindingValue::Node(id), HashFamily::NodeIdentity) => Ok(HashKeyValue::NodeId(*id)),
        (BindingValue::Relationship(record), HashFamily::RelationshipIdentity) => {
            Ok(HashKeyValue::RelationshipId(record.id))
        }
        _ => Err(QueryError::internal(
            "Hash identity key has no proven entity kind",
        )),
    }
}

fn validate_family(value: &Value, family: HashFamily) -> QueryResult<()> {
    if matches!(
        (value, family),
        (Value::Boolean(_), HashFamily::Boolean)
            | (Value::Integer(_) | Value::Float(_), HashFamily::Number)
            | (Value::String(_), HashFamily::String)
    ) {
        Ok(())
    } else {
        Err(QueryError::internal(
            "Hash property value disagrees with its family proof",
        ))
    }
}

fn digest_key(
    value: &HashKeyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<[u8; 32]>> {
    control.work(1)?;
    let mut digest = blake3::Hasher::new();
    match value {
        HashKeyValue::Scalar(Value::Null) => return Ok(None),
        HashKeyValue::Scalar(Value::Boolean(value)) => {
            digest.update(&[0, u8::from(*value)]);
        }
        HashKeyValue::Scalar(Value::Integer(value)) => integer_digest(&mut digest, *value),
        HashKeyValue::Scalar(Value::Float(value)) => {
            if value.is_nan() {
                return Ok(None);
            }
            match exact_integer(*value) {
                Some(value) => integer_digest(&mut digest, value),
                None => {
                    digest.update(&[2]);
                    digest.update(&value.to_bits().to_le_bytes());
                }
            }
        }
        HashKeyValue::Scalar(Value::String(value)) => {
            digest.update(&[3]);
            for chunk in value.as_bytes().chunks(4096) {
                control.work(1)?;
                control.bytes(chunk.len() as u64)?;
                digest.update(chunk);
            }
        }
        HashKeyValue::NodeId(id) => {
            digest.update(&[4]);
            digest.update(&id.to_le_bytes());
        }
        HashKeyValue::RelationshipId(id) => {
            digest.update(&[5]);
            digest.update(&id.to_le_bytes());
        }
        _ => {
            return Err(QueryError::internal(
                "Hash key has an unsupported scalar family",
            ));
        }
    }
    control.check()?;
    Ok(Some(*digest.finalize().as_bytes()))
}

fn integer_digest(digest: &mut blake3::Hasher, value: i64) {
    digest.update(&[1]);
    digest.update(&value.to_le_bytes());
}

fn exact_integer(value: f64) -> Option<i64> {
    // This is the TRUE-EQ interval used by cypher::integer_float_cmp. Casting
    // all Integer values to Float would lose identity beyond 2^53.
    const LOWER: f64 = -9_223_372_036_854_775_808.0;
    const UPPER: f64 = 9_223_372_036_854_775_808.0;
    (value.is_finite() && value.fract() == 0.0 && (LOWER..UPPER).contains(&value))
        .then_some(value as i64)
}

pub(in crate::query::stream::match_pipeline) fn hash_keys_equal(
    left: &HashKeyValue,
    right: &HashKeyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<bool>> {
    control.work(1)?;
    match (left, right) {
        (HashKeyValue::Scalar(left), HashKeyValue::Scalar(right)) => {
            cypher_equals_controlled(left, right, Some(control))
        }
        (HashKeyValue::NodeId(left), HashKeyValue::NodeId(right))
        | (HashKeyValue::RelationshipId(left), HashKeyValue::RelationshipId(right)) => {
            Ok(Some(left == right))
        }
        _ => Ok(Some(false)),
    }
}
