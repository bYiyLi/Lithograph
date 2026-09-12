use super::*;
use lithograph_core::cypher::{VectorCoordinateType, VectorValue, VectorValues};

#[test]
fn property_type_unions_are_normalized_and_preserve_non_null_semantics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Metric => {value :: ANY<FLOAT NOT NULL | INTEGER NOT NULL | FLOAT NOT NULL>, embedding :: VECTOR<FLOAT32>(3)}) }",
        ExecutionOptions::default(),
    )
    .expect("normalized non-null union");

    let schema = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("schema head"),
    )
    .expect("schema");
    let rule = &schema
        .graph_nodes
        .get("Metric")
        .expect("Metric Graph Node Type")
        .properties["value"];
    assert!(rule.required);
    assert!(matches!(
        &rule.property_type,
        PropertyType::Union { members }
            if matches!(members.as_slice(), [PropertyType::Integer, PropertyType::Float])
    ));

    let before = branch_head(&connection, "main").expect("head before missing value");
    let error = execute(
        &connection,
        "CREATE (:Metric) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("non-null union must require the property");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after missing value"),
        before
    );

    let (shown, _) = execute(
        &connection,
        "SHOW CURRENT GRAPH TYPE",
        ExecutionOptions::default(),
    )
    .expect("show normalized graph type");
    let Value::String(specification) = &shown[0][0] else {
        panic!("graph type specification must be a string")
    };
    assert!(specification.contains("`value` :: INTEGER NOT NULL | FLOAT NOT NULL"));
    assert!(specification.contains("`embedding` :: VECTOR<FLOAT32>(3)"));

    let round_trip = fresh_storage();
    execute(
        &round_trip,
        &format!("ALTER CURRENT GRAPH TYPE SET {specification}"),
        ExecutionOptions::default(),
    )
    .expect("normalized graph type must round-trip");
    assert_eq!(
        SchemaState::load(
            &round_trip,
            branch_head(&round_trip, "main").expect("round-trip head")
        )
        .expect("round-trip schema"),
        schema
    );
}

#[test]
fn property_type_constraints_reject_conflicting_types() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_integer FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
        ExecutionOptions::default(),
    )
    .expect("initial property type constraint");

    for query in [
        "CREATE CONSTRAINT metric_value_float FOR (n:Metric) REQUIRE n.value IS :: FLOAT",
        "CREATE CONSTRAINT metric_value_float IF NOT EXISTS FOR (n:Metric) REQUIRE n.value IS :: FLOAT",
    ] {
        let before = branch_head(&connection, "main").expect("head before conflicting type");
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("different property types on the same schema must conflict");
        assert_eq!(error.kind, QueryErrorKind::Schema);
        assert_eq!(
            branch_head(&connection, "main").expect("head after conflicting type"),
            before
        );
    }

    let before = branch_head(&connection, "main").expect("head before equivalent type");
    let before_schema =
        SchemaState::load(&connection, before).expect("schema before equivalent type");
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_integer_copy IF NOT EXISTS FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
        ExecutionOptions::default(),
    )
    .expect("equivalent IF NOT EXISTS is a no-op");
    let after = branch_head(&connection, "main").expect("head after equivalent type");
    assert_ne!(
        after, before,
        "successful schema command records write intent"
    );
    assert_eq!(
        SchemaState::load(&connection, after).expect("schema after equivalent type"),
        before_schema
    );
}

#[test]
fn vector_property_type_aliases_canonicalize_and_validate_values() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Embedding => {value :: VECTOR<INT8>(2)}) }",
        ExecutionOptions::default(),
    )
    .expect("Vector coordinate alias");

    let schema = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("vector schema head"),
    )
    .expect("vector schema");
    assert!(matches!(
        &schema.graph_nodes["Embedding"].properties["value"].property_type,
        PropertyType::Vector {
            coordinate,
            dimension: 2
        } if coordinate == "INTEGER8"
    ));

    let valid = Value::Vector(
        VectorValue::new(VectorCoordinateType::I8, VectorValues::I8(vec![1, 2]))
            .expect("I8 vector"),
    );
    execute_params(
        &connection,
        "CREATE (:Embedding {value:$value}) FINISH",
        BTreeMap::from([("value".to_owned(), valid)]),
        ExecutionOptions::default(),
    )
    .expect("canonicalized alias must accept matching Vector value");

    let (shown, _) = execute(
        &connection,
        "SHOW CURRENT GRAPH TYPE",
        ExecutionOptions::default(),
    )
    .expect("show vector alias");
    assert!(matches!(
        &shown[0][0],
        Value::String(specification) if specification.contains("VECTOR<INTEGER8>(2)")
    ));
}

