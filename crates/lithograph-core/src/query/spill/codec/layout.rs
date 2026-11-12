//! Logical JSON payload bounds for independent fixed identity bindings.
//! This describes the existing codec, never Rust DOM/String scratch capacity.

use super::*;

const I64_BYTES: u64 = 20;
const EMPTY_ROW_BYTES: u64 =
    br#"{"values":{},"usedRelationships":[],"order":[],"loadCsvContext":null}"#.len() as u64;
const NODE_BYTES: u64 = br#"{"kind":"node","id":0}"#.len() as u64 + I64_BYTES - 1;
const RELATIONSHIP_BYTES: u64 =
    br#"{"kind":"relationship","record":[0,0,0,0]}"#.len() as u64 + 4 * (I64_BYTES - 1);

/// `names` are the written start Node, Relationship and end Node names. A
/// repeated Node name is encoded once. The caller proves these are identity
/// bindings with no Path, Scalar or inherited LOAD CSV context; this API makes
/// no width claim for those other values. Anonymous Relationships still enter
/// the per-MATCH used set and must be counted by `used_relationships`.
pub(in crate::query) fn identity_binding_encoded_upper(
    names: [Option<&str>; 3],
    used_relationships: usize,
) -> QueryResult<u64> {
    let mut bytes = EMPTY_ROW_BYTES;
    let mut count = 0;
    for (position, name) in names.iter().enumerate() {
        let Some(name) = name else { continue };
        if names[..position].contains(&Some(*name)) {
            continue;
        }
        // A UTF-8 source byte needs at most six JSON bytes. The name occurs
        // both as a values-object key and as one string in the order array.
        let name_bytes = checked_mul(name.len() as u64, 12)?;
        let value = if names[1] == Some(*name) {
            RELATIONSHIP_BYTES
        } else {
            NODE_BYTES
        };
        bytes = checked_add(bytes, checked_add(name_bytes, checked_add(value, 5)?)?)?;
        count += 1;
    }
    if count > 0 {
        bytes = checked_add(bytes, 2 * (count - 1))?;
    }
    if used_relationships > 0 {
        bytes = checked_add(bytes, checked_mul(used_relationships as u64, I64_BYTES)?)?;
        bytes = checked_add(bytes, (used_relationships - 1) as u64)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryResources;
    use crate::query::expression::{BindingRow, BindingValue};

    fn compare_codec(names: [Option<&str>; 3], used: bool) {
        let mut row = BindingRow::default();
        for (position, name) in names.iter().enumerate() {
            let Some(name) = name else { continue };
            if names[..position].contains(&Some(*name)) {
                continue;
            }
            let value = if position == 1 {
                BindingValue::Relationship(RelationshipRecord {
                    id: i64::MIN,
                    source: i64::MIN,
                    type_id: i64::MIN,
                    target: i64::MIN,
                })
            } else {
                BindingValue::Node(i64::MIN)
            };
            row.insert((*name).to_owned(), value);
        }
        if used {
            row.used_relationships.insert(i64::MIN);
        }
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let actual = encode_binding_row(&row, &control).unwrap();
        let bound = identity_binding_encoded_upper(names, usize::from(used)).unwrap();
        assert!(
            actual.len() as u64 <= bound,
            "actual={} bound={bound}",
            actual.len()
        );
    }

    #[test]
    fn identity_traffic_bound_covers_codec_max_digits_and_shared_or_anonymous_names() {
        for (names, used) in [
            ([Some("n"), None, None], false),
            ([Some("n"), Some("r"), Some("m")], true),
            ([Some("n"), Some("r"), Some("n")], true),
            ([Some("~"), Some("~F"), Some("~S")], true),
            ([None, None, None], true),
            ([None, None, None], false),
        ] {
            compare_codec(names, used);
        }
    }

    #[test]
    fn identity_traffic_bound_covers_json_escaping_without_borrowed_name_allocations() {
        let name = "\"\\\n\0🧭".repeat(128);
        compare_codec([Some(&name), Some("r\t"), Some("m\r")], true);
        let bound = identity_binding_encoded_upper([Some("n"), None, None], 0).unwrap();
        assert!(
            bound < 256,
            "a single identity is not a multi-KiB scratch lease"
        );
    }
}
