use std::cell::Cell;
use std::collections::BTreeSet;

use super::*;
use crate::cypher;
use crate::query::plan::join::PartSet;
use crate::query::{ExecutionOptions, QueryErrorKind, QueryResources};
use crate::storage::{
    self, CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord,
};
use rusqlite::Connection;

fn numeric_values() -> Vec<Value> {
    vec![
        Value::Integer(i64::MIN),
        Value::Integer(i64::MIN + 1),
        Value::Integer(-9_007_199_254_740_993),
        Value::Integer(-9_007_199_254_740_992),
        Value::Integer(-1),
        Value::Integer(0),
        Value::Integer(1),
        Value::Integer(9_007_199_254_740_991),
        Value::Integer(9_007_199_254_740_992),
        Value::Integer(9_007_199_254_740_993),
        Value::Integer(i64::MAX),
        Value::Float(i64::MIN as f64),
        Value::Float(f64::from_bits((i64::MIN as f64).to_bits() - 1)),
        Value::Float(-9_007_199_254_740_992.0),
        Value::Float(-1.0),
        Value::Float(-0.0),
        Value::Float(0.0),
        Value::Float(1.0),
        Value::Float(1.5),
        Value::Float(9_007_199_254_740_991.0),
        Value::Float(9_007_199_254_740_992.0),
        Value::Float(9_007_199_254_740_994.0),
        Value::Float(f64::from_bits((i64::MAX as f64).to_bits() - 1)),
        Value::Float(i64::MAX as f64),
        Value::Float(f64::from_bits(1)),
        Value::Float(-f64::from_bits(1)),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        Value::Float(f64::NAN),
        Value::Float(f64::from_bits(0x7ff8_0000_0000_0001)),
        Value::Float(f64::from_bits(0xfff8_0000_0000_0001)),
        Value::Null,
    ]
}

#[test]
fn numeric_true_equality_implies_the_same_digest_at_precision_boundaries() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    for left in numeric_values() {
        let left_key = HashKeyValue::Scalar(left.clone());
        let left_digest = digest_key(&left_key, &control).unwrap();
        assert_eq!(
            left_digest.is_none(),
            matches!(&left, Value::Null) || matches!(&left, Value::Float(value) if value.is_nan()),
            "{left:?}"
        );
        for right in numeric_values() {
            let right_key = HashKeyValue::Scalar(right.clone());
            let expected = cypher::cypher_equals(&left, &right).unwrap();
            assert_eq!(
                hash_keys_equal(&left_key, &right_key, &control).unwrap(),
                expected,
                "{left:?} / {right:?}"
            );
            if expected == Some(true) {
                assert!(left_digest.is_some());
                assert_eq!(
                    left_digest,
                    digest_key(&right_key, &control).unwrap(),
                    "{left:?} / {right:?}"
                );
            }
        }
    }
    let integer = HashKeyValue::Scalar(Value::Integer(9_007_199_254_740_993));
    let rounded = HashKeyValue::Scalar(Value::Float(9_007_199_254_740_992.0));
    assert_ne!(
        digest_key(&integer, &control).unwrap(),
        digest_key(&rounded, &control).unwrap()
    );
    assert_eq!(resources.used(), 0);
}

