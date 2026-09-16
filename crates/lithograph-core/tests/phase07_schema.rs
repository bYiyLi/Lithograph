use std::collections::BTreeMap;

use lithograph_core::cypher::{PointValue, Value};
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryErrorKind, QuerySummary, QueryType, prepare,
};
use lithograph_core::storage::{
    PropertyType, SchemaState, branch_head, create_branch, create_storage_schema, initialize_root,
};
use rusqlite::Connection;

#[path = "phase07_schema/regressions.rs"]
mod regressions;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1),magic TEXT NOT NULL,database_id TEXT NOT NULL,storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    execute_params(connection, query, BTreeMap::new(), options)
}

fn execute_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    let prepared = prepare(connection, query, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16)?;
        rows.extend(batch.rows);
        if batch.done {
            let summary = cursor.complete(connection)?;
            return Ok((rows, summary));
        }
    }
}

#[test]
fn graph_type_change_commits_and_time_travels() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before");

    let (_, summary) = execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}) }",
        ExecutionOptions::default(),
    )
    .expect("set graph type");

    let after = branch_head(&connection, "main").expect("head after");
    assert_ne!(before, after);
    assert_eq!(summary.query_type, QueryType::Schema);
    assert!(
        SchemaState::load(&connection, before)
            .expect("old schema")
            .graph_nodes
            .is_empty()
    );
    assert!(
        SchemaState::load(&connection, after)
            .expect("new schema")
            .graph_nodes
            .contains_key("Person")
    );
}

#[test]
fn graph_type_property_types_lower_into_versioned_schema_state() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Typed => {anything :: ANY NOT NULL, embedding :: VECTOR<FLOAT32>(3), numbers :: LIST<INTEGER NOT NULL>, unioned :: STRING | LIST<INTEGER NOT NULL>, flag :: BOOLEAN, count :: INTEGER, ratio :: FLOAT, name :: STRING, day :: DATE, localClock :: LOCAL TIME, zonedClock :: ZONED TIME, localStamp :: LOCAL DATETIME, zonedStamp :: ZONED DATETIME, span :: DURATION, location :: POINT, identifier :: UUID}) }",
        ExecutionOptions::default(),
    )
    .expect("Graph Type persistent property families");

    let schema = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("typed schema head"),
    )
    .expect("typed schema");
    let properties = &schema
        .graph_nodes
        .get("Typed")
        .expect("Typed Graph Node Type")
        .properties;

    assert!(properties["anything"].required);
    assert!(matches!(
        properties["anything"].property_type,
        PropertyType::Any
    ));
    assert!(matches!(
        &properties["embedding"].property_type,
        PropertyType::Vector {
            coordinate,
            dimension: 3
        } if coordinate == "FLOAT32"
    ));
    assert!(matches!(
        &properties["numbers"].property_type,
        PropertyType::List { element } if matches!(element.as_ref(), PropertyType::Integer)
    ));
    assert!(matches!(
        &properties["unioned"].property_type,
        PropertyType::Union { members }
            if matches!(members.as_slice(), [PropertyType::String, PropertyType::List { .. }])
    ));
    for (property, expected) in [
        ("flag", PropertyType::Boolean),
        ("count", PropertyType::Integer),
        ("ratio", PropertyType::Float),
        ("name", PropertyType::String),
        ("day", PropertyType::Date),
        ("localClock", PropertyType::LocalTime),
        ("zonedClock", PropertyType::ZonedTime),
        ("localStamp", PropertyType::LocalDateTime),
        ("zonedStamp", PropertyType::ZonedDateTime),
        ("span", PropertyType::Duration),
        ("location", PropertyType::Point),
        ("identifier", PropertyType::Uuid),
    ] {
        assert_eq!(properties[property].property_type, expected, "{property}");
    }
}

#[test]
fn graph_type_add_alter_and_drop_preserve_frozen_element_semantics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => :Resident {name :: STRING IS UNIQUE}), (:Person =>)-[:WORKS_FOR => {role :: STRING}]->() }",
        ExecutionOptions::default(),
    )
    .expect("initial graph type");

    let before_invalid_alter = branch_head(&connection, "main").expect("head before alter");
    let error = execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE ALTER { (:Person => :Resident {name :: STRING IS KEY}) }",
        ExecutionOptions::default(),
    )
    .expect_err("ALTER must not add or modify key/unique constraints");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert_eq!(
        branch_head(&connection, "main").expect("head after rejected alter"),
        before_invalid_alter
    );

    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE ALTER { (:Person => :Citizen {age :: INTEGER}) }",
        ExecutionOptions::default(),
    )
    .expect("alter element type without key/unique");
    let altered = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("altered head"),
    )
    .expect("altered schema");
    assert!(altered.constraints.values().any(|constraint| {
        matches!(
            constraint.kind,
            lithograph_core::storage::ConstraintDefinitionKind::Unique
        ) && constraint.origin.as_deref() == Some("graph/node/Person")
    }));

    let error = execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE DROP { (:Person =>) }",
        ExecutionOptions::default(),
    )
    .expect_err("referenced node element type must not be dropped");
    assert_eq!(error.kind, QueryErrorKind::Schema);

    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE DROP { ()-[:WORKS_FOR =>]->() }",
        ExecutionOptions::default(),
    )
    .expect("drop relationship by identifying type");

    let before_mismatch = branch_head(&connection, "main").expect("head before mismatch");
    let error = execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE DROP { (:Person => :Wrong {age :: INTEGER}) }",
        ExecutionOptions::default(),
    )
    .expect_err("full DROP definition must match exactly");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert_eq!(
        branch_head(&connection, "main").expect("head after mismatch"),
        before_mismatch
    );

    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE DROP { (:Person => :Citizen {age :: INTEGER}) }",
        ExecutionOptions::default(),
    )
    .expect("drop exact node element definition");
    let dropped = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("dropped head"),
    )
    .expect("schema after drop");
    assert!(!dropped.graph_nodes.contains_key("Person"));
    let unique_name = dropped
        .constraints
        .values()
        .find(|constraint| {
            matches!(
                constraint.kind,
                lithograph_core::storage::ConstraintDefinitionKind::Unique
            ) && constraint.origin.is_none()
        })
        .expect("unique constraint survives element drop")
        .name
        .clone();
    execute(
        &connection,
        &format!("ALTER CURRENT GRAPH TYPE DROP {{ CONSTRAINT {unique_name} }}"),
        ExecutionOptions::default(),
    )
    .expect("drop detached key/unique by Graph Type constraint name");
    let after_constraint_drop = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("head after constraint drop"),
    )
    .expect("schema after constraint drop");
    assert!(!after_constraint_drop.constraints.contains_key(&unique_name));
}

