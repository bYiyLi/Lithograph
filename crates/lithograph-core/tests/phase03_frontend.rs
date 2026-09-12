use std::collections::BTreeMap;

use lithograph_core::cypher::{
    AstKind, CypherComparison, CypherType, DateValue, DurationValue, FrontendErrorKind,
    LocalDateTimeValue, LocalTimeValue, PointValue, TimeValue, UuidValue, Value,
    VectorCoordinateType, VectorValue, VectorValues, ZonedDateTimeValue, cypher_compare,
    cypher_equals, decode_json, decode_parameters, encode_json, parse, validate,
};
use serde_json::json;

#[test]
fn cypher25_frozen_grammar_families_parse() {
    let queries = [
        "CYPHER 25 RETURN 1 AS n",
        "CYPHER 25 PROFILE RETURN 1 AS n",
        "CYPHER runtime=slotted EXPLAIN RETURN 1 AS n",
        "PROFILE CYPHER 25 runtime=slotted RETURN 1 AS n",
        "EXPLAIN MATCH (n:Person)-[r:KNOWS]->(m) WHERE n.age >= 18 RETURN n, r, m",
        "RETURN ALL $0param AS n",
        "RETURN ALL (1) AS n",
        "WITH ALL 1 AS n RETURN n",
        "WITH ALL (1) AS n RETURN n",
        "MATCH REPEATABLE ELEMENTS p = (:Node)-->{1,2}() RETURN p",
        "MATCH DIFFERENT RELATIONSHIPS p = (:Node)-->{1,2}() RETURN p",
        "{ RETURN 1 AS n UNION RETURN 2 AS n } UNION ALL RETURN 3 AS n",
        "{ WHEN true THEN RETURN 1 AS n ELSE RETURN 2 AS n } UNION { WHEN false THEN RETURN 3 AS n ELSE RETURN 4 AS n }",
        "MATCH (n) FILTER n.active RETURN n NEXT MATCH (m) RETURN m",
        "WHEN true THEN RETURN 1 AS value ELSE RETURN 2 AS value",
        "UNWIND [1,2] AS x WITH x WHERE x > 1 RETURN x",
        "FOR x IN [1,2] RETURN x",
        "MATCH (n) SEARCH n IN (VECTOR INDEX embedding_idx FOR $query LIMIT 10) SCORE AS score RETURN n, score",
        "CALL (x) { WITH x RETURN x AS y } RETURN y",
        "CALL { RETURN 1 AS x } IN TRANSACTIONS OF 100 ROWS ON ERROR CONTINUE REPORT STATUS AS status RETURN status",
        "LOAD CSV WITH HEADERS FROM 'file:///rows.csv' AS row RETURN row.name",
        "CREATE VECTOR INDEX vec_idx FOR (n:Doc) ON (n.embedding)",
        "CREATE CONSTRAINT person_key FOR (n:Person) REQUIRE n.id IS UNIQUE",
        "SHOW VECTOR INDEXES YIELD name RETURN name",
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {id :: INTEGER IS UNIQUE}) }",
        "RETURN vector([1,2,3], 3, INTEGER) AS v",
        "RETURN uuid('550e8400-e29b-41d4-a716-446655440000') AS id",
        r#"WITH 'Ada' AS name RETURN s"Hello {name}" AS greeting"#,
        "MATCH p = SHORTEST 2 PATHS (a)-[:KNOWS*1..3]->(b) RETURN p",
    ];
    for query in queries {
        parse(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }
}

#[test]
fn cypher25_frozen_grammar_rejects_invalid_new_syntax_combinations() {
    for query in [
        "MATCH DIFFERENT ELEMENTS (n)-->(m) RETURN n",
        "MATCH REPEATABLE RELATIONSHIPS (n)-->(m) RETURN n",
        "WHEN true THEN",
        "WHEN true THEN RETURN 1 AS n ELSE",
        "RETURN ALL DISTINCT 1 AS n",
        "CYPHER 5 RETURN 1 AS n",
    ] {
        let error = parse(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Parse, "{query}");
    }
}

#[test]
fn invalid_syntax_reports_stable_line_and_column() {
    let error = parse("MATCH (n)\nRETURN").expect_err("missing projection must fail");
    assert_eq!(error.kind, FrontendErrorKind::Parse);
    assert_eq!((error.line, error.column), (2, 7));
    assert!(error.span.start <= error.span.end);

    let error = parse("MATCH (n) RETURN n,").expect_err("trailing comma must fail");
    assert_eq!(error.kind, FrontendErrorKind::Parse);
    assert_eq!((error.line, error.column), (1, 20));
}

