use super::*;

fn hash_operator() -> query::OperatorRuntimeMetrics {
    query::OperatorRuntimeMetrics {
        id: 3,
        operator: "HashJoin".to_owned(),
        rows: 17,
        db_hits: 0,
        semi_mask: None,
        factorized: None,
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
            dynamic_filter: None,
        }),
    }
}

#[test]
fn finite_id_advice_is_an_actual_optional_annotation_on_the_shared_encoder() {
    let mut operator = hash_operator();
    operator.hash = None;
    operator.operator = "LabelIndexScan".to_owned();
    let fallback_bytes = operator_profile_metadata_bytes(&operator).unwrap();
    assert!(operator_profile_json(&operator).get("semiMask").is_none());
    operator.semi_mask = Some(query::SemiMaskRuntimeMetrics { keys: 3, seeks: 2 });
    assert_eq!(
        operator_profile_json(&operator),
        json!({"id": 3, "operator": "LabelIndexScan", "rows": 17, "dbHits": 0,
               "semiMask": {"keys": 3, "seeks": 2}})
    );
    let entries = query::resources::map_entry_bytes::<Value>().unwrap() * 3;
    assert!(operator_profile_metadata_bytes(&operator).unwrap() >= fallback_bytes + entries);
    operator.semi_mask = None;
    assert!(operator_profile_json(&operator).get("semiMask").is_none());
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
fn dynamic_filter_metadata_contains_only_actual_fixed_work_counters() {
    let mut operator = hash_operator();
    let hash = operator.hash.as_mut().unwrap();
    hash.dynamic_filter = Some(Default::default());
    let filter = hash.dynamic_filter.as_mut().unwrap();
    filter.applied = true;
    filter.build_rows = 1024;
    filter.distinct_keys = 3;
    filter.collector_probes = 1030;
    filter.keyset_peak_bytes = 512;
    filter.prefix_rows_read = 256;
    filter.equality_seeks = 3;
    filter.tail_owners = 2;
    let value = operator_profile_json(&operator);
    assert_eq!(
        value["hash"]["dynamicFilter"],
        json!({
            "applied": true, "buildRows": 1024, "distinctKeys": 3,
            "collectorProbes": 1030, "keysetPeakBytes": 512,
            "prefixRowsRead": 256, "equalitySeeks": 3, "tailOwners": 2,
        })
    );
    assert!(
        operator_profile_metadata_bytes(&operator).unwrap()
            >= query::resources::json_bytes(&value).unwrap()
    );
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
                dynamic_filter: None,
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
        query::resources::checked_add(preflight, profile_memory_metadata_bytes().unwrap()).unwrap();
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

#[test]
fn factorized_profile_fields_are_reserved_and_absent_for_flat_fallback() {
    let mut operator = hash_operator();
    operator.operator = "ASPJoin".into();
    operator.hash = None;
    let flat = operator_profile_metadata_bytes(&operator).unwrap();
    operator.factorized = Some(query::FactorizedRuntimeMetrics {
        prefixes: 1,
        accumulate_rows: 2,
        build_rows: 3,
        hash_entries: 4,
        probe_rows: 5,
        group_members: 6,
        flattened_rows: 7,
        semijoin_keys: 8,
        semijoin_seeks: 9,
        memory_peak_bytes: u64::MAX,
        spill_encoded_bytes: u64::MAX,
        spill_batches: 12,
        spill_vm_steps: 13,
        temp_allocated_bytes: u64::MAX,
        accumulate_micros: 15,
        build_micros: 16,
        probe_micros: 17,
        predicate_evaluations: 18,
        expression_evaluations: 19,
    });
    let value = operator_profile_json(&operator);
    assert_eq!(
        value["factorized"],
        json!({
            "prefixes":1,"accumulateRows":2,"buildRows":3,"hashEntries":4,
            "probeRows":5,"groupMembers":6,"flattenedRows":7,
            "semijoinKeys":8,"semijoinSeeks":9,"memoryPeakBytes":u64::MAX,
            "spillEncodedBytes":u64::MAX,"spillBatches":12,"spillVmSteps":13,
            "tempAllocatedBytes":u64::MAX,"accumulateMicros":15,
            "buildMicros":16,"probeMicros":17,"predicateEvaluations":18,
            "expressionEvaluations":19,
        })
    );
    let estimate = operator_profile_metadata_bytes(&operator).unwrap();
    assert!(estimate >= query::resources::json_bytes(&value).unwrap());
    assert!(estimate >= flat + query::resources::map_entry_bytes::<Value>().unwrap() * 20);
    let resources = query::QueryResources::new(Some(estimate - 1));
    let constructed = std::cell::Cell::new(false);
    assert!(
        budget::json_owned(&resources, estimate, || {
            constructed.set(true);
            value
        })
        .is_err()
    );
    assert!(!constructed.get());
    assert_eq!(resources.peak(), 0);
    operator.factorized = None;
    assert!(operator_profile_json(&operator).get("factorized").is_none());
    assert_eq!(operator_profile_metadata_bytes(&operator).unwrap(), flat);
}