#[test]
fn unique_constraint_uses_cypher_numeric_equality_for_scalars() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_unique FOR (n:Metric) REQUIRE n.value IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("unique constraint");
    execute(
        &connection,
        "CREATE (:Metric {value:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("integer value");
    let before = branch_head(&connection, "main").expect("head before conflict");

    let error = execute(
        &connection,
        "CREATE (:Metric {value:1.0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("Cypher numeric equality must reject 1 and 1.0 as duplicates");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after conflict"),
        before
    );

    execute(
        &connection,
        "CREATE CONSTRAINT precise_value_unique FOR (n:Precise) REQUIRE n.value IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("precision constraint");
    execute(
        &connection,
        "CREATE (:Precise {value:9007199254740993}), (:Precise {value:9007199254740992.0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("distinct integer and rounded float must remain distinct");
}

#[test]
fn unique_constraint_uses_cypher_numeric_equality_for_lists_and_composite_keys() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT list_value_unique FOR (n:ListMetric) REQUIRE n.value IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("list unique constraint");
    execute(
        &connection,
        "CREATE (:ListMetric {value:[1]}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("list integer value");
    let before_list = branch_head(&connection, "main").expect("head before list conflict");
    let error = execute(
        &connection,
        "CREATE (:ListMetric {value:[1.0]}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("list numeric equality must reject [1] and [1.0] as duplicates");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after list conflict"),
        before_list
    );

    execute(
        &connection,
        "CREATE CONSTRAINT composite_key FOR (n:Scoped) REQUIRE (n.scope, n.value) IS NODE KEY",
        ExecutionOptions::default(),
    )
    .expect("composite key constraint");
    execute(
        &connection,
        "CREATE (:Scoped {scope:'a', value:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("composite key seed");
    let before_composite =
        branch_head(&connection, "main").expect("head before composite conflict");
    let error = execute(
        &connection,
        "CREATE (:Scoped {scope:'a', value:1.0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("composite key must use numeric equality for each component");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after composite conflict"),
        before_composite
    );
}

#[test]
fn unique_constraint_allows_non_reflexive_nan_values() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT nan_value_unique FOR (n:NanMetric) REQUIRE n.value IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("NaN unique constraint");
    let nan_params = BTreeMap::from([("value".to_owned(), Value::Float(f64::NAN))]);
    execute_params(
        &connection,
        "CREATE (:NanMetric {value:$value}) FINISH",
        nan_params.clone(),
        ExecutionOptions::default(),
    )
    .expect("first NaN value");
    execute_params(
        &connection,
        "CREATE (:NanMetric {value:$value}) FINISH",
        nan_params,
        ExecutionOptions::default(),
    )
    .expect("NaN does not equal itself and must not conflict under Cypher equality");
}

#[test]
fn unique_constraint_normalizes_point_signed_zero() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT point_value_unique FOR (n:PointMetric) REQUIRE n.value IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("Point unique constraint");
    let positive_zero = BTreeMap::from([(
        "value".to_owned(),
        Value::Point(PointValue::new("cartesian", vec![0.0, 1.0]).expect("point")),
    )]);
    let negative_zero = BTreeMap::from([(
        "value".to_owned(),
        Value::Point(PointValue::new("cartesian", vec![-0.0, 1.0]).expect("point")),
    )]);
    execute_params(
        &connection,
        "CREATE (:PointMetric {value:$value}) FINISH",
        positive_zero,
        ExecutionOptions::default(),
    )
    .expect("positive-zero Point");
    let before_point = branch_head(&connection, "main").expect("head before Point conflict");
    let error = execute_params(
        &connection,
        "CREATE (:PointMetric {value:$value}) FINISH",
        negative_zero,
        ExecutionOptions::default(),
    )
    .expect_err("Point signed zero coordinates are equal and must conflict");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after Point conflict"),
        before_point
    );
}