#[test]
fn scope_isolation_and_correlation_are_enforced() {
    for query in [
        "WITH [1,2] AS list RETURN any(x IN list WHERE x > 1) AS ok",
        "WITH [1,2] AS list RETURN [x IN list WHERE x > 1 | x] AS xs",
        "MATCH (n) CALL (n) { RETURN n AS x } RETURN x",
        "CALL { { RETURN 1 AS x } UNION ALL { RETURN 2 AS x } } RETURN x",
        "CALL { WHEN true THEN RETURN 1 AS x ELSE RETURN 2 AS x } RETURN x",
        "MATCH (n) RETURN EXISTS { MATCH (m) WHERE m.id = n.id } AS ok",
        "MATCH (n) WITH n AS x RETURN x",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }

    for query in [
        "MATCH (n) CALL () { RETURN n } RETURN n",
        "MATCH (n) WITH n AS x RETURN n",
        "MATCH (n) RETURN m",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Semantic, "{query}");
    }
}

#[test]
fn graph_element_categories_and_search_bindings_are_checked() {
    validate("MATCH (n) SEARCH n IN (VECTOR INDEX idx FOR $q LIMIT 3) RETURN n")
        .expect("node SEARCH binding");

    let error = validate("WITH 1 AS n MATCH (n)-[r]->() RETURN r").expect_err("value/node reuse");
    assert_eq!(error.kind, FrontendErrorKind::Semantic);

    let error = validate("WITH 1 AS n SEARCH n IN (VECTOR INDEX idx FOR $q LIMIT 3) RETURN n")
        .expect_err("SEARCH requires graph element");
    assert_eq!(error.kind, FrontendErrorKind::Parse);
}

#[test]
fn type_and_function_validation_catches_invalid_forms() {
    for query in [
        "RETURN 1 IS TYPED SIGNED INTEGER",
        "RETURN 1 IS TYPED LOCAL TIME",
        "RETURN 1 IS TYPED ZONED TIME",
        "RETURN 1 IS TYPED LOCAL DATETIME",
        "RETURN 1 IS TYPED ZONED DATETIME",
        "RETURN 1 IS TYPED TIMESTAMP WITHOUT TIME ZONE",
        "RETURN 1 IS TYPED TIMESTAMP WITH TIMEZONE",
        "RETURN 1 IS TYPED TIME WITHOUT TIMEZONE",
        "RETURN 1 IS TYPED TIME WITH TIME ZONE",
        "RETURN 1 IS TYPED INTEGER!",
        "RETURN 1 IS TYPED INTEGER LIST",
        "RETURN 1 IS TYPED ARRAY<INTEGER>",
        "RETURN 1 IS TYPED VECTOR<INTEGER8>(3)",
        "RETURN 1 IS TYPED VECTOR(3, FLOAT32)",
        "RETURN 1 IS TYPED ANY NODE",
        "RETURN 1 IS TYPED PROPERTY VALUE",
        "RETURN 1 IS TYPED NOTHING",
        "RETURN 1 :: INTEGER",
        "RETURN 1 IS TYPED INTEGER | STRING",
        "RETURN 1 IS TYPED ANY<INTEGER | STRING>",
        "RETURN 1 IS TYPED LIST<INTEGER NOT NULL>",
        "RETURN 1 IS TYPED INTEGER NOT NULL LIST NOT NULL",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }

    validate("RETURN 1 + 2.5 AS n").expect("numeric coercion");
    validate("RETURN 'a' + 'b' AS s").expect("string concatenation");
    for query in [
        "RETURN -9223372036854775808 AS min",
        "RETURN -0x8000000000000000 AS min_hex",
        "RETURN -0o1000000000000000000000 AS min_octal",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }
    for query in [
        "RETURN 9223372036854775808",
        "RETURN --9223372036854775808",
        "RETURN +9223372036854775808",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Type, "{query}");
    }
    for query in [
        "RETURN uuid() AS id",
        "RETURN uuid('550e8400-e29b-41d4-a716-446655440000') AS id",
        "RETURN uuid(1, 2) AS id",
        "RETURN date() AS d",
        "RETURN date('2026-09-10') AS d",
        "RETURN date('10/09/2026', 'dd/MM/yyyy') AS d",
        "RETURN zoned_time() AS t",
        "RETURN local_datetime() AS dt",
        "RETURN duration('P1D') AS d",
        "RETURN point({x: 1, y: 2}) AS p",
        "RETURN toInteger(true) AS n",
        "RETURN toFloat('1.5') AS n",
        "RETURN toString(date('2026-09-10')) AS s",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }
    validate("RETURN vector([1,2], 2, INTEGER) AS v").expect("vector signature");
    validate("RETURN size(vector([1,2], 2, INTEGER8)) AS size")
        .expect("size() accepts Vector input");
    let error = validate("RETURN size({a: 1}) AS size").expect_err("size() must reject Map input");
    assert_eq!(error.kind, FrontendErrorKind::Type);

    let error = validate("RETURN 'x' + 1").expect_err("incompatible +");
    assert_eq!(error.kind, FrontendErrorKind::Type);

    for query in [
        "RETURN (CASE WHEN true THEN 'x' END) - 1",
        "RETURN (CASE 1 WHEN 1 THEN 'x' END) - 1",
        "RETURN 1 + 'x' = 2",
        "RETURN 1 - 'x' = 2",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Type, "{query}");
    }

    let error = validate("RETURN (1 :: INTEGER) + 1").expect_err("type predicate is Boolean");
    assert_eq!(error.kind, FrontendErrorKind::Type);

    for query in [
        "RETURN uuid(1)",
        "RETURN uuid('a', 'b')",
        "RETURN uuid(1, 2, 3)",
        "RETURN duration()",
        "RETURN duration('P1D', 1)",
        "RETURN point()",
        "RETURN point(1)",
        "RETURN point({}, {})",
        "RETURN date(1, 'yyyy')",
        "RETURN date('2026', 1)",
        "RETURN date(1, 2, 3)",
        "RETURN toInteger([1])",
        "RETURN toFloat(true)",
        "RETURN toString({x: 1})",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Type, "{query}");
    }
    validate("RETURN duration('5 hours', \"h 'hours'\")")
        .expect("duration input and pattern are part of the frozen profile");
}

