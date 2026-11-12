use super::*;

fn hash_operator() -> query::OperatorRuntimeMetrics {
    query::OperatorRuntimeMetrics {
        id: 3,
        operator: "HashJoin".to_owned(),
        rows: 17,
        db_hits: 0,
        hash: Some(query::HashJoinRuntimeMetrics {
            build_rows: 1,
            probe_rows: 2,
            hash_collisions: 3,
            spill_partitions: 4,
            spill_batches: 5,
            spill_vm_steps: 6,
            spill_encoded_bytes: 7,
            memory_peak_bytes: 8,
            temp_allocated_bytes: 9,
        }),
    }
}

#[test]
fn one_operator_encoder_exposes_exact_fixed_hash_keys_and_omits_fallback() {
    let mut operator = hash_operator();
    let value = operator_profile_json(&operator);
    assert_eq!(value["hash"].as_object().unwrap().len(), 9);
    for (key, expected) in [
        ("buildRows", 1),
        ("probeRows", 2),
        ("hashCollisions", 3),
        ("spillPartitions", 4),
        ("spillBatches", 5),
        ("spillVmSteps", 6),
        ("spillEncodedBytes", 7),
        ("memoryPeakBytes", 8),
        ("tempAllocatedBytes", 9),
    ] {
        assert_eq!(value["hash"][key], expected);
    }
    assert_eq!(value["rows"], 17);
    assert_eq!(value["dbHits"], 0);
    operator.operator = "NestedLoop".to_owned();
    operator.hash = None;
    let fallback = operator_profile_json(&operator);
    assert!(fallback.get("hash").is_none());
    assert_eq!(fallback.as_object().unwrap().len(), 4);
}

#[test]
fn hash_json_metadata_is_reserved_before_building_many_operator_objects() {
    let operators = (0..128)
        .map(|id| {
            let mut operator = hash_operator();
            operator.id = id;
            operator.hash = Some(query::HashJoinRuntimeMetrics {
                build_rows: u64::MAX,
                probe_rows: u64::MAX,
                hash_collisions: u64::MAX,
                spill_partitions: u64::MAX,
                spill_batches: u64::MAX,
                spill_vm_steps: u64::MAX,
                spill_encoded_bytes: u64::MAX,
                memory_peak_bytes: u64::MAX,
                temp_allocated_bytes: u64::MAX,
            });
            operator
        })
        .collect::<Vec<_>>();
    let expected = json!({
        "operators": operators.iter().map(operator_profile_json).collect::<Vec<_>>(),
        "executionMemoryPeakBytes": u64::MAX, "processPeakRssBytes": null,
    });
    let preflight = operators
        .iter()
        .try_fold(65_536, |bytes, operator| {
            query::resources::checked_add(bytes, operator_profile_metadata_bytes(operator)?)
        })
        .unwrap();
    let preflight =
        query::resources::checked_add(preflight, hash_memory_metadata_bytes().unwrap()).unwrap();
    assert!(preflight >= query::resources::json_bytes(&expected).unwrap());
    let resources = query::QueryResources::new(Some(preflight - 1));
    let constructed = std::cell::Cell::new(false);
    let result = budget::json_owned(&resources, preflight, || {
        constructed.set(true);
        expected
    });
    assert!(result.is_err());
    assert!(!constructed.get());
    assert_eq!(resources.peak(), 0);
}
