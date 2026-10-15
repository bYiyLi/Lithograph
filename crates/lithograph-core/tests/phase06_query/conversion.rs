use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryErrorKind, prepare};

use super::{fresh_storage, query_error, rows};

#[test]
fn uuid_string_conversion_agrees_for_typed_expressions_and_stored_properties() {
    let connection = fresh_storage();
    let canonical = "550e8400-e29b-41d4-a716-446655440000";
    assert_eq!(
        rows(
            &connection,
            "RETURN toString(uuid('550e8400-e29b-41d4-a716-446655440000')) AS text",
        ),
        vec![vec![Value::String(canonical.to_owned())]],
    );
    rows(
        &connection,
        "CREATE (:UuidConversion {id:uuid('550e8400-e29b-41d4-a716-446655440000')}) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH (n:UuidConversion) RETURN toString(n.id), toStringOrNull(n.id)",
        ),
        vec![vec![
            Value::String(canonical.to_owned()),
            Value::String(canonical.to_owned()),
        ]],
    );
    let error = prepare(
        &connection,
        "RETURN toString({key:1})",
        Default::default(),
        ExecutionOptions::default(),
    )
    .expect_err("Map conversion remains outside the frozen Profile");
    assert_eq!(error.kind, QueryErrorKind::Type);
}

#[test]
fn nested_path_functions_keep_their_own_result_and_property_owner() {
    let connection = fresh_storage();
    rows(
        &connection,
        "CREATE (:PathFunction {name:'Ada'})-[:READ {ordinal:1}]->(:PathFunction {name:'Lin'}) FINISH",
    );
    assert_eq!(
        rows(
            &connection,
            "MATCH p=(:PathFunction)-[:READ]->(:PathFunction) \
             RETURN [n IN nodes(p) | n.name], \
                    [r IN relationships(p) | r.ordinal], \
                    size(nodes(p)), type(head(relationships(p))), size([p])",
        ),
        vec![vec![
            Value::List(vec![
                Value::String("Ada".to_owned()),
                Value::String("Lin".to_owned())
            ]),
            Value::List(vec![Value::Integer(1)]),
            Value::Integer(2),
            Value::String("READ".to_owned()),
            Value::Integer(1),
        ]],
    );
    assert!(
        query_error(&connection, "MATCH p=()-[:READ]->() RETURN p.name").contains("Path values")
    );
    let error = prepare(
        &connection,
        "MATCH p=()-[:READ]->() RETURN size(p)",
        Default::default(),
        ExecutionOptions::default(),
    )
    .expect_err("size() still rejects a Path argument");
    assert!(matches!(
        error.kind,
        QueryErrorKind::Semantic | QueryErrorKind::Type
    ));
}