#[test]
fn vector_and_schema_validation_catches_invalid_forms() {
    let error = validate("RETURN vector([1,2], 2)").expect_err("vector arity");
    assert_eq!(error.kind, FrontendErrorKind::Type);

    for query in [
        "RETURN vector(1, 1, INTEGER)",
        "RETURN vector([1,2], '2', INTEGER)",
        "RETURN vector([1,2], 0, INTEGER)",
        "RETURN vector([1,2], 4097, INTEGER)",
        "RETURN vector([1,2], 3, INTEGER)",
        "RETURN vector(['x'], 1, FLOAT32)",
        "RETURN vector([1,2], 2, I8)",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Type, "{query}");
    }

    for query in [
        "RETURN vector([1,2], 2, INT8)",
        "RETURN vector([1,2], 2, INTEGER64)",
        "RETURN vector([1.0,2], 2, FLOAT32)",
        "RETURN vector('1,2', 2, FLOAT64)",
        "RETURN vector($values, $dimension, INTEGER16)",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }

    let error = validate("ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: NODE}) }")
        .expect_err("structural property type");
    assert_eq!(error.kind, FrontendErrorKind::Schema);

    let error = validate("ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: MADE_UP}) }")
        .expect_err("unknown type");
    assert_eq!(error.kind, FrontendErrorKind::Type);

    for query in [
        "RETURN 1 IS TYPED TIME",
        "RETURN 1 IS TYPED DATETIME",
        "RETURN 1 IS TYPED VECTOR<BAD>(3)",
        "RETURN 1 IS TYPED VECTOR<INTEGER8>(0)",
        "RETURN 1 IS TYPED VECTOR(4097, FLOAT32)",
        "RETURN 1 IS TYPED LIST",
        "RETURN 1 IS TYPED STRING<INTEGER>",
        "RETURN 1 IS TYPED LIST<INTEGER, STRING>",
        "RETURN 1 IS TYPED INTEGER NOT NULL | STRING",
        "RETURN 1 IS TYPED ANY<INTEGER | STRING> NOT NULL",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Type, "{query}");
    }

    for query in [
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {a :: ANY NOT NULL, b :: VECTOR<FLOAT32>(3), c :: LIST<INTEGER NOT NULL>}) }",
        "CREATE CONSTRAINT c FOR (n:Person) REQUIRE n.x IS :: STRING | LIST<INTEGER NOT NULL>",
        "CREATE CONSTRAINT c FOR (n:Person) REQUIRE n.x IS :: VECTOR<FLOAT32>(3)",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }

    for query in [
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: ANY}) }",
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: VECTOR}) }",
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: VECTOR<FLOAT32>}) }",
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: LIST<INTEGER>}) }",
        "ALTER CURRENT GRAPH TYPE ADD { (p IS Person => {x :: LIST<VECTOR<FLOAT32>(3) NOT NULL>}) }",
        "CREATE CONSTRAINT c FOR (n:Person) REQUIRE n.x IS :: ANY NOT NULL",
        "CREATE CONSTRAINT c FOR (n:Person) REQUIRE n.x IS :: VECTOR(3)",
        "CREATE CONSTRAINT c FOR (n:Person) REQUIRE n.x IS :: LIST<INTEGER>",
    ] {
        let error = validate(query).expect_err(query);
        assert_eq!(error.kind, FrontendErrorKind::Schema, "{query}");
    }
}