#[test]
fn graph_type_relationship_endpoint_identity_is_versioned_and_round_trips() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}), (:Person)-[:KNOWS => {since :: INTEGER}]->(:Person =>) }",
        ExecutionOptions::default(),
    )
    .expect("graph type with mixed endpoint identity");

    let head = branch_head(&connection, "main").expect("head");
    let schema = SchemaState::load(&connection, head).expect("schema");
    let knows = schema
        .graph_relationships
        .get("KNOWS")
        .expect("KNOWS element type");
    assert_eq!(knows.source_label.as_deref(), Some("Person"));
    assert!(!knows.source_identifying);
    assert_eq!(knows.target_label.as_deref(), Some("Person"));
    assert!(knows.target_identifying);

    let (shown, _) = execute(
        &connection,
        "SHOW CURRENT GRAPH TYPE",
        ExecutionOptions::default(),
    )
    .expect("show graph type");
    let Value::String(specification) = &shown[0][0] else {
        panic!("graph type specification must be text")
    };
    assert!(specification.contains("(:`Person`)-[:`KNOWS` =>"));
    assert!(specification.contains("->(:`Person` =>)"));

    let round_trip = fresh_storage();
    execute(
        &round_trip,
        &format!("ALTER CURRENT GRAPH TYPE SET {specification}"),
        ExecutionOptions::default(),
    )
    .expect("round-trip graph type");
    let round_trip_schema = SchemaState::load(
        &round_trip,
        branch_head(&round_trip, "main").expect("round-trip head"),
    )
    .expect("round-trip schema");
    assert_eq!(round_trip_schema, schema);

    let non_identifying = fresh_storage();
    execute(
        &non_identifying,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}), (:Person)-[:KNOWS => {since :: INTEGER}]->() }",
        ExecutionOptions::default(),
    )
    .expect("non-identifying endpoint");
    execute(
        &non_identifying,
        "ALTER CURRENT GRAPH TYPE DROP { (:Person =>) }",
        ExecutionOptions::default(),
    )
    .expect("non-identifying endpoint must not pin the node element type");
}

#[test]
fn graph_type_rejects_missing_identifying_endpoint_and_empty_element_definitions() {
    for query in [
        "ALTER CURRENT GRAPH TYPE SET { (:Missing =>)-[:R => {x :: INTEGER}]->() }",
        "ALTER CURRENT GRAPH TYPE SET { (:Person =>) }",
        "ALTER CURRENT GRAPH TYPE SET { ()-[:R =>]->() }",
    ] {
        let connection = fresh_storage();
        let before = branch_head(&connection, "main").expect("head before invalid graph type");
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("invalid Graph Type must fail");
        assert_eq!(error.kind, QueryErrorKind::Schema, "{query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after invalid graph type"),
            before,
            "{query}"
        );
    }
}

#[test]
fn graph_type_rejects_invalid_identifier_and_independent_constraint_combinations() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => :Resident {name :: STRING}) }",
        ExecutionOptions::default(),
    )
    .expect("initial graph type");

    for query in [
        "ALTER CURRENT GRAPH TYPE ADD { (:Resident => {id :: INTEGER}) }",
        "ALTER CURRENT GRAPH TYPE ADD { CONSTRAINT person_age FOR (n:Person) REQUIRE n.age IS :: INTEGER }",
        "ALTER CURRENT GRAPH TYPE ADD { (:Person => {id :: INTEGER}) }",
    ] {
        let before = branch_head(&connection, "main").expect("head before invalid add");
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("invalid Graph Type extension must fail");
        assert_eq!(error.kind, QueryErrorKind::Schema, "{query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after invalid add"),
            before,
            "{query}"
        );
    }

    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE ADD { CONSTRAINT company_name FOR (n:Company) REQUIRE n.name IS UNIQUE }",
        ExecutionOptions::default(),
    )
    .expect("key/unique may exist before an element type is added");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE ADD { (:Company => {name :: STRING}) }",
        ExecutionOptions::default(),
    )
    .expect("key/unique on the identifier remains valid");
}

