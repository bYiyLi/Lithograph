use super::*;

fn typed_row() -> BindingRow {
    let mut row = value_row(i64::MAX, 0);
    let relationship = RelationshipRecord {
        id: i64::MAX,
        source: 1,
        type_id: 2,
        target: 3,
    };
    row.insert("node".to_owned(), BindingValue::Node(9_007_199_254_740_993));
    row.insert("edge".to_owned(), BindingValue::Relationship(relationship));
    row.insert(
        "path".to_owned(),
        BindingValue::Path {
            nodes: vec![1, 3],
            relationships: vec![relationship],
        },
    );
    row.insert("null".to_owned(), BindingValue::Null);
    row.insert(
        "negative_zero".to_owned(),
        BindingValue::Scalar(Value::Float(-0.0)),
    );
    row.insert(
        "float".to_owned(),
        BindingValue::Scalar(Value::Float(51.248_178_375_505_404)),
    );
    row.insert(
        "nan".to_owned(),
        BindingValue::Scalar(Value::Float(f64::NAN)),
    );
    row.used_relationships.extend([1, i64::MAX]);
    row.load_csv_context = Some(LoadCsvContext {
        file: Some("synthetic.csv".to_owned()),
        line: Some(7),
    });
    row
}

#[test]
fn typed_binding_metadata_and_float_bits_survive_partition_moves() {
    let expected = typed_row();
    let resources = fixture_resources(&expected);
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = seeded_build_store(&resources, &control, [17; 32], expected.clone());
    store.repartition(PartitionId::ROOT, &control).unwrap();
    let leaf = store.route_digest(&[17; 32]).unwrap();
    let mut page = store
        .read_batch_reserved(leaf, HashSide::Build, -1, limit(1, 1), &control)
        .unwrap();
    let actual = page.next().unwrap();
    assert_eq!(actual.digest, [17; 32]);
    assert_eq!(actual.row.order, expected.order);
    assert_eq!(actual.row.used_relationships, expected.used_relationships);
    assert_eq!(actual.row.load_csv_context, expected.load_csv_context);
    for key in ["id", "text", "node", "edge", "path", "null"] {
        assert_eq!(actual.row.values[key], expected.values[key]);
    }
    for key in ["negative_zero", "float", "nan"] {
        let (
            BindingValue::Scalar(Value::Float(actual)),
            BindingValue::Scalar(Value::Float(expected)),
        ) = (&actual.row.values[key], &expected.values[key])
        else {
            panic!("typed Float");
        };
        if expected.is_nan() {
            assert!(actual.is_nan());
        } else {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }
    assert_eq!(
        actual.memory.bytes(),
        binding_row_bytes(&actual.row, &control).unwrap()
    );
    drop(actual);
    drop(page);
    store.abort().unwrap();
    assert_eq!(resources.used(), 0);
}

#[test]
fn append_refuses_unreserved_payload_and_wrong_route_before_sql() {
    for wrong_route in [false, true] {
        let resources = QueryResources::new(Some(64 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
        let row = value_row(1, 0);
        let bytes = if wrong_route {
            binding_row_bytes(&row, &control).unwrap()
        } else {
            0
        };
        let memory = resources.reserve(bytes).unwrap();
        let partition = if wrong_route {
            PartitionId(999)
        } else {
            PartitionId::ROOT
        };
        let error = store
            .append_reserved(partition, HashSide::Build, [0; 32], row, memory, &control)
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Internal);
        assert_eq!(store.counters().written_rows, [0; 2]);
        assert_closed(&store);
        drop(store);
        assert_eq!(resources.used(), 0);
    }
}