#[test]
fn structured_semantics_handle_escaped_identifiers_comments_and_arrows() {
    for query in [
        "MATCH (`n`) RETURN `n`",
        "MATCH (`a,b`) RETURN `a,b`",
        "MATCH (`a,b`) CALL (`a,b`) { RETURN `a,b` AS x } RETURN x",
        "MATCH (`n`) SEARCH `n` IN (VECTOR INDEX idx FOR $q LIMIT 3) RETURN `n`",
        "LOAD CSV FROM 'file:///x.csv' AS /* comment */ row RETURN row",
        "MATCH (n) CALL (n /* comment */) { RETURN n AS x } RETURN x",
        "CREATE ()-[:R {x:'>'}]->()",
    ] {
        validate(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    }

    let error = validate("CREATE ()-[:R {x:'>'}]-()").expect_err("CREATE must be directed");
    assert_eq!(error.kind, FrontendErrorKind::Semantic);
}

#[test]
fn interpolation_errors_are_mapped_to_the_original_query_span() {
    let error =
        validate("WITH 1 AS x RETURN s\"ok {missing}\" AS y").expect_err("undefined variable");
    assert_eq!(error.kind, FrontendErrorKind::Semantic);
    assert_eq!((error.line, error.column), (1, 26));

    let error =
        validate("WITH 1 AS x\nRETURN s\"ok { missing }\" AS y").expect_err("undefined variable");
    assert_eq!((error.line, error.column), (2, 15));
}

#[test]
fn runtime_value_model_separates_parameter_and_property_values() {
    let scalar = Value::Integer(7);
    assert!(scalar.is_parameter_value());
    assert!(scalar.is_property_value());

    let mut map = BTreeMap::new();
    map.insert("x".to_owned(), Value::Integer(1));
    let map = Value::Map(map);
    assert!(map.is_parameter_value());
    assert!(!map.is_property_value());

    let homogeneous = Value::List(vec![Value::Integer(1), Value::Integer(2)]);
    assert!(homogeneous.is_property_value());
    let mixed = Value::List(vec![Value::Integer(1), Value::Float(2.0)]);
    assert!(!mixed.is_property_value());
    let nested = Value::List(vec![Value::List(vec![Value::Integer(1)])]);
    assert!(!nested.is_property_value());
    let vector_list = Value::List(vec![Value::Vector(
        VectorValue::new(VectorCoordinateType::I8, VectorValues::I8(vec![1, 2]))
            .expect("valid vector"),
    )]);
    assert!(!vector_list.is_property_value());
}

#[test]
fn null_numeric_and_collection_comparison_semantics_are_stable() {
    assert_eq!(cypher_equals(&Value::Null, &Value::Integer(1)), Ok(None));
    assert_eq!(
        cypher_equals(&Value::Integer(1), &Value::Float(1.0)),
        Ok(Some(true))
    );
    assert_eq!(
        cypher_equals(&Value::Float(f64::NAN), &Value::Float(f64::NAN)),
        Ok(Some(false))
    );
    assert_eq!(
        cypher_compare(&Value::Integer(2), &Value::Float(1.5)),
        Ok(Some(CypherComparison::Greater))
    );

    // The integer cannot be rounded through f64 for equality.
    let exact = Value::Integer(9_007_199_254_740_993);
    let rounded = Value::Float(9_007_199_254_740_992.0);
    assert_eq!(cypher_equals(&exact, &rounded), Ok(Some(false)));
    assert_eq!(
        cypher_compare(&exact, &rounded),
        Ok(Some(CypherComparison::Greater))
    );

    assert_eq!(
        cypher_equals(
            &Value::List(vec![Value::Integer(1), Value::Null]),
            &Value::List(vec![Value::Integer(1), Value::Null])
        ),
        Ok(None)
    );
}

#[test]
fn temporal_and_path_comparison_semantics_use_value_identity() {
    let earlier = Value::Time(TimeValue::parse("12:00:00+02:00").expect("time"));
    let later = Value::Time(TimeValue::parse("11:00:00Z").expect("time"));
    assert_eq!(
        cypher_compare(&earlier, &later),
        Ok(Some(CypherComparison::Less))
    );

    let month = Value::Duration(DurationValue::parse("P1M").expect("duration"));
    let days = Value::Duration(DurationValue::parse("P30D").expect("duration"));
    assert_eq!(cypher_compare(&month, &days), Ok(None));

    let left_node = lithograph_core::cypher::NodeValue {
        element_id: "n:1".into(),
        labels: vec!["A".into()],
        properties: BTreeMap::new(),
    };
    let right_node = lithograph_core::cypher::NodeValue {
        element_id: "n:2".into(),
        labels: vec![],
        properties: BTreeMap::new(),
    };
    let relationship = lithograph_core::cypher::RelationshipValue {
        element_id: "r:1".into(),
        relationship_type: "R".into(),
        start: "n:1".into(),
        end: "n:2".into(),
        properties: BTreeMap::new(),
    };
    let same_left_identity = lithograph_core::cypher::NodeValue {
        element_id: "n:1".into(),
        labels: vec!["Changed".into()],
        properties: BTreeMap::from([("x".into(), Value::Integer(1))]),
    };
    let same_relationship_identity = lithograph_core::cypher::RelationshipValue {
        properties: BTreeMap::from([("x".into(), Value::Integer(2))]),
        ..relationship.clone()
    };
    let left = Value::Path(lithograph_core::cypher::PathValue {
        nodes: vec![left_node.clone(), right_node.clone()],
        relationships: vec![relationship.clone()],
    });
    let right = Value::Path(lithograph_core::cypher::PathValue {
        nodes: vec![same_left_identity, right_node.clone()],
        relationships: vec![same_relationship_identity],
    });
    assert_eq!(cypher_equals(&left, &right), Ok(Some(true)));
    assert_eq!(
        cypher_equals(
            &left,
            &Value::List(vec![
                Value::Node(left_node),
                Value::Relationship(relationship),
                Value::Node(right_node),
            ]),
        ),
        Ok(Some(true))
    );
}

#[test]
fn duration_values_use_canonical_component_text() {
    for (input, expected) in [
        ("P12M", "P1Y"),
        ("P2W", "P14D"),
        ("PT70S", "PT1M10S"),
        ("PT0.750000000S", "PT0.75S"),
        ("PT1H-30M", "PT30M"),
        ("P0D", "PT0S"),
    ] {
        let value = DurationValue::parse(input).unwrap_or_else(|error| panic!("{input}: {error}"));
        assert_eq!(value.as_str(), expected, "{input}");
        let encoded = encode_json(&Value::Duration(value));
        assert_eq!(encoded["value"], expected, "{input}");
    }
}

#[test]
fn lithograph_json_round_trips_exact_value_families() {
    let uuid = UuidValue::parse("550E8400-E29B-41D4-A716-446655440000").expect("uuid");
    let values = [
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        Value::Date(DateValue::parse("2026-09-10").expect("date")),
        Value::LocalTime(LocalTimeValue::parse("12:34:56.123456789").expect("local time")),
        Value::Time(TimeValue::parse("12:34:56+08:00").expect("time")),
        Value::LocalDateTime(
            LocalDateTimeValue::parse("2026-09-10T12:34:56").expect("local datetime"),
        ),
        Value::ZonedDateTime(
            ZonedDateTimeValue::parse("2026-09-10T12:34:56+08:00", "Asia/Shanghai")
                .expect("zoned datetime"),
        ),
        Value::Duration(DurationValue::parse("P1M2DT3S").expect("duration")),
        Value::Uuid(uuid),
        Value::Vector(
            VectorValue::new(
                VectorCoordinateType::I64,
                VectorValues::I64(vec![i64::MIN, 0, i64::MAX]),
            )
            .expect("I64 vector"),
        ),
        Value::Vector(
            VectorValue::new(
                VectorCoordinateType::F64,
                VectorValues::F64(vec![-1.5, -0.0, 2.25]),
            )
            .expect("F64 vector"),
        ),
    ];
    for value in values {
        let encoded = encode_json(&value);
        let decoded = decode_json(&encoded).expect("round trip");
        assert_value_equivalent(&decoded, &value);
    }
}

#[test]
fn lithograph_json_preserves_reserved_map_keys_and_rejects_structural_params() {
    let mut values = BTreeMap::new();
    values.insert("$type".to_owned(), Value::String("user-data".to_owned()));
    values.insert("n".to_owned(), Value::Integer(i64::MAX));
    let value = Value::Map(values);
    let encoded = encode_json(&value);
    assert_eq!(encoded["$type"], "Map");
    assert_value_equivalent(&decode_json(&encoded).expect("wrapped map"), &value);

    let params = json!({
        "id": {"$type":"Integer","value":"9223372036854775807"},
        "uuid": {"$type":"UUID","value":"550e8400-e29b-41d4-a716-446655440000"}
    });
    let decoded = decode_parameters(&params).expect("parameters");
    assert_eq!(decoded["id"], Value::Integer(i64::MAX));

    let structural = json!({
        "node": {"$type":"Node","elementId":"n:1","labels":[],"properties":{}}
    });
    assert!(decode_parameters(&structural).is_err());
}

#[test]
fn ast_is_lithograph_owned_and_frontend_has_no_sqlite_dependency_surface() {
    let ast = parse("MATCH (n) RETURN n").expect("parse");
    assert!(
        ast.root
            .descendants()
            .any(|node| node.kind == AstKind::NodePattern)
    );
    assert!(
        !format!("{ast:#?}")
            .to_ascii_lowercase()
            .contains("graphqlite"),
        "owned AST must not expose reference implementation nodes"
    );
    // Compile-time surface proof: validation requires only query text, not a SQLite handle.
    let validator: fn(&str) -> Result<_, _> = validate;
    validator("RETURN 1").expect("standalone frontend validation");

    let value = Value::Vector(
        VectorValue::new(VectorCoordinateType::I16, VectorValues::I16(vec![1, 2, 3]))
            .expect("valid vector"),
    );
    assert_eq!(
        CypherType::of_value(&value),
        CypherType::Vector(Some(VectorCoordinateType::I16), Some(3))
    );
}

fn assert_value_equivalent(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Float(left), Value::Float(right)) if left.is_nan() && right.is_nan() => {}
        (Value::Vector(left), Value::Vector(right)) => match (left.values(), right.values()) {
            (VectorValues::F64(left), VectorValues::F64(right)) => {
                assert_eq!(left.len(), right.len());
                for (left, right) in left.iter().zip(right) {
                    if right.is_nan() {
                        assert!(left.is_nan());
                    } else {
                        assert_eq!(left.to_bits(), right.to_bits());
                    }
                }
            }
            _ => assert_eq!(actual, expected),
        },
        _ => assert_eq!(actual, expected),
    }
}

