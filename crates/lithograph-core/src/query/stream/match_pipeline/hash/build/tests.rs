use super::*;

#[test]
fn dynamic_filter_optional_claim_cannot_make_a_resident_hash_require_temp() {
    let row = BindingRow::default();
    let row_bytes = expression::binding_row_bytes(&row).unwrap();
    let key_bytes = std::mem::size_of::<Value>() as u64;
    let probe_resources = QueryResources::default();
    let probe_control = ExecutionControl::new(&probe_resources, &|| false);
    let table = HashTable::new(&probe_control).unwrap();
    let projected = table.projected_bytes(row_bytes, key_bytes).unwrap();
    let headroom = row_headroom(row_bytes).unwrap();
    drop(table);
    assert_eq!(probe_resources.used(), 0);

    let limit = checked_add(projected, headroom).unwrap();
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let key = EvaluatedHashKey {
        digest: [0; 32],
        value: key::HashKeyValue::Scalar(Value::Integer(7)),
        memory: control.reserve(key_bytes).unwrap(),
    };
    let input = (row, control.reserve(row_bytes).unwrap());
    let mut state = HashBuildState::new();
    state.storage = BuildStorage::Memory(HashTable::new(&control).unwrap());
    let mut filter = dynamic_filter::KeyCollector::new(&control)
        .unwrap()
        .unwrap();
    assert!(filter.observe(&key, &control).unwrap());
    let incoming = checked_add(row_bytes, key_bytes).unwrap();
    assert!(hash_resident_target(&resources, 0, incoming, headroom) < projected);
    assert_eq!(
        hash_resident_target(&resources, filter.owned_bytes(), incoming, headroom),
        projected,
        "only the optional allocation crosses the original resident boundary"
    );
    state.filter = Some(filter);
    state.keep_build_row(input, key, &control).unwrap();
    assert!(
        state.filter.is_none(),
        "an incomplete advisory set is discarded"
    );
    let BuildStorage::Memory(table) = &state.storage else {
        panic!("optional state must not force a new TEMP requirement")
    };
    assert_eq!(table.len(), 1);
    assert!(resources.peak() <= limit);
    drop(state);
    assert_eq!(resources.used(), 0);
}
