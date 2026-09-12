use std::collections::BTreeMap;
use std::path::PathBuf;

use lithograph_core::cypher::{Value, parse};
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use lithograph_test_support::compatibility::{
    CompatibilityReport, ExpectedOutcome, Fixture, FixtureExecutor, FixtureStatus, ObservedError,
    ObservedOutcome, TypedValue, load_fixture_directory, run_fixture,
};
use rusqlite::Connection;

#[test]
fn enabled_cypher25_fixtures_execute_against_the_real_engine() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/cypher25");
    let fixtures = load_fixture_directory(&root).expect("CY25 fixture inventory");
    let mut executor = CoreExecutor;
    let results = fixtures
        .iter()
        .map(|fixture| run_fixture(fixture, &mut executor))
        .collect::<Vec<_>>();
    let report = CompatibilityReport::from_results("CY25-2026.08", results);

    assert_eq!(report.total, 17);
    assert_eq!(report.passed, 15, "{:#?}", report.results);
    assert_eq!(report.planned, 2, "{:#?}", report.results);
    assert_eq!(report.failed, 0, "{:#?}", report.results);
    assert!(report.results.iter().all(|result| {
        result.status != FixtureStatus::Planned
            || matches!(result.family.as_str(), "Graph Type" | "SEARCH")
    }));
}

struct CoreExecutor;

impl FixtureExecutor for CoreExecutor {
    fn execute(&mut self, fixture: &Fixture) -> Result<ObservedOutcome, String> {
        if matches!(fixture.expected, ExpectedOutcome::Parse { .. }) {
            return Ok(match parse(&fixture.query) {
                Ok(_) => ObservedOutcome::Parse {
                    valid: true,
                    error: None,
                },
                Err(error) => ObservedOutcome::Parse {
                    valid: false,
                    error: Some(ObservedError {
                        category: "SyntaxError".to_owned(),
                        line: Some(error.line),
                        column: Some(error.column),
                        message: error.message,
                    }),
                },
            });
        }

        let connection = fresh_storage()?;
        for setup in &fixture.initial_graph {
            execute_query(&connection, setup, BTreeMap::new())
                .map_err(|error| error.to_string())?;
        }
        let params = fixture
            .params
            .iter()
            .map(|(name, value)| Ok((name.clone(), input_value(value)?)))
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        match execute_query(&connection, &fixture.query, params) {
            Ok((columns, rows)) => Ok(ObservedOutcome::Rows {
                columns,
                rows: rows
                    .iter()
                    .map(|row| row.iter().map(output_value).collect())
                    .collect::<Result<_, _>>()?,
            }),
            Err(error) => Ok(ObservedOutcome::Error {
                error: observed_error(&error),
            }),
        }
    }
}

fn fresh_storage() -> Result<Connection, String> {
    let connection = Connection::open_in_memory().map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL);",
        )
        .map_err(|error| error.to_string())?;
    create_storage_schema(&connection).map_err(|error| error.to_string())?;
    initialize_root(&connection).map_err(|error| error.to_string())?;
    Ok(connection)
}

fn execute_query(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
) -> Result<(Vec<String>, Vec<Vec<Value>>), QueryError> {
    let prepared = prepare(connection, query, params, ExecutionOptions::default())?;
    let columns = prepared.columns.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 2)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok((columns, rows));
        }
    }
}

fn input_value(value: &TypedValue) -> Result<Value, String> {
    match value {
        TypedValue::Null => Ok(Value::Null),
        TypedValue::Boolean { value } => Ok(Value::Boolean(*value)),
        TypedValue::Integer { value } => Ok(Value::Integer(*value)),
        TypedValue::Float { value } => value
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|error| error.to_string()),
        TypedValue::String { value } => Ok(Value::String(value.clone())),
        TypedValue::List { value } => value
            .iter()
            .map(input_value)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        TypedValue::Map { value } => value
            .iter()
            .map(|(key, value)| Ok((key.clone(), input_value(value)?)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Value::Map),
    }
}

fn output_value(value: &Value) -> Result<TypedValue, String> {
    match value {
        Value::Null => Ok(TypedValue::Null),
        Value::Boolean(value) => Ok(TypedValue::Boolean { value: *value }),
        Value::Integer(value) => Ok(TypedValue::Integer { value: *value }),
        Value::Float(value) => Ok(TypedValue::Float {
            value: value.to_string(),
        }),
        Value::String(value) => Ok(TypedValue::String {
            value: value.clone(),
        }),
        Value::List(value) => Ok(TypedValue::List {
            value: value
                .iter()
                .map(output_value)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        Value::Map(value) => Ok(TypedValue::Map {
            value: value
                .iter()
                .map(|(key, value)| output_value(value).map(|encoded| (key.clone(), encoded)))
                .collect::<Result<BTreeMap<_, _>, String>>()?,
        }),
        other => Err(format!(
            "compatibility fixture encoder does not yet support {other:?}"
        )),
    }
}

fn observed_error(error: &QueryError) -> ObservedError {
    let category = match error.kind {
        QueryErrorKind::Parse => "SyntaxError",
        QueryErrorKind::Semantic => "SemanticError",
        QueryErrorKind::Type => "TypeError",
        QueryErrorKind::Schema => "SchemaError",
        QueryErrorKind::Constraint => "ConstraintError",
        _ => "ExecutionError",
    };
    ObservedError {
        category: category.to_owned(),
        line: error.line,
        column: error.column,
        message: error.message.clone(),
    }
}