#[test]
fn scalar_and_entity_domains_are_separate_and_strings_use_exact_utf8() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let values = [
        HashKeyValue::Scalar(Value::Boolean(true)),
        HashKeyValue::Scalar(Value::Boolean(false)),
        HashKeyValue::Scalar(Value::Integer(1)),
        HashKeyValue::Scalar(Value::String("1".to_owned())),
        HashKeyValue::NodeId(1),
        HashKeyValue::RelationshipId(1),
        HashKeyValue::Scalar(Value::String("é".to_owned())),
        HashKeyValue::Scalar(Value::String("e\u{301}".to_owned())),
        HashKeyValue::Scalar(Value::String("a\0b".to_owned())),
        HashKeyValue::Scalar(Value::String("a".to_owned())),
    ];
    let mut digests = BTreeSet::new();
    for value in &values {
        let digest = digest_key(value, &control).unwrap().unwrap();
        assert!(
            digests.insert(digest),
            "unexpected shared domain: {value:?}"
        );
        assert_eq!(hash_keys_equal(value, value, &control).unwrap(), Some(true));
    }
    assert_eq!(
        hash_keys_equal(&values[4], &values[5], &control).unwrap(),
        Some(false)
    );
    for value in [Value::List(vec![]), Value::Map(Default::default())] {
        assert_eq!(
            digest_key(&HashKeyValue::Scalar(value), &control)
                .unwrap_err()
                .kind,
            QueryErrorKind::Internal
        );
    }
}

#[test]
fn a_forced_full_digest_collision_still_uses_typed_equality_and_owned_leases() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let left = EvaluatedHashKey {
        digest: [0xa5; 32],
        value: HashKeyValue::Scalar(Value::String("left".to_owned())),
        memory: control.reserve(128).unwrap(),
    };
    let right = EvaluatedHashKey {
        digest: [0xa5; 32],
        value: HashKeyValue::Scalar(Value::String("right".to_owned())),
        memory: control.reserve(192).unwrap(),
    };
    assert_eq!(left.digest, right.digest);
    assert_eq!(
        hash_keys_equal(&left.value, &right.value, &control).unwrap(),
        Some(false)
    );
    assert_eq!(resources.used(), left.memory.bytes() + right.memory.bytes());
    drop(left);
    assert_eq!(resources.used(), right.memory.bytes());
    drop(right);
    assert_eq!(resources.used(), 0);
}