#[test]
fn persistent_property_value_validation_rejects_non_property_list_shapes() {
    use lithograph_core::storage::{
        PropertyValue as PersistentValue, VectorCoordinateType as PersistentCoordinateType,
        VectorValue as PersistentVector,
    };

    assert!(
        !PersistentValue::List(vec![
            PersistentValue::Integer(1),
            PersistentValue::Float(2.0),
        ])
        .is_property_value()
    );
    assert!(
        !PersistentValue::List(vec![PersistentValue::List(vec![PersistentValue::Integer(
            1
        ),])])
        .is_property_value()
    );
    assert!(
        !PersistentValue::List(vec![PersistentValue::Vector(PersistentVector {
            coordinate_type: PersistentCoordinateType::I8,
            dimension: 1,
            packed: vec![1],
        },)])
        .is_property_value()
    );
    assert!(
        PersistentValue::List(vec![
            PersistentValue::Integer(1),
            PersistentValue::Integer(2),
        ])
        .is_property_value()
    );
}

#[test]
fn lithograph_json_round_trips_structural_spatial_and_text_surfaces() {
    use lithograph_core::cypher::{
        NodeValue, PathValue, PointValue, RelationshipValue, decode_json_text,
        decode_parameters_text, encode_json_text,
    };

    let mut node_properties = BTreeMap::new();
    node_properties.insert("name".to_owned(), Value::String("Ada".to_owned()));
    node_properties.insert("age".to_owned(), Value::Integer(i64::MAX));
    let left = NodeValue {
        element_id: "n:1".to_owned(),
        labels: vec!["Person".to_owned(), "Engineer".to_owned()],
        properties: node_properties,
    };
    let right = NodeValue {
        element_id: "n:2".to_owned(),
        labels: vec!["Company".to_owned()],
        properties: BTreeMap::new(),
    };
    let relationship = RelationshipValue {
        element_id: "r:7".to_owned(),
        relationship_type: "WORKS_AT".to_owned(),
        start: "n:1".to_owned(),
        end: "n:2".to_owned(),
        properties: BTreeMap::from([("since".to_owned(), Value::Integer(2026))]),
    };
    let values = [
        Value::Node(left.clone()),
        Value::Relationship(relationship.clone()),
        Value::Path(PathValue {
            nodes: vec![left, right],
            relationships: vec![relationship],
        }),
        Value::Point(PointValue::new("wgs-84", vec![120.5, 30.25]).expect("point")),
        Value::Map(BTreeMap::from([
            ("enabled".to_owned(), Value::Boolean(true)),
            (
                "items".to_owned(),
                Value::List(vec![Value::Integer(1), Value::Null]),
            ),
        ])),
    ];

    for value in values {
        let text = encode_json_text(&value);
        let decoded = decode_json_text(&text).expect("text round trip");
        assert_value_equivalent(&decoded, &value);
    }

    let params = decode_parameters_text(
        r#"{"limit":3,"query":"Ada","point":{"$type":"Point","crs":"cartesian","coordinates":[1.0,2.0]}}"#,
    )
    .expect("parameter text");
    assert_eq!(params["limit"], Value::Integer(3));
    assert!(matches!(params["point"], Value::Point(_)));
}

