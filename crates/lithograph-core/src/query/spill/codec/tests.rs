use super::*;
use crate::cypher::{
    NodeValue, PathValue, PointValue, RelationshipValue, VectorCoordinateType, VectorValue,
    VectorValues,
};
use crate::query::spill::{BindingSpill, open_spill_connection};

fn numeric_values() -> Vec<Value> {
    vec![
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Integer(9_007_199_254_740_991),
        Value::Integer(9_007_199_254_740_992),
        Value::Integer(9_007_199_254_740_993),
        Value::Float(1.0),
        Value::Float(0.0),
        Value::Float(-0.0),
        Value::Float(51.248_178_375_505_404),
        Value::Float(2.003_039_774_426_776_2e-253),
        Value::Float(f64::from_bits(1)),
        Value::Float(f64::MAX),
        Value::Float(9_223_372_036_854_775_808.0),
        Value::Float(-9_223_372_036_854_775_808.0),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        Value::Float(f64::NAN),
        Value::Null,
    ]
}

fn assert_value_exact(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Float(actual), Value::Float(expected)) if expected.is_nan() => {
            assert!(actual.is_nan());
        }
        (Value::Float(actual), Value::Float(expected)) => {
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "Float {expected:?} became {actual:?}"
            );
        }
        (Value::List(actual), Value::List(expected)) => {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_value_exact(actual, expected);
            }
        }
        (Value::Map(actual), Value::Map(expected)) => {
            assert_map_exact(actual, expected);
        }
        (Value::Node(actual), Value::Node(expected)) => assert_node_exact(actual, expected),
        (Value::Relationship(actual), Value::Relationship(expected)) => {
            assert_relationship_exact(actual, expected)
        }
        (Value::Path(actual), Value::Path(expected)) => {
            assert_eq!(actual.nodes.len(), expected.nodes.len());
            assert_eq!(actual.relationships.len(), expected.relationships.len());
            for (actual, expected) in actual.nodes.iter().zip(&expected.nodes) {
                assert_node_exact(actual, expected);
            }
            for (actual, expected) in actual.relationships.iter().zip(&expected.relationships) {
                assert_relationship_exact(actual, expected);
            }
        }
        (Value::Point(actual), Value::Point(expected)) => {
            assert_eq!(actual.crs(), expected.crs());
            assert_eq!(actual.srid(), expected.srid());
            assert_eq!(actual.coordinates().len(), expected.coordinates().len());
            for (actual, expected) in actual.coordinates().iter().zip(expected.coordinates()) {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
        (Value::Vector(actual), Value::Vector(expected)) => {
            assert_eq!(actual.coordinate_type(), expected.coordinate_type());
            match (actual.values(), expected.values()) {
                (VectorValues::F64(actual), VectorValues::F64(expected)) => {
                    assert_eq!(actual.len(), expected.len());
                    for (actual, expected) in actual.iter().zip(expected) {
                        assert_eq!(actual.to_bits(), expected.to_bits());
                    }
                }
                _ => assert_eq!(actual, expected),
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

fn assert_map_exact(actual: &BTreeMap<String, Value>, expected: &BTreeMap<String, Value>) {
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (key, expected) in expected {
        assert_value_exact(&actual[key], expected);
    }
}

fn assert_node_exact(actual: &NodeValue, expected: &NodeValue) {
    assert_eq!(actual.element_id, expected.element_id);
    assert_eq!(actual.labels, expected.labels);
    assert_map_exact(&actual.properties, &expected.properties);
}

fn assert_relationship_exact(actual: &RelationshipValue, expected: &RelationshipValue) {
    assert_eq!(actual.element_id, expected.element_id);
    assert_eq!(actual.start, expected.start);
    assert_eq!(actual.end, expected.end);
    assert_eq!(actual.relationship_type, expected.relationship_type);
    assert_map_exact(&actual.properties, &expected.properties);
}

fn structured_payload() -> Value {
    Value::Map(BTreeMap::from([
        ("numbers".to_owned(), Value::List(numeric_values())),
        (
            "nested".to_owned(),
            Value::List(vec![Value::Map(BTreeMap::from([
                ("fraction".to_owned(), Value::Float(51.248_178_375_505_404)),
                ("negative_zero".to_owned(), Value::Float(-0.0)),
            ]))]),
        ),
        ("reserved".to_owned(), reserved_map()),
        ("graph".to_owned(), graph_payload()),
        (
            "point".to_owned(),
            Value::Point(PointValue::new("cartesian", vec![51.248_178_375_505_404, -0.0]).unwrap()),
        ),
        (
            "vector".to_owned(),
            Value::Vector(
                VectorValue::new(
                    VectorCoordinateType::F64,
                    VectorValues::F64(vec![
                        51.248_178_375_505_404,
                        2.003_039_774_426_776_2e-253,
                        -0.0,
                    ]),
                )
                .unwrap(),
            ),
        ),
    ]))
}

fn reserved_map() -> Value {
    Value::Map(BTreeMap::from([
        (
            "$type".to_owned(),
            Value::String("SpillFloatBits".to_owned()),
        ),
        (
            "value".to_owned(),
            Value::String("3ff0000000000000".to_owned()),
        ),
        (
            "marker".to_owned(),
            Value::String("~F3ff0000000000000".to_owned()),
        ),
        (
            "escaped".to_owned(),
            Value::String("~S~F3ff0000000000000".to_owned()),
        ),
        ("unknown".to_owned(), Value::String("~other".to_owned())),
        (
            "entries".to_owned(),
            Value::Map(BTreeMap::from([
                ("$type".to_owned(), Value::String("Map".to_owned())),
                ("entries".to_owned(), Value::Float(51.248_178_375_505_404)),
            ])),
        ),
    ]))
}

fn graph_payload() -> Value {
    let properties = BTreeMap::from([
        (
            "$type".to_owned(),
            Value::String("SpillFloatBits".to_owned()),
        ),
        (
            "value".to_owned(),
            Value::String("3ff0000000000000".to_owned()),
        ),
        ("payload".to_owned(), Value::List(numeric_values())),
    ]);
    let a = NodeValue {
        element_id: "n:9007199254740993".to_owned(),
        labels: vec!["Synthetic".to_owned()],
        properties: properties.clone(),
    };
    let b = NodeValue {
        element_id: "n:9007199254740994".to_owned(),
        labels: vec![],
        properties: BTreeMap::new(),
    };
    let r = RelationshipValue {
        element_id: "r:9007199254740993".to_owned(),
        relationship_type: "SYNTHETIC".to_owned(),
        start: a.element_id.clone(),
        end: b.element_id.clone(),
        properties,
    };
    Value::List(vec![
        Value::Node(a.clone()),
        Value::Relationship(r.clone()),
        Value::Path(PathValue {
            nodes: vec![a, b],
            relationships: vec![r],
        }),
    ])
}

#[test]
fn float_markers_do_not_reinterpret_reserved_user_maps() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let values = vec![
        reserved_map(),
        graph_payload(),
        Value::Map(BTreeMap::from([
            (
                "$type".to_owned(),
                Value::String("SpillFloatBits".to_owned()),
            ),
            (
                "value".to_owned(),
                Value::String("3ff0000000000000".to_owned()),
            ),
        ])),
    ];
    let actual = decode_row(&encode_row(&values, &control).unwrap(), &control).unwrap();
    for (actual, expected) in actual.iter().zip(&values) {
        assert_value_exact(actual, expected);
    }
}

#[test]
fn finite_tags_keep_distinct_point_signed_zero_and_public_shapes() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let point = |x| Value::Point(PointValue::new("cartesian", vec![x, 1.0]).unwrap());
    assert_eq!(
        distinct_row_key(&[point(0.0)], Some(&control)).unwrap(),
        distinct_row_key(&[point(-0.0)], Some(&control)).unwrap()
    );
    assert!(cypher::encode_json(&Value::Float(51.248_178_375_505_404)).is_number());
    let encoded = encode_row(&[Value::Float(51.248_178_375_505_404)], &control).unwrap();
    assert!(encoded.contains("~F"));
}

#[test]
fn finite_metadata_walks_poll_cancellation_and_reject_corrupt_bits() {
    use std::cell::Cell;
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 2
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let mut numbers = json!(vec![1.25; 4096]);
    assert_eq!(
        finite::encode(&mut numbers, Some(&control))
            .unwrap_err()
            .kind,
        QueryErrorKind::Interrupted
    );
    checks.set(0);
    let mut encoded = json!(vec!["~F3ff0000000000000"; 4096]);
    let decode_control = ExecutionControl::new(&resources, &interrupted);
    assert_eq!(
        finite::decode(&mut encoded, &decode_control)
            .unwrap_err()
            .kind,
        QueryErrorKind::Interrupted
    );
    let control = ExecutionControl::new(&resources, &|| false);
    for invalid in ["xyz", "7ff0000000000000", "7ff8000000000000"] {
        let encoded = format!(r#"["~F{invalid}"]"#);
        assert_eq!(
            decode_row(&encoded, &control).unwrap_err().kind,
            QueryErrorKind::Internal
        );
    }
}

fn compact_vectors(coordinate: VectorCoordinateType, total: usize) -> Vec<Value> {
    let mut values = Vec::new();
    let mut remaining = total;
    while remaining > 0 {
        let count = remaining.min(4096);
        let packed = match coordinate {
            VectorCoordinateType::I8 => VectorValues::I8(vec![127; count]),
            VectorCoordinateType::I16 => VectorValues::I16(vec![32_767; count]),
            VectorCoordinateType::F32 => VectorValues::F32(vec![1.25; count]),
            VectorCoordinateType::F64 => VectorValues::F64(vec![51.248_178_375_505_404; count]),
            _ => unreachable!("compact coordinate fixture"),
        };
        values.push(Value::Vector(VectorValue::new(coordinate, packed).unwrap()));
        remaining -= count;
    }
    values
}

#[test]
fn compact_float_vectors_prepay_actual_dom_and_text_scratch() {
    for coordinate in [VectorCoordinateType::F32, VectorCoordinateType::F64] {
        for total in [2048, 16_384] {
            let values = compact_vectors(coordinate, total);
            let resources = QueryResources::default();
            let control = ExecutionControl::new(&resources, &|| false);
            let raw = crate::query::resources::row_bytes(&values).unwrap();
            let base = resources
                .reserve(super::super::serialization_bytes(raw).unwrap())
                .unwrap();
            let tree = resources
                .reserve(
                    dom_supplement(
                        crate::query::resources::encoded_row_bytes_controlled(&values, &control)
                            .unwrap(),
                        raw,
                    )
                    .unwrap(),
                )
                .unwrap();
            let mut dom = JsonValue::Array(
                values
                    .iter()
                    .map(|value| encode_value(value, Some(&control)))
                    .collect::<QueryResult<Vec<_>>>()
                    .unwrap(),
            );
            let extra = resources
                .reserve(finite::encoding_bytes(&dom, Some(&control)).unwrap())
                .unwrap();
            finite::encode(&mut dom, Some(&control)).unwrap();
            let encoded = serde_json::to_string(&dom).unwrap();
            let dom_bytes = crate::query::resources::json_bytes(&dom).unwrap();
            let actual = dom_bytes + encoded.capacity() as u64;
            eprintln!(
                "compact {coordinate:?} coords={total} raw={raw} dom={dom_bytes} text_capacity={} base={} tree={} extra={} actual={actual}",
                encoded.capacity(),
                base.bytes(),
                tree.bytes(),
                extra.bytes()
            );
            assert!(actual <= base.bytes() + tree.bytes() + extra.bytes());
            assert!(
                encoded.capacity() as u64 <= base.bytes(),
                "final text remains covered after DOM scratch release"
            );
            drop(dom);
            drop(extra);
            drop(tree);
            assert_eq!(resources.used(), base.bytes());
            drop(encoded);
            drop(base);
            assert_eq!(resources.used(), 0);
        }
    }
}

#[test]
fn compact_vector_marker_budget_rejects_before_temp_insert() {
    use crate::query::spill::RowSpill;
    let db = open_spill_connection().unwrap();
    let values = compact_vectors(VectorCoordinateType::F32, 16_384);
    let raw = crate::query::resources::row_bytes(&values).unwrap();
    let base_bytes = super::super::serialization_bytes(raw).unwrap();
    let unbounded = QueryResources::default();
    let estimate_control = ExecutionControl::new(&unbounded, &|| false);
    let tree_bytes = dom_supplement(
        crate::query::resources::encoded_row_bytes_controlled(&values, &estimate_control).unwrap(),
        raw,
    )
    .unwrap();
    let resources = QueryResources::new(Some(base_bytes + tree_bytes));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill = RowSpill::create_with_resources(&db, resources.clone()).unwrap();
    assert_eq!(
        spill.push(&db, &values, &control).unwrap_err().kind,
        QueryErrorKind::Resource
    );
    assert_eq!(spill.sequence, 0);
    let rows: i64 = db
        .query_row(
            &format!("SELECT count(*) FROM temp.{}", spill.table),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(resources.peak(), base_bytes + tree_bytes);
    assert_eq!(resources.used(), 0);
    super::super::drop_temp_table(&db, &spill.table).unwrap();
}

#[test]
fn compact_integer_vectors_prepay_initial_json_before_temp_insert() {
    use crate::query::spill::RowSpill;
    for coordinate in [VectorCoordinateType::I8, VectorCoordinateType::I16] {
        let db = open_spill_connection().unwrap();
        let values = compact_vectors(coordinate, 16_384);
        let raw = crate::query::resources::row_bytes(&values).unwrap();
        let base = super::super::serialization_bytes(raw).unwrap();
        let estimate_resources = QueryResources::default();
        let estimate_control = ExecutionControl::new(&estimate_resources, &|| false);
        let tree =
            crate::query::resources::encoded_row_bytes_controlled(&values, &estimate_control)
                .unwrap();
        assert!(tree > base, "{coordinate:?} expands beyond old raw bound");
        let limited = QueryResources::new(Some(base));
        let control = ExecutionControl::new(&limited, &|| false);
        let mut spill = RowSpill::create_with_resources(&db, limited.clone()).unwrap();
        assert_eq!(
            spill.push(&db, &values, &control).unwrap_err().kind,
            QueryErrorKind::Resource
        );
        assert_eq!(spill.sequence, 0);
        let rows: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM temp.{}", spill.table),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(limited.peak(), base);
        assert_eq!(limited.used(), 0);
        super::super::drop_temp_table(&db, &spill.table).unwrap();

        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let base_lease = resources.reserve(base).unwrap();
        let encoded = encode_row(&values, &control).unwrap();
        let decoded = decode_row(&encoded, &control).unwrap();
        assert_eq!(decoded, values);
        assert_eq!(resources.used(), base);
        drop(base_lease);
        assert_eq!(resources.used(), 0);
        eprintln!("compact {coordinate:?} coords=16384 raw={raw} base={base} tree={tree}");
    }
}

#[test]
fn row_codec_preserves_numeric_variants_bits_and_structured_payloads() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut expected = numeric_values();
    expected.push(structured_payload());
    let encoded = encode_row(&expected, &control).unwrap();
    let actual = decode_row(&encoded, &control).unwrap();
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_value_exact(actual, expected);
    }
}

#[test]
fn real_binding_temp_roundtrip_preserves_identity_metadata_and_scalar_bits() {
    let db = open_spill_connection().unwrap();
    let resources = QueryResources::new(Some(4 * 1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let id = 9_007_199_254_740_993;
    let relationship = RelationshipRecord {
        id,
        source: id,
        type_id: i64::MAX - 1,
        target: i64::MAX,
    };
    let mut expected = BindingRow::default();
    expected.insert("n".to_owned(), BindingValue::Node(id));
    expected.insert("r".to_owned(), BindingValue::Relationship(relationship));
    expected.insert(
        "p".to_owned(),
        BindingValue::Path {
            nodes: vec![id, i64::MAX],
            relationships: vec![relationship],
        },
    );
    expected.insert(
        "payload".to_owned(),
        BindingValue::Scalar(structured_payload()),
    );
    expected.insert("null".to_owned(), BindingValue::Null);
    expected.insert(
        "~F3ff0000000000000".to_owned(),
        BindingValue::Scalar(Value::Integer(4)),
    );
    expected.used_relationships.extend([id, i64::MAX]);
    expected.load_csv_context = Some(LoadCsvContext {
        file: Some("~F3ff0000000000000".to_owned()),
        line: Some(i64::MAX),
    });
    let mut spill = BindingSpill::create_with_resources(&db, resources.clone()).unwrap();
    spill.push(&db, &expected, &control).unwrap();
    let (batch, memory) = spill.read_batch_reserved(&db, -1, 1, &control).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].0, 0);
    let actual = &batch[0].1;
    assert_eq!(actual.order, expected.order);
    assert_eq!(actual.used_relationships, expected.used_relationships);
    assert_eq!(actual.load_csv_context, expected.load_csv_context);
    assert_eq!(actual.values["n"], BindingValue::Node(id));
    assert_eq!(actual.values["r"], BindingValue::Relationship(relationship));
    assert_eq!(actual.values["p"], expected.values["p"]);
    assert_eq!(actual.values["null"], BindingValue::Null);
    assert_eq!(
        actual.values["~F3ff0000000000000"],
        BindingValue::Scalar(Value::Integer(4))
    );
    let (BindingValue::Scalar(actual), BindingValue::Scalar(expected)) =
        (&actual.values["payload"], &expected.values["payload"])
    else {
        panic!("scalar payload");
    };
    assert_value_exact(actual, expected);
    assert!(resources.used() > 0);
    drop(batch);
    drop(memory);
    assert_eq!(resources.used(), 0);
    spill.abort(&db).unwrap();
}