#[test]
fn unique_constraint_normalizes_vector_signed_zero() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE CONSTRAINT vector_value_unique FOR (n:VectorMetric) REQUIRE n.value IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("Vector unique constraint");
    let positive_zero = BTreeMap::from([(
        "value".to_owned(),
        Value::Vector(
            VectorValue::new(VectorCoordinateType::F64, VectorValues::F64(vec![0.0, 1.0]))
                .expect("vector"),
        ),
    )]);
    let negative_zero = BTreeMap::from([(
        "value".to_owned(),
        Value::Vector(
            VectorValue::new(
                VectorCoordinateType::F64,
                VectorValues::F64(vec![-0.0, 1.0]),
            )
            .expect("vector"),
        ),
    )]);
    execute_params(
        &connection,
        "CREATE (:VectorMetric {value:$value}) FINISH",
        positive_zero,
        ExecutionOptions::default(),
    )
    .expect("positive-zero Vector");
    let before_vector = branch_head(&connection, "main").expect("head before Vector conflict");
    let error = execute_params(
        &connection,
        "CREATE (:VectorMetric {value:$value}) FINISH",
        negative_zero,
        ExecutionOptions::default(),
    )
    .expect_err("Vector signed zero coordinates are equal and must conflict");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after Vector conflict"),
        before_vector
    );
}

#[test]
fn point_index_equality_preserves_signed_zero_semantics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Place {name:'origin', location:point({x:0.0, y:1.0})}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed point");
    let target = Value::Point(PointValue::new("cartesian", vec![-0.0, 1.0]).expect("point"));
    let params = BTreeMap::from([("target".to_owned(), target)]);
    let query = "MATCH (n:Place) WHERE n.location = $target RETURN n.name";
    let in_query = "MATCH (n:Place) WHERE n.location IN [$target] RETURN n.name";

    let (scan, _) = execute_params(
        &connection,
        query,
        params.clone(),
        ExecutionOptions::default(),
    )
    .expect("point scan");
    assert_eq!(scan, vec![vec![Value::String("origin".to_owned())]]);
    let (in_scan, _) = execute_params(
        &connection,
        in_query,
        params.clone(),
        ExecutionOptions::default(),
    )
    .expect("point IN scan");
    assert_eq!(in_scan, scan);

    execute(
        &connection,
        "CREATE POINT INDEX place_location FOR (n:Place) ON (n.location)",
        ExecutionOptions::default(),
    )
    .expect("point index");
    let (seek, _) = execute_params(
        &connection,
        query,
        params.clone(),
        ExecutionOptions::default(),
    )
    .expect("point seek");
    assert_eq!(seek, scan);
    let (in_seek, _) = execute_params(&connection, in_query, params, ExecutionOptions::default())
        .expect("point IN seek");
    assert_eq!(in_seek, in_scan);
}