#[test]
fn lithograph_json_covers_every_vector_coordinate_type() {
    let vectors = [
        VectorValue::new(
            VectorCoordinateType::I8,
            VectorValues::I8(vec![i8::MIN, 0, i8::MAX]),
        )
        .expect("I8 vector"),
        VectorValue::new(
            VectorCoordinateType::I16,
            VectorValues::I16(vec![i16::MIN, 0, i16::MAX]),
        )
        .expect("I16 vector"),
        VectorValue::new(
            VectorCoordinateType::I32,
            VectorValues::I32(vec![i32::MIN, 0, i32::MAX]),
        )
        .expect("I32 vector"),
        VectorValue::new(
            VectorCoordinateType::I64,
            VectorValues::I64(vec![i64::MIN, 0, i64::MAX]),
        )
        .expect("I64 vector"),
        VectorValue::new(
            VectorCoordinateType::F32,
            VectorValues::F32(vec![-1.5, -0.0, 2.25]),
        )
        .expect("F32 vector"),
        VectorValue::new(
            VectorCoordinateType::F64,
            VectorValues::F64(vec![-1.5, -0.0, 2.25]),
        )
        .expect("F64 vector"),
    ];

    for vector in vectors {
        let value = Value::Vector(vector.clone());
        let decoded = decode_json(&encode_json(&value)).expect("vector round trip");
        assert_value_equivalent(&decoded, &value);
        assert!(!vector.values().is_empty());
        assert_eq!(vector.dimension(), 3);
        assert_eq!(
            VectorCoordinateType::parse(vector.coordinate_type().as_str()),
            Some(vector.coordinate_type())
        );
    }

    for (alias, expected) in [
        ("INTEGER8", VectorCoordinateType::I8),
        ("INTEGER16", VectorCoordinateType::I16),
        ("INTEGER32", VectorCoordinateType::I32),
        ("INTEGER64", VectorCoordinateType::I64),
        ("FLOAT32", VectorCoordinateType::F32),
        ("FLOAT64", VectorCoordinateType::F64),
    ] {
        assert_eq!(VectorCoordinateType::parse(alias), Some(expected));
    }
    assert_eq!(VectorCoordinateType::parse("made-up"), None);
    assert!(VectorValues::I8(Vec::new()).is_empty());
}