#[test]
fn dependent_graph_type_constraints_cannot_be_dropped_independently() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING NOT NULL}) }",
        ExecutionOptions::default(),
    )
    .expect("graph type");
    let schema = SchemaState::load(&connection, branch_head(&connection, "main").expect("head"))
        .expect("schema");
    let dependent = schema
        .constraints
        .values()
        .find(|constraint| {
            constraint.origin.as_deref() == Some("graph/node/Person")
                && matches!(
                    constraint.kind,
                    lithograph_core::storage::ConstraintDefinitionKind::Type { .. }
                )
        })
        .expect("dependent type constraint")
        .name
        .clone();

    for query in [
        format!("DROP CONSTRAINT {dependent}"),
        format!("ALTER CURRENT GRAPH TYPE DROP {{ CONSTRAINT {dependent} }}"),
    ] {
        let before = branch_head(&connection, "main").expect("head before dependent drop");
        let error = execute(&connection, &query, ExecutionOptions::default())
            .expect_err("dependent constraints must move with their element type");
        assert_eq!(error.kind, QueryErrorKind::Schema, "{query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after dependent drop"),
            before,
            "{query}"
        );
    }
}

#[test]
fn graph_type_require_builds_exact_composite_keys() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (p:Person => {id :: INTEGER, tenant :: STRING}) REQUIRE (p.id, p.tenant) IS NODE KEY, ()-[r:MEMBER_OF => {id :: INTEGER, tenant :: STRING}]->() REQUIRE (r.id, r.tenant) IS RELATIONSHIP KEY }",
        ExecutionOptions::default(),
    )
    .expect("composite graph type keys");
    let schema = SchemaState::load(
        &connection,
        branch_head(&connection, "main").expect("schema head"),
    )
    .expect("schema");
    assert!(schema.constraints.values().any(|constraint| {
        constraint.properties == vec!["id".to_owned(), "tenant".to_owned()]
            && matches!(
                constraint.target,
                lithograph_core::storage::SchemaTarget::Node { .. }
            )
            && matches!(
                constraint.kind,
                lithograph_core::storage::ConstraintDefinitionKind::Key
            )
    }));
    assert!(schema.constraints.values().any(|constraint| {
        constraint.properties == vec!["id".to_owned(), "tenant".to_owned()]
            && matches!(
                constraint.target,
                lithograph_core::storage::SchemaTarget::Relationship { .. }
            )
            && matches!(
                constraint.kind,
                lithograph_core::storage::ConstraintDefinitionKind::Key
            )
    }));

    execute(
        &connection,
        "CREATE (a:Person {id:1, tenant:'a'}), (b:Person {id:1, tenant:'b'}), (a)-[:MEMBER_OF {id:1, tenant:'a'}]->(b) FINISH",
        ExecutionOptions::default(),
    )
    .expect("distinct composite keys");
    let before = branch_head(&connection, "main").expect("head before duplicate");
    let error = execute(
        &connection,
        "CREATE (:Person {id:1, tenant:'a'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("duplicate composite node key must fail");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after duplicate"),
        before
    );
}

#[test]
fn schema_ddl_conflicts_and_idempotent_forms_match_index_constraint_rules() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE RANGE INDEX person_age FOR (n:Person) ON (n.age)",
        ExecutionOptions::default(),
    )
    .expect("range index");

    let before_duplicate = branch_head(&connection, "main").expect("before duplicate");
    let error = execute(
        &connection,
        "CREATE RANGE INDEX person_age_2 FOR (n:Person) ON (n.age)",
        ExecutionOptions::default(),
    )
    .expect_err("equivalent range index must fail");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert_eq!(
        branch_head(&connection, "main").expect("after duplicate"),
        before_duplicate
    );

    let before_noop = branch_head(&connection, "main").expect("before IF NOT EXISTS");
    execute(
        &connection,
        "CREATE RANGE INDEX person_age_2 IF NOT EXISTS FOR (n:Person) ON (n.age)",
        ExecutionOptions::default(),
    )
    .expect("equivalent IF NOT EXISTS must not fail");
    let after_noop = branch_head(&connection, "main").expect("after IF NOT EXISTS");
    assert_ne!(
        after_noop, before_noop,
        "successful schema command still records write intent"
    );

    execute(
        &connection,
        "CREATE CONSTRAINT person_name FOR (n:Person) REQUIRE n.name IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("unique constraint");
    for query in [
        "CREATE RANGE INDEX other_name FOR (n:Person) ON (n.name)",
        "CREATE RANGE INDEX other_name IF NOT EXISTS FOR (n:Person) ON (n.name)",
        "CREATE CONSTRAINT person_age_unique FOR (n:Person) REQUIRE n.age IS UNIQUE",
        "CREATE CONSTRAINT person_age_unique IF NOT EXISTS FOR (n:Person) REQUIRE n.age IS UNIQUE",
        "CREATE RANGE INDEX person_name IF NOT EXISTS FOR (n:Other) ON (n.value)",
    ] {
        let before = branch_head(&connection, "main").expect("head before schema conflict");
        let error = execute(&connection, query, ExecutionOptions::default()).expect_err(query);
        assert_eq!(error.kind, QueryErrorKind::Schema, "{query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after conflict"),
            before,
            "{query}"
        );
    }

    execute(
        &connection,
        "CREATE LOOKUP INDEX node_lookup FOR (n) ON EACH labels(n)",
        ExecutionOptions::default(),
    )
    .expect("node lookup");
    let error = execute(
        &connection,
        "CREATE LOOKUP INDEX node_lookup_2 FOR (n) ON EACH labels(n)",
        ExecutionOptions::default(),
    )
    .expect_err("second node lookup must fail");
    assert_eq!(error.kind, QueryErrorKind::Schema);
}