#[test]
fn range_index_cache_covers_all_property_value_families() {
    let connection = fresh_storage();
    let seed_params = BTreeMap::from([
        ("list".to_owned(), Value::List(vec![Value::Integer(1)])),
        (
            "point".to_owned(),
            Value::Point(PointValue::new("cartesian", vec![0.0, 1.0]).expect("point")),
        ),
        (
            "vector".to_owned(),
            Value::Vector(
                VectorValue::new(VectorCoordinateType::F64, VectorValues::F64(vec![0.0, 1.0]))
                    .expect("vector"),
            ),
        ),
    ]);
    execute_params(
        &connection,
        "CREATE (:Indexed {name:'list', value:$list}), (:Indexed {name:'point', value:$point}), (:Indexed {name:'vector', value:$vector}) FINISH",
        seed_params,
        ExecutionOptions::default(),
    )
    .expect("seed all range-indexable property families");
    execute(
        &connection,
        "CREATE CONSTRAINT indexed_value_types FOR (n:Indexed) REQUIRE n.value IS :: LIST<INTEGER NOT NULL> | POINT | VECTOR<FLOAT64>(2)",
        ExecutionOptions::default(),
    )
    .expect("type proof for heterogeneous exact equality");

    let existence_query =
        "MATCH (n:Indexed) WHERE n.value IS NOT NULL RETURN n.name ORDER BY n.name";
    let (existence_scan, _) = execute(&connection, existence_query, ExecutionOptions::default())
        .expect("range existence scan baseline");
    assert_eq!(
        existence_scan,
        vec![
            vec![Value::String("list".to_owned())],
            vec![Value::String("point".to_owned())],
            vec![Value::String("vector".to_owned())],
        ]
    );

    let equality_cases = [
        ("list", Value::List(vec![Value::Float(1.0)])),
        (
            "point",
            Value::Point(PointValue::new("cartesian", vec![-0.0, 1.0]).expect("point")),
        ),
        (
            "vector",
            Value::Vector(
                VectorValue::new(
                    VectorCoordinateType::F64,
                    VectorValues::F64(vec![-0.0, 1.0]),
                )
                .expect("vector"),
            ),
        ),
    ];
    for (name, target) in &equality_cases {
        let params = BTreeMap::from([("target".to_owned(), target.clone())]);
        let (rows, _) = execute_params(
            &connection,
            "MATCH (n:Indexed) WHERE n.value = $target RETURN n.name",
            params,
            ExecutionOptions::default(),
        )
        .unwrap_or_else(|error| panic!("{name} range equality scan: {error}"));
        assert_eq!(rows, vec![vec![Value::String((*name).to_owned())]]);
    }

    execute(
        &connection,
        "CREATE RANGE INDEX indexed_value FOR (n:Indexed) ON (n.value)",
        ExecutionOptions::default(),
    )
    .expect("range index");
    let (existence_seek, _) = execute(&connection, existence_query, ExecutionOptions::default())
        .expect("range existence seek");
    assert_eq!(existence_seek, existence_scan);

    for (name, target) in equality_cases {
        let params = BTreeMap::from([("target".to_owned(), target)]);
        let (rows, _) = execute_params(
            &connection,
            "MATCH (n:Indexed) WHERE n.value = $target RETURN n.name",
            params.clone(),
            ExecutionOptions::default(),
        )
        .unwrap_or_else(|error| panic!("{name} range equality seek: {error}"));
        assert_eq!(rows, vec![vec![Value::String(name.to_owned())]]);
        let (plan, _) = execute_params(
            &connection,
            "EXPLAIN MATCH (n:Indexed) WHERE n.value = $target RETURN n.name",
            params,
            ExecutionOptions::default(),
        )
        .unwrap_or_else(|error| panic!("{name} range equality explain: {error}"));
        assert!(matches!(&plan[0][0], Value::String(plan) if plan.contains("indexed_value")));
    }
}

#[test]
fn range_index_vector_equality_preserves_type_errors_without_type_proof() {
    let connection = fresh_storage();
    let stored = Value::Vector(
        VectorValue::new(VectorCoordinateType::F64, VectorValues::F64(vec![1.0, 2.0]))
            .expect("stored vector"),
    );
    execute_params(
        &connection,
        "CREATE (:Indexed {value:$value}) FINISH",
        BTreeMap::from([("value".to_owned(), stored)]),
        ExecutionOptions::default(),
    )
    .expect("seed vector");
    let target = Value::Vector(
        VectorValue::new(VectorCoordinateType::F32, VectorValues::F32(vec![1.0, 2.0]))
            .expect("target vector"),
    );
    let query = "MATCH (n:Indexed) WHERE n.value = $target RETURN n.value";
    let params = BTreeMap::from([("target".to_owned(), target)]);

    let scan_error = execute_params(
        &connection,
        query,
        params.clone(),
        ExecutionOptions::default(),
    )
    .expect_err("incompatible Vector equality scan must fail");
    assert_eq!(scan_error.kind, QueryErrorKind::Type);

    execute(
        &connection,
        "CREATE RANGE INDEX indexed_value FOR (n:Indexed) ON (n.value)",
        ExecutionOptions::default(),
    )
    .expect("range index");
    let indexed_error = execute_params(
        &connection,
        query,
        params.clone(),
        ExecutionOptions::default(),
    )
    .expect_err("Range Index must not suppress incompatible Vector equality errors");
    assert_eq!(indexed_error.kind, QueryErrorKind::Type);

    let (plan, _) = execute_params(
        &connection,
        "EXPLAIN MATCH (n:Indexed) WHERE n.value = $target RETURN n.value",
        params,
        ExecutionOptions::default(),
    )
    .expect("explain");
    assert!(matches!(&plan[0][0], Value::String(plan) if !plan.contains("IndexSeek")));
}