#[test]
fn malformed_lithograph_json_is_rejected_at_the_value_boundary() {
    use lithograph_core::cypher::{decode_json_text, decode_parameters_text};

    assert!(decode_json_text("{").is_err());
    assert!(decode_parameters_text("[]").is_err());
    assert!(decode_parameters_text("{").is_err());

    let invalid = [
        json!({"$type": 3}),
        json!({"$type":"Unknown"}),
        json!({"$type":"Integer","value":"9223372036854775808"}),
        json!({"$type":"Float","value":"finite"}),
        json!({"$type":"Map"}),
        json!({"$type":"Node","elementId":"n:1","labels":[1],"properties":{}}),
        json!({"$type":"Node","elementId":"n:1","labels":[],"properties":[]}),
        json!({"$type":"Relationship","elementId":"r:1","type":"R","start":"n:1"}),
        json!({"$type":"Path","nodes":[1],"relationships":[]}),
        json!({"$type":"Path","nodes":[],"relationships":[{"$type":"Relationship","elementId":"r:1","type":"R","start":"n:1","end":"n:2","properties":{}}]}),
        json!({"$type":"Point","crs":"cartesian","coordinates":[1]}),
        json!({"$type":"Point","crs":"cartesian","coordinates":[1,"x"]}),
        json!({"$type":"Vector","coordinateType":"BAD","dimension":1,"values":[1]}),
        json!({"$type":"Vector","coordinateType":"I8","dimension":2,"values":[1]}),
        json!({"$type":"Vector","coordinateType":"I8","dimension":1,"values":[128]}),
        json!({"$type":"Vector","coordinateType":"I16","dimension":1,"values":["x"]}),
        json!({"$type":"Vector","coordinateType":"F32","dimension":1,"values":["x"]}),
        json!({"$type":"Vector","coordinateType":"F64","dimension":-1,"values":[]}),
        json!({"$type":"Vector","coordinateType":"F32","dimension":0,"values":[]}),
        json!({"$type":"Vector","coordinateType":"F32","dimension":1,"values":[1e308]}),
        json!({"$type":"Vector","coordinateType":"F64","dimension":1,"values":[{"$type":"Float","value":"Infinity"}]}),
        json!({"$type":"Date","value":"not-a-date"}),
        json!({"$type":"Date","value":"2025-02-29"}),
        json!({"$type":"Time","value":"25:99:99+99:99"}),
        json!({"$type":"LocalDateTime","value":"2026-02-30T12:00:00"}),
        json!({"$type":"ZonedDateTime","value":"2026-09-10T12:00:00+08:00","zone":"bad zone!"}),
        json!({"$type":"ZonedDateTime","value":"2026-09-10T12:00:00+08:00","zone":"+07:00"}),
        json!({"$type":"Duration","value":"P-nope"}),
        json!({"$type":"Duration","value":"P1D1Y"}),
        json!({"$type":"Duration","value":"P1Y1Y"}),
        json!({"$type":"Duration","value":"P1DT"}),
        json!({"$type":"Duration","value":"PT1S1H"}),
        json!({"$type":"Point","crs":"bogus","coordinates":[1,2]}),
        json!({"$type":"Point","crs":"wgs-84","coordinates":[1,2,3]}),
        json!({"$type":"Point","crs":"wgs-84","coordinates":[1,91]}),
        json!({"$type":"UUID","value":"not-a-uuid"}),
        json!({"$type":"UUID","value":"550e8400-e29b-41d4-a716-44665544000z"}),
        json!({"$type":"Integer","value":"1","extra":true}),
        json!({"$type":"ZonedDateTime","value":"2026-09-10T12:00:00+08:00","zone":"Asia/Shanghai","extra":true}),
        json!({"$type":"Vector","coordinateType":"I8","dimension":1,"values":[1],"extra":true}),
    ];
    for value in invalid {
        assert!(decode_json(&value).is_err(), "must reject {value}");
    }
}