#[test]
fn invalid_existing_data_rejects_schema_without_commit() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name: 42}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed invalid data");
    let before = branch_head(&connection, "main").expect("head before schema");

    let error = execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}) }",
        ExecutionOptions::default(),
    )
    .expect_err("existing invalid data must reject schema");

    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after"),
        before
    );
}

#[test]
fn write_constraint_violation_does_not_commit() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}) }",
        ExecutionOptions::default(),
    )
    .expect("set graph type");
    let before = branch_head(&connection, "main").expect("head before write");

    let error = execute(
        &connection,
        "CREATE (:Person {name: 42}) FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("invalid write must fail");

    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after"),
        before
    );
}

#[test]
fn schema_ddl_rejects_graph_view_without_commit() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before");
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["Hidden"]}}"#)
        .expect("graph view options");
    let error = execute(
        &connection,
        "CREATE RANGE INDEX person_name FOR (n:Person) ON (n.name)",
        options,
    )
    .expect_err("schema DDL must reject graph view");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(
        branch_head(&connection, "main").expect("head after"),
        before
    );
}

#[test]
fn branch_schema_histories_are_isolated() {
    let connection = fresh_storage();
    let main = branch_head(&connection, "main").expect("main head");
    create_branch(&connection, "feature", main).expect("feature branch");
    let options = ExecutionOptions::parse_text(r#"{"branch":"feature"}"#).expect("feature options");
    execute(
        &connection,
        "CREATE RANGE INDEX person_name FOR (n:Person) ON (n.name)",
        options,
    )
    .expect("feature index");

    let feature = branch_head(&connection, "feature").expect("feature head");
    assert!(
        !SchemaState::load(&connection, main)
            .expect("main schema")
            .indexes
            .contains_key("person_name")
    );
    assert!(
        SchemaState::load(&connection, feature)
            .expect("feature schema")
            .indexes
            .contains_key("person_name")
    );
}

#[test]
fn hidden_unique_conflict_is_still_a_constraint_error() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {id :: INTEGER IS NODE UNIQUE}) }",
        ExecutionOptions::default(),
    )
    .expect("unique graph type");
    execute(
        &connection,
        "CREATE (:Person:Visible {id:1}), (:Person:Hidden {id:2}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed distinct values");
    let before = branch_head(&connection, "main").expect("head before conflict");
    let options = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("visible view");

    let error = execute(&connection, "MATCH (n:Person) SET n.id = 2 FINISH", options)
        .expect_err("hidden conflict must reject");
    assert_eq!(error.kind, QueryErrorKind::Constraint);
    assert_eq!(
        branch_head(&connection, "main").expect("head after"),
        before
    );
}

#[test]
fn show_surfaces_read_versioned_schema_state() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => :Resident {name :: STRING NOT NULL}), (:Person)-[:WORKS_FOR => {role :: STRING}]->(:Company), CONSTRAINT resident_code FOR (n:Resident) REQUIRE n.code IS :: STRING }",
        ExecutionOptions::default(),
    )
    .expect("graph type");
    execute(
        &connection,
        "CREATE CONSTRAINT person_name_unique FOR (n:Person) REQUIRE n.name IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("constraint");
    execute(
        &connection,
        "CREATE TEXT INDEX person_name_text FOR (n:Person) ON (n.name)",
        ExecutionOptions::default(),
    )
    .expect("text index");

    assert_graph_type_show_surfaces(&connection);
    assert_schema_catalog_show_surfaces(&connection);
}

fn assert_graph_type_show_surfaces(connection: &Connection) {
    let (graph_type, _) = execute(
        connection,
        "SHOW CURRENT GRAPH TYPE",
        ExecutionOptions::default(),
    )
    .expect("show graph type");
    assert_eq!(graph_type.len(), 1);
    let Value::String(specification) = &graph_type[0][0] else {
        panic!("graph type specification must be a string")
    };
    assert!(specification.contains("(:`Person` => :`Resident`"));
    assert!(specification.contains("CONSTRAINT `person_name_unique`"));
    assert!(specification.contains("CONSTRAINT `resident_code`"));
    assert!(!specification.contains("graph_constraint_"));
    let round_trip = fresh_storage();
    execute(
        &round_trip,
        &format!("ALTER CURRENT GRAPH TYPE SET {specification}"),
        ExecutionOptions::default(),
    )
    .expect("canonical graph type specification must be reusable as SET input");

    let (as_graph, _) = execute(
        connection,
        "SHOW CURRENT GRAPH TYPE AS GRAPH",
        ExecutionOptions::default(),
    )
    .expect("show graph type as graph");
    let Value::List(nodes) = &as_graph[0][0] else {
        panic!("AS GRAPH nodes must be a list")
    };
    let Value::List(relationships) = &as_graph[0][1] else {
        panic!("AS GRAPH relationships must be a list")
    };
    assert!(nodes.iter().any(|value| matches!(
        value,
        Value::Node(node)
            if node.labels == vec!["NodeElementType".to_owned()]
                && node.properties.get("label") == Some(&Value::String("Person".to_owned()))
                && node.element_id.starts_with('-')
    )));
    assert!(nodes.iter().any(|value| matches!(
        value,
        Value::Node(node)
            if node.labels == vec!["NodeLabel".to_owned()]
                && node.properties.get("label") == Some(&Value::String("Resident".to_owned()))
    )));
    assert!(relationships.iter().any(|value| matches!(
        value,
        Value::Relationship(relationship) if relationship.relationship_type == "IMPLIES"
    )));
    assert!(relationships.iter().any(|value| matches!(
        value,
        Value::Relationship(relationship)
            if relationship.relationship_type == "RELATIONSHIP_ELEMENT_TYPE"
                && relationship.properties.get("relationshipType")
                    == Some(&Value::String("WORKS_FOR".to_owned()))
    )));
}