#[test]
fn typed_standard_indexes_do_not_answer_untyped_existence_predicates() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Mixed {name:'string-value', value:'text'}), (:Mixed {name:'number-value', value:1}), (:Mixed {name:'point-location', location:point({x:1.0,y:2.0})}), (:Mixed {name:'string-location', location:'not-a-point'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed heterogeneous properties");

    let value_query = "MATCH (n:Mixed) WHERE n.value IS NOT NULL RETURN n.name ORDER BY n.name";
    let location_query =
        "MATCH (n:Mixed) WHERE n.location IS NOT NULL RETURN n.name ORDER BY n.name";
    let (value_scan, _) = execute(&connection, value_query, ExecutionOptions::default())
        .expect("value existence baseline");
    let (location_scan, _) = execute(&connection, location_query, ExecutionOptions::default())
        .expect("location existence baseline");

    execute(
        &connection,
        "CREATE TEXT INDEX mixed_value_text FOR (n:Mixed) ON (n.value)",
        ExecutionOptions::default(),
    )
    .expect("text index");
    execute(
        &connection,
        "CREATE POINT INDEX mixed_location_point FOR (n:Mixed) ON (n.location)",
        ExecutionOptions::default(),
    )
    .expect("point index");

    let (value_indexed, _) = execute(&connection, value_query, ExecutionOptions::default())
        .expect("value existence after text index");
    let (location_indexed, _) = execute(&connection, location_query, ExecutionOptions::default())
        .expect("location existence after point index");
    assert_eq!(value_indexed, value_scan);
    assert_eq!(location_indexed, location_scan);

    for (query, forbidden_index) in [
        (
            "EXPLAIN MATCH (n:Mixed) WHERE n.value IS NOT NULL RETURN n.name",
            "mixed_value_text",
        ),
        (
            "EXPLAIN MATCH (n:Mixed) WHERE n.location IS NOT NULL RETURN n.name",
            "mixed_location_point",
        ),
    ] {
        let (plan, _) = execute(&connection, query, ExecutionOptions::default()).expect("explain");
        assert!(matches!(
            &plan[0][0],
            Value::String(plan) if !plan.contains(forbidden_index)
        ));
    }
}

#[test]
fn typed_standard_indexes_preserve_type_errors_without_type_proof() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Mixed {name:'typed', text:'alpha', location:point({x:1.0,y:2.0})}), (:Mixed {name:'wrong', text:1, location:'not-a-point'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed heterogeneous typed-index properties");

    let text_query = "MATCH (n:Mixed) WHERE n.text STARTS WITH 'a' RETURN n.name";
    let point_query = "MATCH (n:Mixed) WHERE point.withinBBox(n.location, point({x:0.0,y:0.0}), point({x:2.0,y:3.0})) RETURN n.name";
    for query in [text_query, point_query] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("scan must surface incompatible property types");
        assert_eq!(error.kind, QueryErrorKind::Type, "{query}");
    }

    execute(
        &connection,
        "CREATE TEXT INDEX mixed_text FOR (n:Mixed) ON (n.text)",
        ExecutionOptions::default(),
    )
    .expect("text index");
    execute(
        &connection,
        "CREATE POINT INDEX mixed_location FOR (n:Mixed) ON (n.location)",
        ExecutionOptions::default(),
    )
    .expect("point index");

    for (query, forbidden_index) in [(text_query, "mixed_text"), (point_query, "mixed_location")] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("typed Index must not suppress incompatible property type errors");
        assert_eq!(error.kind, QueryErrorKind::Type, "{query}");
        let (plan, _) = execute(
            &connection,
            &format!("EXPLAIN {query}"),
            ExecutionOptions::default(),
        )
        .expect("explain");
        assert!(matches!(
            &plan[0][0],
            Value::String(plan) if !plan.contains(forbidden_index)
        ));
    }
}