#[test]
fn runtime_value_equality_ordering_and_uuid_helpers_cover_edge_families() {
    let uuid = UuidValue::parse("550E8400-E29B-41D4-A716-446655440000").expect("uuid");
    assert_eq!(uuid.to_canonical(), "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(uuid.as_bytes().len(), 16);
    assert!(UuidValue::parse("550e8400e29b41d4a716446655440000").is_err());
    assert!(UuidValue::parse("550e8400-e29b-41d4-a716-44665544000z").is_err());

    let fixed_zone = ZonedDateTimeValue::parse("2026-09-10T12:00:00+08:00", "+08:00")
        .expect("fixed offset zone");
    assert_eq!(fixed_zone.zone(), "+08:00");
    assert!(ZonedDateTimeValue::parse("2026-09-10T12:00:00+08:00", "+07:00").is_err());
    let named_zone = ZonedDateTimeValue::parse(
        "2026-09-10T12:00:00-03:00",
        "America/Argentina/Buenos_Aires",
    )
    .expect("IANA timezone offset is validated and preserved");
    assert_eq!(named_zone.zone(), "America/Argentina/Buenos_Aires");

    let left_map = BTreeMap::from([("x".to_owned(), Value::Integer(1))]);
    let right_map = BTreeMap::from([("x".to_owned(), Value::Integer(1))]);
    assert_eq!(
        cypher_equals(&Value::Map(left_map), &Value::Map(right_map)),
        Ok(Some(true))
    );
    assert_eq!(
        cypher_equals(
            &Value::List(vec![Value::Integer(1)]),
            &Value::List(vec![Value::Integer(1), Value::Integer(2)])
        ),
        Ok(Some(false))
    );
    assert_eq!(
        cypher_compare(&Value::String("a".into()), &Value::String("b".into())),
        Ok(Some(CypherComparison::Less))
    );
    assert_eq!(
        cypher_compare(&Value::Boolean(false), &Value::Boolean(true)),
        Ok(Some(CypherComparison::Less))
    );
    assert_eq!(
        cypher_compare(&Value::Float(f64::NAN), &Value::Float(1.0)),
        Ok(Some(CypherComparison::Unordered))
    );
    assert_eq!(
        cypher_compare(&Value::Integer(1), &Value::Float(f64::INFINITY)),
        Ok(Some(CypherComparison::Less))
    );
    assert_eq!(
        cypher_compare(&Value::Integer(1), &Value::Float(f64::NEG_INFINITY)),
        Ok(Some(CypherComparison::Greater))
    );

    assert_eq!(
        cypher_equals(&Value::Integer(1), &Value::String("1".into())),
        Ok(Some(false)),
        "Cypher equality across distinct value types is false"
    );
    assert_eq!(
        cypher_equals(
            &Value::List(vec![Value::Integer(1), Value::Integer(2)]),
            &Value::String("foo".into())
        ),
        Ok(Some(false)),
        "TCK List3 requires list/literal equality to be false"
    );
    assert!(
        cypher_compare(
            &Value::Point(PointValue::new("cartesian", vec![1.0, 2.0]).expect("point")),
            &Value::Point(PointValue::new("cartesian", vec![1.0, 3.0]).expect("point")),
        )
        .is_err(),
        "POINT cannot use direct ordering comparison"
    );
}

#[test]
fn runtime_structural_and_temporal_equality_uses_identity_and_value() {
    use lithograph_core::cypher::{NodeValue, RelationshipValue};

    let node = NodeValue {
        element_id: "n:1".into(),
        labels: vec![],
        properties: BTreeMap::new(),
    };
    let same_node = NodeValue {
        element_id: "n:1".into(),
        ..node.clone()
    };
    assert_eq!(
        cypher_equals(&Value::Node(node), &Value::Node(same_node)),
        Ok(Some(true))
    );
    let relationship = RelationshipValue {
        element_id: "r:1".into(),
        relationship_type: "R".into(),
        start: "n:1".into(),
        end: "n:2".into(),
        properties: BTreeMap::new(),
    };
    assert_eq!(
        cypher_equals(
            &Value::Relationship(relationship.clone()),
            &Value::Relationship(relationship)
        ),
        Ok(Some(true))
    );
    assert_eq!(
        cypher_equals(
            &Value::Point(PointValue::new("cartesian", vec![1.0, 2.0]).expect("point")),
            &Value::Point(PointValue::new("cartesian", vec![1.0, 2.0]).expect("point"))
        ),
        Ok(Some(true))
    );
    assert_eq!(
        cypher_equals(
            &Value::ZonedDateTime(
                ZonedDateTimeValue::parse("2026-09-10T12:00:00+08:00", "Asia/Shanghai")
                    .expect("zoned datetime"),
            ),
            &Value::ZonedDateTime(
                ZonedDateTimeValue::parse("2026-09-10T12:00:00+08:00", "Asia/Shanghai")
                    .expect("zoned datetime"),
            )
        ),
        Ok(Some(true))
    );
}