fn assert_schema_catalog_show_surfaces(connection: &Connection) {
    let (indexes, _) = execute(
        connection,
        "SHOW INDEXES YIELD name, type RETURN name, type ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("show indexes");
    assert!(indexes.iter().any(|row| {
        row == &vec![
            Value::String("person_name_text".to_owned()),
            Value::String("TEXT".to_owned()),
        ]
    }));
    assert!(indexes.iter().any(|row| {
        row == &vec![
            Value::String("person_name_unique".to_owned()),
            Value::String("RANGE".to_owned()),
        ]
    }));

    let (constraints, _) = execute(
        connection,
        "SHOW CONSTRAINTS YIELD name, type, enforcedLabel, classification, propertyType, createStatement RETURN name, type, enforcedLabel, classification, propertyType, createStatement ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("show constraints");
    assert!(constraints.iter().any(|row| {
        row[0] == Value::String("person_name_unique".to_owned())
            && row[1] == Value::String("NODE_PROPERTY_UNIQUENESS".to_owned())
            && row[2] == Value::Null
            && row[3] == Value::String("undesignated".to_owned())
            && row[5] != Value::Null
    }));
    assert!(constraints.iter().any(|row| {
        row[1] == Value::String("NODE_PROPERTY_TYPE".to_owned())
            && row[3] == Value::String("dependent".to_owned())
            && row[4] == Value::String("STRING".to_owned())
            && row[5] == Value::Null
    }));
    assert!(constraints.iter().any(|row| {
        row[1] == Value::String("NODE_PROPERTY_EXISTENCE".to_owned())
            && row[3] == Value::String("dependent".to_owned())
            && row[5] == Value::Null
    }));
    assert!(constraints.iter().any(|row| {
        row[1] == Value::String("NODE_LABEL_EXISTENCE".to_owned())
            && row[2] == Value::String("Resident".to_owned())
            && row[3] == Value::String("dependent".to_owned())
    }));
    assert!(constraints.iter().any(|row| {
        row[0] == Value::String("resident_code".to_owned())
            && row[1] == Value::String("NODE_PROPERTY_TYPE".to_owned())
            && row[3] == Value::String("independent".to_owned())
            && row[5] != Value::Null
    }));
}

#[test]
fn standard_index_seeks_match_scans_and_rebuild_after_cache_loss() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person:Visible {name:'Alice', age:1, location:point({x:1, y:2})}), (:Person:Visible {name:'Bob', age:2, location:point({x:2, y:3})}), (:Person:Hidden {name:'Alfred', age:2, location:point({x:3, y:4})}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed people");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING, age :: INTEGER, location :: POINT}) }",
        ExecutionOptions::default(),
    )
    .expect("property type proof for typed standard indexes");
    let target = Value::Point(PointValue::new("cartesian", vec![1.0, 2.0]).expect("point"));
    let point_params = BTreeMap::from([("target".to_owned(), target)]);

    let (lookup_scan, _) = execute(
        &connection,
        "MATCH (n:Person) RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("lookup scan baseline");
    let (range_scan, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.age = 2 RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("range scan baseline");
    let (text_scan, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.name STARTS WITH 'Al' RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("text scan baseline");
    let (point_scan, _) = execute_params(
        &connection,
        "MATCH (n:Person) WHERE n.location = $target RETURN n.name ORDER BY n.name",
        point_params.clone(),
        ExecutionOptions::default(),
    )
    .expect("point scan baseline");

    for ddl in [
        "CREATE LOOKUP INDEX person_labels FOR (n) ON EACH labels(n)",
        "CREATE RANGE INDEX person_age FOR (n:Person) ON (n.age)",
        "CREATE TEXT INDEX person_name_text FOR (n:Person) ON (n.name)",
        "CREATE POINT INDEX person_location FOR (n:Person) ON (n.location)",
    ] {
        execute(&connection, ddl, ExecutionOptions::default()).expect("create standard index");
    }

    let (lookup_seek, _) = execute(
        &connection,
        "MATCH (n:Person) RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("lookup seek");
    let (range_seek, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.age = 2 RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("range seek");
    let (text_seek, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.name STARTS WITH 'Al' RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("text seek");
    let (point_seek, _) = execute_params(
        &connection,
        "MATCH (n:Person) WHERE n.location = $target RETURN n.name ORDER BY n.name",
        point_params.clone(),
        ExecutionOptions::default(),
    )
    .expect("point seek");
    assert_eq!(lookup_seek, lookup_scan);
    assert_eq!(range_seek, range_scan);
    assert_eq!(text_seek, text_scan);
    assert_eq!(point_seek, point_scan);

    assert_basic_standard_index_plans(&connection, &point_params);

    let view = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("visible graph view");
    let (visible, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.age = 2 RETURN n.name ORDER BY n.name",
        view,
    )
    .expect("view-filtered range seek");
    assert_eq!(visible, vec![vec![Value::String("Bob".to_owned())]]);

    drop_query_local_standard_index_cache(&connection);
    let (rebuilt, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.age = 2 RETURN n.name ORDER BY n.name",
        ExecutionOptions::default(),
    )
    .expect("rebuild range cache");
    assert_eq!(rebuilt, range_scan);
}

fn drop_query_local_standard_index_cache(connection: &Connection) {
    connection
        .execute_batch(
            "DROP VIEW temp._lithograph_standard_index_cache;\
             DROP TABLE temp._lithograph_standard_index_cache_local;\
             DROP TABLE temp._lithograph_standard_index_cache_meta;\
             DROP TABLE temp._lithograph_standard_index_cache_config;",
        )
        .expect("delete derived cache");
}

fn assert_basic_standard_index_plans(
    connection: &Connection,
    point_params: &BTreeMap<String, Value>,
) {
    for (query, expected_index) in [
        ("EXPLAIN MATCH (n:Person) RETURN n.name", "person_labels"),
        (
            "EXPLAIN MATCH (n:Person) WHERE n.age = 2 RETURN n.name",
            "person_age",
        ),
        (
            "EXPLAIN MATCH (n:Person) WHERE n.name STARTS WITH 'Al' RETURN n.name",
            "person_name_text",
        ),
    ] {
        let (rows, _) = execute(connection, query, ExecutionOptions::default()).expect("explain");
        assert!(matches!(&rows[0][0], Value::String(plan) if plan.contains(expected_index)));
    }
    let (point_plan, _) = execute_params(
        connection,
        "EXPLAIN MATCH (n:Person) WHERE n.location = $target RETURN n.name",
        point_params.clone(),
        ExecutionOptions::default(),
    )
    .expect("point explain");
    assert!(matches!(&point_plan[0][0], Value::String(plan) if plan.contains("person_location")));
}

#[test]
fn range_index_preserves_numeric_and_temporal_ordering_semantics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Metric {name:'int-one', value:1}), (:Metric {name:'float-one', value:1.0}), (:Metric {name:'safe-float', value:9007199254740992.0}), (:Metric {name:'huge-int', value:9007199254740993}), (:Metric {name:'prefix-a', text:'alpha'}), (:Metric {name:'prefix-b', text:'alpine'}), (:Metric {name:'prefix-c', text:'beta'}), (:Event {name:'a', day:date('2026-01-01')}), (:Event {name:'b', day:date('2026-01-02')}), (:Event {name:'c', day:date('2026-01-03')}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed range values");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Metric => {value :: INTEGER | FLOAT, text :: STRING}), (:Event => {day :: DATE}) }",
        ExecutionOptions::default(),
    )
    .expect("property type proof for ordered range seeks");

    let queries = [
        "MATCH (n:Metric) WHERE n.value = 1 RETURN n.name ORDER BY n.name",
        "MATCH (n:Metric) WHERE n.value = 1.0 RETURN n.name ORDER BY n.name",
        "MATCH (n:Metric) WHERE n.value IN [1.0] RETURN n.name ORDER BY n.name",
        "MATCH (n:Metric) WHERE n.value > 9007199254740992.0 RETURN n.name ORDER BY n.name",
        "MATCH (n:Metric) WHERE n.text STARTS WITH 'al' RETURN n.name ORDER BY n.name",
        "MATCH (n:Event) WHERE n.day >= date('2026-01-02') RETURN n.name ORDER BY n.name",
    ];
    let mut baselines = Vec::new();
    for query in queries {
        baselines.push(
            execute(&connection, query, ExecutionOptions::default())
                .unwrap_or_else(|error| panic!("baseline {query}: {error}"))
                .0,
        );
    }
    assert_eq!(
        baselines[0],
        vec![
            vec![Value::String("float-one".to_owned())],
            vec![Value::String("int-one".to_owned())],
        ]
    );
    assert_eq!(baselines[0], baselines[1]);
    assert_eq!(baselines[0], baselines[2]);
    assert_eq!(
        baselines[3],
        vec![vec![Value::String("huge-int".to_owned())]]
    );

    for ddl in [
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
        "CREATE RANGE INDEX metric_text FOR (n:Metric) ON (n.text)",
        "CREATE RANGE INDEX event_day FOR (n:Event) ON (n.day)",
    ] {
        execute(&connection, ddl, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("DDL {ddl}: {error}"));
    }

    let expected_indexes = [
        "metric_value",
        "metric_value",
        "metric_value",
        "metric_value",
        "metric_text",
        "event_day",
    ];
    for (index, query) in queries.into_iter().enumerate() {
        let indexed = execute(&connection, query, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("indexed {query}: {error}"))
            .0;
        assert_eq!(indexed, baselines[index], "{query}");
        let (plan, _) = execute(
            &connection,
            &format!("EXPLAIN {query}"),
            ExecutionOptions::default(),
        )
        .unwrap_or_else(|error| panic!("explain {query}: {error}"));
        assert!(
            matches!(&plan[0][0], Value::String(value) if value.contains(expected_indexes[index])),
            "{query}: {plan:?}"
        );
    }
}

#[test]
fn show_constraint_filters_and_singular_surfaces_follow_cypher25() {
    let connection = fresh_storage();
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => :Resident {name :: STRING NOT NULL}), ()-[r:KNOWS => {since :: INTEGER NOT NULL}]->() }",
        ExecutionOptions::default(),
    )
    .expect("graph type");
    execute(
        &connection,
        "CREATE CONSTRAINT person_unique FOR (n:Person) REQUIRE n.name IS UNIQUE",
        ExecutionOptions::default(),
    )
    .expect("node uniqueness");
    execute(
        &connection,
        "CREATE CONSTRAINT knows_key FOR ()-[r:KNOWS]-() REQUIRE r.since IS REL KEY",
        ExecutionOptions::default(),
    )
    .expect("relationship key using REL shorthand");
    execute(
        &connection,
        "CREATE RANGE INDEX person_name_range FOR (n:Person) ON (n.name)",
        ExecutionOptions::default(),
    )
    .expect_err("constraint backing range index blocks duplicate explicit range index");
    execute(
        &connection,
        "CREATE TEXT INDEX person_name_text FOR (n:Person) ON (n.name)",
        ExecutionOptions::default(),
    )
    .expect("text index");

    let cases = [
        (
            "SHOW NODE EXISTENCE CONSTRAINT YIELD type RETURN type ORDER BY type",
            vec!["NODE_LABEL_EXISTENCE", "NODE_PROPERTY_EXISTENCE"],
        ),
        (
            "SHOW PROPERTY EXISTENCE CONSTRAINTS YIELD type RETURN type ORDER BY type",
            vec!["NODE_PROPERTY_EXISTENCE", "RELATIONSHIP_PROPERTY_EXISTENCE"],
        ),
        (
            "SHOW REL PROPERTY TYPE CONSTRAINT YIELD type RETURN type ORDER BY type",
            vec!["RELATIONSHIP_PROPERTY_TYPE"],
        ),
        (
            "SHOW UNIQUE CONSTRAINT YIELD type RETURN type ORDER BY type",
            vec!["NODE_PROPERTY_UNIQUENESS"],
        ),
        (
            "SHOW KEY CONSTRAINTS YIELD type RETURN type ORDER BY type",
            vec!["RELATIONSHIP_KEY"],
        ),
    ];
    for (query, expected_types) in cases {
        let (rows, _) = execute(&connection, query, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("{query}: {error}"));
        let actual = rows
            .into_iter()
            .map(|row| match &row[0] {
                Value::String(value) => value.clone(),
                other => panic!("{query}: expected string type, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected_types, "{query}");
    }

    let (all_constraints, _) = execute(
        &connection,
        "SHOW ALL CONSTRAINT YIELD name RETURN name ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("SHOW ALL CONSTRAINT singular");
    let (default_constraints, _) = execute(
        &connection,
        "SHOW CONSTRAINTS YIELD name RETURN name ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("SHOW CONSTRAINTS");
    assert_eq!(all_constraints, default_constraints);

    let (all_indexes, _) = execute(
        &connection,
        "SHOW ALL INDEX YIELD name RETURN name ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("SHOW ALL INDEX singular");
    let (default_indexes, _) = execute(
        &connection,
        "SHOW INDEXES YIELD name RETURN name ORDER BY name",
        ExecutionOptions::default(),
    )
    .expect("SHOW INDEXES");
    assert_eq!(all_indexes, default_indexes);
}

#[test]
fn point_index_spatial_seek_preserves_wgs84_dateline_and_distance_results() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Place {name:'east', location:point({longitude:179.0, latitude:0.0})}), (:Place {name:'west', location:point({longitude:-179.0, latitude:0.0})}), (:Place {name:'far', location:point({longitude:0.0, latitude:0.0})}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed WGS84 points");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Place => {location :: POINT}) }",
        ExecutionOptions::default(),
    )
    .expect("property type proof for point spatial seek");
    let queries = [
        "MATCH (n:Place) WHERE point.withinBBox(n.location, point({longitude:170.0, latitude:-10.0}), point({longitude:-170.0, latitude:10.0})) RETURN n.name ORDER BY n.name",
        "MATCH (n:Place) WHERE point.distance(n.location, point({longitude:179.5, latitude:0.0})) <= 200000.0 RETURN n.name ORDER BY n.name",
    ];
    let baselines = queries
        .iter()
        .map(|query| {
            execute(&connection, query, ExecutionOptions::default())
                .unwrap_or_else(|error| panic!("baseline {query}: {error}"))
                .0
        })
        .collect::<Vec<_>>();
    assert_eq!(
        baselines[0],
        vec![
            vec![Value::String("east".to_owned())],
            vec![Value::String("west".to_owned())],
        ]
    );
    assert_eq!(baselines[0], baselines[1]);

    execute(
        &connection,
        "CREATE POINT INDEX place_location FOR (n:Place) ON (n.location)",
        ExecutionOptions::default(),
    )
    .expect("point index");
    for (index, query) in queries.iter().enumerate() {
        let indexed = execute(&connection, query, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("indexed {query}: {error}"))
            .0;
        assert_eq!(indexed, baselines[index], "{query}");
        let (plan, _) = execute(
            &connection,
            &format!("EXPLAIN {query}"),
            ExecutionOptions::default(),
        )
        .unwrap_or_else(|error| panic!("explain {query}: {error}"));
        assert!(
            matches!(&plan[0][0], Value::String(value) if value.contains("place_location")),
            "{query}: {plan:?}"
        );
    }
}

#[test]
fn standard_indexes_cover_composite_range_text_spatial_and_relationship_seeks() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:Person:Visible {name:'Alice', age:1, score:8, location:point({x:1, y:1})}), (b:Person:Visible {name:'Bob', age:2, score:10, location:point({x:2, y:2})}), (c:Person:Hidden {name:'Carol', age:3, score:12, location:point({x:3, y:3})}), (a)-[:ROUTE {name:'alpha', distance:5, location:point({x:1, y:1})}]->(b), (b)-[:ROUTE {name:'beta', distance:15, location:point({x:2, y:2})}]->(c), (c)-[:ROUTE {name:'gamma', distance:25, location:point({x:3, y:3})}]->(a) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed indexed graph");
    execute(
        &connection,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING, age :: INTEGER, score :: INTEGER, location :: POINT}), ()-[r:ROUTE => {name :: STRING, distance :: INTEGER, location :: POINT}]->() }",
        ExecutionOptions::default(),
    )
    .expect("property type proof for typed and ordered standard indexes");

    let queries = [
        "MATCH (n:Person) WHERE n.age >= 2 RETURN n.name ORDER BY n.name",
        "MATCH (n:Person) WHERE n.age IN [1,3] RETURN n.name ORDER BY n.name",
        "MATCH (n:Person) WHERE n.age >= 2 AND n.score <= 10 RETURN n.name ORDER BY n.name",
        "MATCH (n:Person) WHERE n.name IN ['Alice','Carol'] RETURN n.name ORDER BY n.name",
        "MATCH (n:Person) WHERE point.withinBBox(n.location, point({x:1.5,y:1.5}), point({x:3.5,y:3.5})) RETURN n.name ORDER BY n.name",
        "MATCH (n:Person) WHERE point.distance(n.location, point({x:1,y:1})) <= 1.5 RETURN n.name ORDER BY n.name",
        "MATCH ()-[r:ROUTE]->() RETURN r.distance ORDER BY r.distance",
        "MATCH ()-[r:ROUTE]->() WHERE r.distance < 20 RETURN r.distance ORDER BY r.distance",
        "MATCH ()-[r:ROUTE]->() WHERE r.name CONTAINS 'a' RETURN r.name ORDER BY r.name",
        "MATCH ()-[r:ROUTE]->() WHERE point.withinBBox(r.location, point({x:0.5,y:0.5}), point({x:2.5,y:2.5})) RETURN r.distance ORDER BY r.distance",
    ];
    let mut baselines = Vec::new();
    for query in queries {
        baselines.push(
            execute(&connection, query, ExecutionOptions::default())
                .unwrap_or_else(|error| panic!("baseline {query}: {error}"))
                .0,
        );
    }

    for ddl in [
        "CREATE LOOKUP INDEX node_lookup FOR (n) ON EACH labels(n)",
        "CREATE LOOKUP INDEX relationship_lookup FOR ()-[r]-() ON EACH type(r)",
        "CREATE RANGE INDEX person_age FOR (n:Person) ON (n.age)",
        "CREATE RANGE INDEX person_age_score FOR (n:Person) ON (n.age, n.score)",
        "CREATE TEXT INDEX person_name FOR (n:Person) ON (n.name)",
        "CREATE POINT INDEX person_location FOR (n:Person) ON (n.location)",
        "CREATE RANGE INDEX route_distance FOR ()-[r:ROUTE]-() ON (r.distance)",
        "CREATE TEXT INDEX route_name FOR ()-[r:ROUTE]-() ON (r.name)",
        "CREATE POINT INDEX route_location FOR ()-[r:ROUTE]-() ON (r.location)",
    ] {
        execute(&connection, ddl, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("DDL {ddl}: {error}"));
    }

    let expected_indexes = [
        "person_age",
        "person_age",
        "person_age_score",
        "person_name",
        "person_location",
        "person_location",
        "relationship_lookup",
        "route_distance",
        "route_name",
        "route_location",
    ];
    for (index, query) in queries.into_iter().enumerate() {
        let rows = execute(&connection, query, ExecutionOptions::default())
            .unwrap_or_else(|error| panic!("indexed {query}: {error}"))
            .0;
        assert_eq!(rows, baselines[index], "{query}");
        let (plan, _) = execute(
            &connection,
            &format!("EXPLAIN {query}"),
            ExecutionOptions::default(),
        )
        .unwrap_or_else(|error| panic!("explain {query}: {error}"));
        assert!(
            matches!(&plan[0][0], Value::String(value) if value.contains(expected_indexes[index])),
            "{query}: {plan:?}"
        );
    }

    let view = ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#)
        .expect("visible view");
    let (visible_nodes, _) = execute(
        &connection,
        "MATCH (n:Person) WHERE n.age >= 2 RETURN n.name ORDER BY n.name",
        view.clone(),
    )
    .expect("view-aware node range seek");
    assert_eq!(visible_nodes, vec![vec![Value::String("Bob".to_owned())]]);
    let (visible_relationships, _) = execute(
        &connection,
        "MATCH ()-[r:ROUTE]->() WHERE r.distance < 20 RETURN r.distance ORDER BY r.distance",
        view,
    )
    .expect("view-aware relationship range seek");
    assert_eq!(visible_relationships, vec![vec![Value::Integer(5)]]);
}
