use super::{has_finite_nonzero_norm, vector_similarity_numbers};

#[test]
fn vector_conversion_refuses_before_allocating_and_checks_cancel_in_coordinates() {
    use crate::cypher::Value;
    use crate::query::{ExecutionControl, QueryErrorKind, QueryResources};
    use std::cell::Cell;

    let input = Value::List(vec![Value::Integer(1); 65_536]);
    let resources = QueryResources::new(Some(4_096));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let error = super::try_vector_numbers_controlled(&input, Some(&control))
        .expect_err("preflight before allocating f32 vector");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.peak(), 0);
    assert_eq!(resources.used(), 0);

    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupt = || {
        checks.set(checks.get() + 1);
        checks.get() >= 4
    };
    let control = ExecutionControl::new(&resources, &interrupt);
    let error = super::try_vector_numbers_controlled(&input, Some(&control))
        .expect_err("coordinate traversal must cancel");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 4);
    assert_eq!(resources.used(), 0);
}

#[test]
fn fulltext_candidates_observe_budget_before_outer_limit_and_release_on_failure() {
    use crate::query::{ExecutionControl, QueryErrorKind, QueryResources};
    use crate::storage::{IndexConfiguration, IndexDefinition, IndexTarget, StandardIndexKind};
    use rusqlite::Connection;

    let connection = Connection::open_in_memory().expect("SQLite");
    connection.execute_batch("CREATE VIRTUAL TABLE temp.resource_fts USING fts5(c0,owner_id UNINDEXED); WITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n<4096) INSERT INTO temp.resource_fts(c0,owner_id) SELECT 'novel',n FROM seq;").expect("synthetic fulltext documents");
    let index = IndexDefinition {
        name: "documents".to_owned(),
        kind: StandardIndexKind::FullText,
        target: IndexTarget::NodeProperties {
            label: "Document".to_owned(),
            properties: vec!["text".to_owned()],
        },
        owning_constraint: None,
        labels_or_types: vec!["Document".to_owned()],
        additional_properties: Vec::new(),
        configuration: Some(IndexConfiguration::FullText {
            analyzer: "unicode61".to_owned(),
            eventually_consistent: false,
        }),
    };
    let resources = QueryResources::new(Some(1_024));
    let never = || false;
    let control = ExecutionControl::new(&resources, &never);
    let error = super::default_fulltext_candidates(
        &connection,
        &index,
        "novel",
        "resource_fts",
        Some(&control),
    )
    .err()
    .expect("full candidate collection must respect budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert!(resources.peak() <= 1_024);
    assert_eq!(resources.used(), 0);
    let resources = QueryResources::new(Some(1_024 * 1_024));
    let control = ExecutionControl::new(&resources, &never);
    let (hits, memory) = super::default_fulltext_candidates(
        &connection,
        &index,
        "novel",
        "resource_fts",
        Some(&control),
    )
    .expect("larger candidate budget");
    assert_eq!(hits.len(), 4096);
    assert!(resources.used() > 0);
    drop(hits);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn search_visibility_checks_current_control_inside_node_label_scan() {
    use super::*;
    use crate::query::options::GraphViewSelector;
    use crate::query::{QueryErrorKind, QueryResources};
    use crate::storage::{CommitMetadata, LayerBuilder};
    use std::cell::Cell;

    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    storage::initialize_root(&connection).unwrap();
    let head = storage::branch_head(&connection, "main").unwrap();
    let node = storage::allocate_node_id(&connection).unwrap();
    let mut layer = LayerBuilder::default();
    layer.add_node(node).unwrap();
    for ordinal in 0..64 {
        let label = storage::intern_label(&connection, &format!("L{ordinal}")).unwrap();
        layer.add_label(node, label).unwrap();
    }
    let commit = storage::commit_layer(
        &connection,
        "main",
        head,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    let snapshot = Snapshot::resolve(&connection, commit).unwrap();
    let view = ResolvedGraphView::resolve(
        &connection,
        &GraphViewSelector {
            require_all_labels: BTreeSet::from(["L0".to_owned()]),
            exclude_any_labels: BTreeSet::new(),
        },
    )
    .unwrap();
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 8
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = visible_fulltext_entity(&snapshot, &view, false, node, Some(&control))
        .expect_err("search visibility must not drop the current label-scan cancellation callback");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 8);
    assert_eq!(resources.used(), 0);
}

#[test]
fn cosine_extreme_finite_vectors_keep_finite_scores() {
    let magnitude = 1.0e20_f32;
    let identical =
        vector_similarity_numbers(&[magnitude, magnitude], &[magnitude, magnitude], "cosine")
            .expect("large identical cosine");
    assert!(identical.is_finite());
    assert!((identical - 1.0).abs() < f64::EPSILON);

    let orthogonal = vector_similarity_numbers(&[magnitude, 0.0], &[0.0, magnitude], "cosine")
        .expect("large orthogonal cosine");
    assert!(orthogonal.is_finite());
    assert!((orthogonal - 0.5).abs() < f64::EPSILON);
    assert!(has_finite_nonzero_norm(&[magnitude, magnitude]));
}

#[test]
fn cosine_zero_vector_remains_invalid() {
    assert!(!has_finite_nonzero_norm(&[0.0, 0.0]));
    assert!(vector_similarity_numbers(&[0.0, 0.0], &[1.0, 0.0], "cosine").is_err());
}

#[test]
fn euclidean_extreme_finite_vectors_keep_finite_nonzero_scores() {
    let magnitude = 1.0e20_f32;
    let score = vector_similarity_numbers(&[magnitude, 0.0], &[-magnitude, 0.0], "euclidean")
        .expect("large euclidean");
    assert!(score.is_finite());
    assert!(score > 0.0);
    assert!(score < 1.0);
}

#[test]
fn fulltext_transition_checks_cancellation_between_ignored_indexes() {
    use super::*;
    use std::cell::Cell;
    let mut next = SchemaState::default();
    for ordinal in 0..512 {
        let name = format!("index_{ordinal}");
        next.indexes.insert(
            name.clone(),
            IndexDefinition {
                name,
                kind: StandardIndexKind::Range,
                target: IndexTarget::NodeProperties {
                    label: "Document".to_owned(),
                    properties: vec!["title".to_owned()],
                },
                owning_constraint: None,
                labels_or_types: Vec::new(),
                additional_properties: Vec::new(),
                configuration: None,
            },
        );
    }
    let calls = Cell::new(0);
    let interrupted = || {
        calls.set(calls.get() + 1);
        calls.get() == 3
    };
    let connection = Connection::open_in_memory().unwrap();
    let error = validate_fulltext_transition_controlled(
        &connection,
        &SchemaState::default(),
        &next,
        &interrupted,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(calls.get(), 3);
}
