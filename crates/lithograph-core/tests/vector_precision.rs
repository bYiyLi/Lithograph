use std::collections::BTreeMap;

use lithograph_core::cypher::{Value, VectorCoordinateType, VectorValue, VectorValues};
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, QueryResult, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL);",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
) -> QueryResult<Vec<Vec<Value>>> {
    let prepared = prepare(connection, query, params, ExecutionOptions::default())?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn integer_vector(values: Vec<i64>) -> Value {
    Value::Vector(
        VectorValue::new(VectorCoordinateType::I64, VectorValues::I64(values))
            .expect("valid INTEGER64 vector"),
    )
}

#[test]
fn integer64_constructors_preserve_list_string_and_storage_coordinates() {
    let connection = fresh_storage();
    let values = vec![
        i64::MIN,
        -9_007_199_254_740_993,
        9_007_199_254_740_993,
        i64::MAX,
    ];
    let params = BTreeMap::from([
        (
            "values".to_owned(),
            Value::List(values.iter().copied().map(Value::Integer).collect()),
        ),
        (
            "text".to_owned(),
            Value::String(format!(
                "[{}]",
                values
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            )),
        ),
    ]);
    let expected = integer_vector(values);
    assert_eq!(
        execute(
            &connection,
            "CREATE (n:VectorPrecision {list:vector($values,4,INTEGER64),\
             text:vector($text,4,INTEGER64)}) RETURN n.list,n.text",
            params,
        )
        .expect("construct and store vectors"),
        vec![vec![expected.clone(), expected.clone()]]
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:VectorPrecision) RETURN n.list,n.text",
            BTreeMap::new(),
        )
        .expect("reload canonical vector coordinates"),
        vec![vec![expected.clone(), expected]]
    );
}

#[test]
fn integer64_float_conversion_checks_exclusive_upper_bound() {
    let connection = fresh_storage();
    for value in [
        9_223_372_036_854_775_808.0,
        -9_223_372_036_854_777_856.0,
        1.5,
        f64::INFINITY,
        f64::NAN,
    ] {
        let error = execute(
            &connection,
            "RETURN vector([$value],1,INTEGER64)",
            BTreeMap::from([("value".to_owned(), Value::Float(value))]),
        )
        .expect_err("unrepresentable INTEGER64 coordinate must fail");
        assert_eq!(error.kind, QueryErrorKind::Type, "{value:?}: {error}");
    }
    assert_eq!(
        execute(
            &connection,
            "RETURN vector([$value,2.0],2,INTEGER64)",
            BTreeMap::from([("value".to_owned(), Value::Float(i64::MIN as f64))]),
        )
        .expect("representable integral floats"),
        vec![vec![integer_vector(vec![i64::MIN, 2])]]
    );
}

#[test]
fn integer_vector_string_overflow_does_not_round_into_range() {
    let connection = fresh_storage();
    for coordinate_type in ["INTEGER8", "INTEGER16", "INTEGER32", "INTEGER64"] {
        for value in [
            "-9223372036854775809",
            "9223372036854775808",
            "+9223372036854775808",
        ] {
            let query = format!("RETURN vector($text,1,{coordinate_type})");
            let error = execute(
                &connection,
                &query,
                BTreeMap::from([("text".to_owned(), Value::String(format!("[ {value} ]")))]),
            )
            .expect_err("integer text outside i64 must fail before floating-point rounding");
            assert_eq!(error.kind, QueryErrorKind::Type, "{value}: {error}");
        }
    }
    for coordinate_type in [VectorCoordinateType::F32, VectorCoordinateType::F64] {
        let (name, values) = match coordinate_type {
            VectorCoordinateType::F32 => (
                "FLOAT32",
                VectorValues::F32(vec![i64::MIN as f32, -(i64::MIN as f32)]),
            ),
            _ => (
                "FLOAT64",
                VectorValues::F64(vec![i64::MIN as f64, -(i64::MIN as f64)]),
            ),
        };
        assert_eq!(
            execute(
                &connection,
                &format!("RETURN vector('-9223372036854775809,9223372036854775808',2,{name})"),
                BTreeMap::new(),
            )
            .expect("floating coordinates retain numeric String parsing"),
            vec![vec![Value::Vector(
                VectorValue::new(coordinate_type, values).expect("finite floating coordinates")
            )]]
        );
    }
    assert_eq!(
        execute(
            &connection,
            "RETURN vector('[-9223372036854775809.0,2e0]',2,INTEGER64)",
            BTreeMap::new(),
        )
        .expect("decimal and exponent text retain Float conversion semantics"),
        vec![vec![integer_vector(vec![i64::MIN, 2])]]
    );
}

#[test]
fn vector_width_casts_preserve_numeric_and_null_contracts() {
    let connection = fresh_storage();
    for coordinate_type in ["INTEGER8", "INTEGER16", "INTEGER32"] {
        for value in [Value::Integer(i64::MAX), Value::Float(1.5), Value::Null] {
            let query = format!("RETURN vector([$value],1,{coordinate_type})");
            let error = execute(
                &connection,
                &query,
                BTreeMap::from([("value".to_owned(), value)]),
            )
            .expect_err("invalid integer coordinate must fail");
            assert_eq!(error.kind, QueryErrorKind::Type, "{error}");
        }
    }
    assert_eq!(
        execute(
            &connection,
            "RETURN vector(null,1,INTEGER64),\
             vector_dimension_count(vector([1,2.5],2,FLOAT64)),\
             vector_norm(vector([3,4],2,FLOAT32),EUCLIDEAN)",
            BTreeMap::new(),
        )
        .expect("other Vector forms remain valid"),
        vec![vec![Value::Null, Value::Integer(2), Value::Float(5.0)]]
    );
}