#[test]
fn range_order_seek_requires_property_type_proof() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Mixed {name:'number', value:2}), (:Mixed {name:'string', value:'two'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed heterogeneous range property");
    let query = "MATCH (n:Mixed) WHERE n.value > 1 RETURN n.name";
    let error = execute(&connection, query, ExecutionOptions::default())
        .expect_err("heterogeneous range scan must fail");
    assert_eq!(error.kind, QueryErrorKind::Type);

    execute(
        &connection,
        "CREATE RANGE INDEX mixed_value FOR (n:Mixed) ON (n.value)",
        ExecutionOptions::default(),
    )
    .expect("range index");
    let error = execute(&connection, query, ExecutionOptions::default())
        .expect_err("Range Index must not suppress heterogeneous comparison errors");
    assert_eq!(error.kind, QueryErrorKind::Type);
    let (plan, _) = execute(
        &connection,
        &format!("EXPLAIN {query}"),
        ExecutionOptions::default(),
    )
    .expect("explain");
    assert!(matches!(&plan[0][0], Value::String(plan) if !plan.contains("mixed_value")));
}

#[test]
fn historical_index_planning_uses_target_commit_type_proof() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Metric {value:1}), (:Metric {value:2}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed metrics");
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
        ExecutionOptions::default(),
    )
    .expect("range index without type proof");
    let untyped_commit = branch_head(&connection, "main").expect("untyped head");

    let query = "EXPLAIN MATCH (n:Metric) WHERE n.value > 1 RETURN n.value";
    let (untyped_plan, _) =
        execute(&connection, query, ExecutionOptions::default()).expect("untyped current plan");
    assert!(matches!(&untyped_plan[0][0], Value::String(plan) if !plan.contains("metric_value")));

    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
        ExecutionOptions::default(),
    )
    .expect("add type proof");
    let typed_commit = branch_head(&connection, "main").expect("typed head");
    let (typed_plan, _) =
        execute(&connection, query, ExecutionOptions::default()).expect("typed current plan");
    assert!(matches!(&typed_plan[0][0], Value::String(plan) if plan.contains("metric_value")));

    let mut historical_untyped = ExecutionOptions::default();
    historical_untyped.snapshot =
        lithograph_core::query::SnapshotSelector::Commit(untyped_commit.to_hex());
    let (historical_untyped_plan, _) =
        execute(&connection, query, historical_untyped).expect("historical untyped plan");
    assert!(matches!(
        &historical_untyped_plan[0][0],
        Value::String(plan) if !plan.contains("metric_value")
    ));

    execute(
        &connection,
        "DROP CONSTRAINT metric_value_type",
        ExecutionOptions::default(),
    )
    .expect("drop current type proof");
    let (dropped_plan, _) = execute(&connection, query, ExecutionOptions::default())
        .expect("current plan after type proof drop");
    assert!(matches!(&dropped_plan[0][0], Value::String(plan) if !plan.contains("metric_value")));

    let mut historical_typed = ExecutionOptions::default();
    historical_typed.snapshot =
        lithograph_core::query::SnapshotSelector::Commit(typed_commit.to_hex());
    let (historical_typed_plan, _) =
        execute(&connection, query, historical_typed).expect("historical typed plan");
    assert!(matches!(
        &historical_typed_plan[0][0],
        Value::String(plan) if plan.contains("metric_value")
    ));
}

#[test]
fn schema_interrupt_after_commit_step_rolls_back_branch_and_schema() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Metric {value:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed metric");
    let before = branch_head(&connection, "main").expect("head before schema interrupt");
    let prepared = prepare(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare schema DDL");
    let checks = std::cell::Cell::new(0_u32);
    let interrupted = || {
        let next = checks.get().saturating_add(1);
        checks.set(next);
        next >= 4
    };
    let mut cursor = QueryCursor::new(prepared);
    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &interrupted)
        .expect_err("interrupt after canonical schema commit must roll back the savepoint");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(
        branch_head(&connection, "main").expect("head after schema interrupt"),
        before
    );
    assert!(
        !SchemaState::load(&connection, before)
            .expect("schema after rollback")
            .indexes
            .contains_key("metric_value")
    );
}