#[test]
fn long_string_hashing_and_collision_comparison_poll_cancellation() {
    let resources = QueryResources::default();
    let key = HashKeyValue::Scalar(Value::String("é".repeat(2 * 1024 * 1024)));
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 2
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    assert_eq!(
        digest_key(&key, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert!(checks.get() >= 2);
    checks.set(0);
    let control = ExecutionControl::new(&resources, &interrupted);
    assert_eq!(
        hash_keys_equal(&key, &key, &control).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert!(checks.get() >= 2);
    assert_eq!(resources.used(), 0);
}

fn key(variable: &str, property: Option<&str>) -> HashInputKey {
    HashInputKey {
        variable: variable.to_owned(),
        property: property.map(str::to_owned),
        providers: PartSet(1),
    }
}

fn graph_fixture() -> (Connection, i64, RelationshipRecord, String) {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let node = storage::allocate_node_id(&db).unwrap();
    let target = storage::allocate_node_id(&db).unwrap();
    let relationship = RelationshipRecord {
        id: storage::allocate_relationship_id(&db).unwrap(),
        source: node,
        target,
        type_id: storage::intern_relationship_type(&db, "LINK").unwrap(),
    };
    let property = storage::intern_property_key(&db, "payload").unwrap();
    let payload = "leased-é".repeat(1024);
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    layer.add_node(target).unwrap();
    layer.add_relationship(relationship).unwrap();
    for (kind, id) in [
        (OwnerKind::Node, node),
        (OwnerKind::Relationship, relationship.id),
    ] {
        layer
            .set_property(kind, id, property, PropertyValue::String(payload.clone()))
            .unwrap();
    }
    storage::commit_layer(
        &db,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    (db, node, relationship, payload)
}

#[test]
fn extraction_reads_real_properties_but_identity_and_masking_do_not_add_hits() {
    let (db, node, relationship, payload) = graph_fixture();
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let view = ResolvedGraphView::default();
    let mut row = BindingRow::default();
    row.insert("n".to_owned(), BindingValue::Node(node));
    row.insert("r".to_owned(), BindingValue::Relationship(relationship));
    for (variable, family) in [
        ("n", HashFamily::NodeIdentity),
        ("r", HashFamily::RelationshipIdentity),
    ] {
        let mut metrics = QueryMetrics::default();
        let extracted = extract_hash_key(
            &key(variable, None),
            family,
            &row,
            &snapshot,
            &view,
            &control,
            &mut metrics,
        )
        .unwrap()
        .unwrap();
        assert_eq!(metrics.db_hits, 0);
        assert_eq!(extracted.memory.bytes(), 0);
        let expected = if variable == "n" {
            HashKeyValue::NodeId(node)
        } else {
            HashKeyValue::RelationshipId(relationship.id)
        };
        assert_eq!(
            extracted.digest,
            digest_key(&expected, &control).unwrap().unwrap()
        );
        drop(extracted);
    }
    for variable in ["n", "r"] {
        let mut metrics = QueryMetrics::default();
        let extracted = extract_hash_key(
            &key(variable, Some("payload")),
            HashFamily::String,
            &row,
            &snapshot,
            &view,
            &control,
            &mut metrics,
        )
        .unwrap()
        .unwrap();
        assert_eq!(metrics.db_hits, 1);
        assert!(extracted.memory.bytes() >= payload.len() as u64);
        assert_eq!(resources.used(), extracted.memory.bytes());
        assert!(
            matches!(&extracted.value, HashKeyValue::Scalar(Value::String(value)) if value == &payload)
        );
        drop(extracted);
        assert_eq!(resources.used(), 0);
    }
    let options =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["payload"]}}}"#)
            .unwrap();
    let masked = ResolvedGraphView::default().with_query_access(options.access.read.query);
    let mut metrics = QueryMetrics::default();
    assert!(
        extract_hash_key(
            &key("n", Some("payload")),
            HashFamily::String,
            &row,
            &snapshot,
            &masked,
            &control,
            &mut metrics
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(metrics.db_hits, 0);
    assert_eq!(resources.used(), 0);
}

#[test]
fn runtime_proof_mismatches_are_errors_and_release_extracted_values() {
    let (db, node, _, _) = graph_fixture();
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let view = ResolvedGraphView::default();
    let mut row = BindingRow::default();
    row.insert("n".to_owned(), BindingValue::Node(node));
    row.insert("scalar".to_owned(), BindingValue::Scalar(Value::Integer(1)));
    for (key, family, hits) in [
        (key("n", Some("payload")), HashFamily::Number, 1),
        (key("n", None), HashFamily::RelationshipIdentity, 0),
        (key("scalar", Some("payload")), HashFamily::Number, 0),
        (key("missing", None), HashFamily::NodeIdentity, 0),
    ] {
        let mut metrics = QueryMetrics::default();
        assert_eq!(
            extract_hash_key(&key, family, &row, &snapshot, &view, &control, &mut metrics)
                .unwrap_err()
                .kind,
            QueryErrorKind::Internal
        );
        assert_eq!(metrics.db_hits, hits);
        assert_eq!(resources.used(), 0);
    }
    for null in [BindingValue::Null, BindingValue::Scalar(Value::Null)] {
        row.insert("null".to_owned(), null);
        let mut metrics = QueryMetrics::default();
        assert!(
            extract_hash_key(
                &key("null", Some("payload")),
                HashFamily::Number,
                &row,
                &snapshot,
                &view,
                &control,
                &mut metrics
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(metrics.db_hits, 0);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn property_extraction_preserves_resource_rejection_and_releases_claims() {
    let (db, node, _, _) = graph_fixture();
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let resources = QueryResources::new(Some(1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let view = ResolvedGraphView::default();
    let mut row = BindingRow::default();
    row.insert("n".to_owned(), BindingValue::Node(node));
    let mut metrics = QueryMetrics::default();
    assert_eq!(
        extract_hash_key(
            &key("n", Some("payload")),
            HashFamily::String,
            &row,
            &snapshot,
            &view,
            &control,
            &mut metrics
        )
        .unwrap_err()
        .kind,
        QueryErrorKind::Resource
    );
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() <= 1024);
}
