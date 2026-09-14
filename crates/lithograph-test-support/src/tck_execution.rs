use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, prepare,
};
use lithograph_core::storage::{
    Snapshot, active_branch, branch_head, create_storage_schema, initialize_connection_state,
    initialize_root, label_name,
};
use rusqlite::Connection;
use serde::Serialize;

use crate::tck::{TckScenarioInstance, TckStep, TckStepArgument, expand_scenarios, read_feature};
use crate::tck_value::{
    ExpectedValue, parse_expected_value, parse_parameter_value, value_matches_expected,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckExecutionReport {
    pub total: usize,
    pub applicable: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_applicable: usize,
    pub not_applicable_reasons: BTreeMap<String, usize>,
    pub exclusions: Vec<TckExecutionExclusion>,
    pub failures: Vec<TckExecutionFailure>,
}

impl TckExecutionReport {
    pub fn is_success(&self) -> bool {
        self.failed == 0 && self.passed == self.applicable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckExecutionFailure {
    pub feature: String,
    pub scenario: String,
    pub line: usize,
    pub examples_line: Option<usize>,
    pub example_row: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckExecutionExclusion {
    pub feature: String,
    pub scenario: String,
    pub line: usize,
    pub examples_line: Option<usize>,
    pub example_row: Option<usize>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorStage {
    Compile,
    Runtime,
}

#[derive(Debug)]
enum QueryAttempt {
    Success {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        summary: Box<QuerySummary>,
        labels_added: u64,
        labels_removed: u64,
    },
    Error {
        stage: ErrorStage,
        error: QueryError,
    },
}

struct ScenarioState {
    connection: Connection,
    parameters: BTreeMap<String, Value>,
    last_attempt: Option<QueryAttempt>,
}

pub fn execute_vendored_tck(repo_root: &Path) -> Result<TckExecutionReport, String> {
    let features_root = repo_root.join("vendor/opencypher-tck/features");
    let mut paths = Vec::new();
    collect_features(&features_root, &mut paths).map_err(|error| error.to_string())?;
    paths.sort();

    let mut report = TckExecutionReport {
        total: 0,
        applicable: 0,
        passed: 0,
        failed: 0,
        not_applicable: 0,
        not_applicable_reasons: BTreeMap::new(),
        exclusions: Vec::new(),
        failures: Vec::new(),
    };
    for path in paths {
        let feature = read_feature(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let scenarios = expand_scenarios(&feature)
            .map_err(|error| format!("failed to expand {}: {error}", path.display()))?;
        let relative = path
            .strip_prefix(&features_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        for scenario in scenarios {
            report.total += 1;
            if let Some(reason) = not_applicable_reason(&relative, &scenario) {
                report.not_applicable += 1;
                *report
                    .not_applicable_reasons
                    .entry(reason.to_owned())
                    .or_default() += 1;
                report.exclusions.push(TckExecutionExclusion {
                    feature: relative.clone(),
                    scenario: scenario.name.clone(),
                    line: scenario.line,
                    examples_line: scenario.examples_line,
                    example_row: scenario.example_row,
                    reason: reason.to_owned(),
                });
                continue;
            }
            report.applicable += 1;
            match execute_scenario(repo_root, &scenario) {
                Ok(()) => report.passed += 1,
                Err(message) => {
                    report.failed += 1;
                    report.failures.push(TckExecutionFailure {
                        feature: relative.clone(),
                        scenario: scenario.name.clone(),
                        line: scenario.line,
                        examples_line: scenario.examples_line,
                        example_row: scenario.example_row,
                        message,
                    });
                }
            }
        }
    }
    Ok(report)
}

fn not_applicable_reason(feature: &str, scenario: &TckScenarioInstance) -> Option<&'static str> {
    if scenario
        .steps
        .iter()
        .any(|step| step.text.starts_with("there exists a procedure "))
    {
        return Some("custom-procedure-registration-outside-product-surface");
    }
    if feature == "clauses/match/Match3.feature" && scenario_number(scenario) == Some(29) {
        return Some("superseded-by-cypher25-match-mode-semantics");
    }
    if is_incomparable_equality_supersession(feature, scenario) {
        return Some("superseded-by-cypher25-incomparable-equality-semantics");
    }
    if is_cross_type_ordering_supersession(feature, scenario) {
        return Some("superseded-by-cypher25-cross-type-ordering-hierarchy");
    }
    if is_exponentiation_supersession(feature, scenario) {
        return Some("superseded-by-cypher25-right-associative-exponentiation");
    }
    if matches!(
        (feature, scenario_number(scenario)),
        ("clauses/merge/Merge6.feature", Some(6)) | ("clauses/merge/Merge7.feature", Some(4))
    ) {
        return Some("removed-in-cypher25-node-relationship-rhs-set-properties");
    }
    if matches!(
        (feature, scenario_number(scenario)),
        ("clauses/with/With2.feature", Some(1))
            | ("clauses/with-skip-limit/WithSkipLimit1.feature", Some(1))
            | ("clauses/with-skip-limit/WithSkipLimit2.feature", Some(2))
    ) {
        return Some("removed-in-cypher25-same-create-property-reference");
    }
    None
}

fn scenario_number(scenario: &TckScenarioInstance) -> Option<usize> {
    scenario
        .name
        .strip_prefix('[')?
        .split_once(']')?
        .0
        .parse()
        .ok()
}

fn example_is(scenario: &TckScenarioInstance, examples_line: usize, rows: &[usize]) -> bool {
    scenario.examples_line == Some(examples_line)
        && scenario.example_row.is_some_and(|row| rows.contains(&row))
}

fn is_incomparable_equality_supersession(feature: &str, scenario: &TckScenarioInstance) -> bool {
    let number = scenario_number(scenario);
    match feature {
        "clauses/match/Match4.feature" => number == Some(4),
        "expressions/comparison/Comparison1.feature" => match number {
            Some(3) => true,
            Some(6) => example_is(scenario, 137, &[3]),
            Some(8) => example_is(scenario, 187, &[4]),
            Some(9) => example_is(scenario, 205, &[3, 4]),
            _ => false,
        },
        "expressions/conditional/Conditional2.feature" => {
            number == Some(1) && example_is(scenario, 52, &[10, 11])
        }
        "expressions/list/List3.feature" => matches!(number, Some(1 | 3 | 6)),
        "expressions/list/List5.feature" => matches!(
            number,
            Some(
                5 | 6
                    | 7
                    | 8
                    | 9
                    | 10
                    | 11
                    | 12
                    | 13
                    | 14
                    | 15
                    | 16
                    | 17
                    | 18
                    | 19
                    | 26
                    | 27
                    | 28
                    | 30
                    | 31
                    | 33
                    | 37
                    | 38
                    | 39
                    | 40
                    | 41
            )
        ),
        "expressions/precedence/Precedence3.feature" => match number {
            Some(4 | 5) => true,
            Some(6) => example_is(scenario, 112, &[1, 2, 3, 4, 5, 6]),
            _ => false,
        },
        "expressions/temporal/Temporal7.feature" => {
            number == Some(6) && example_is(scenario, 130, &[1, 2, 3, 4, 5])
        }
        _ => false,
    }
}

fn is_cross_type_ordering_supersession(feature: &str, scenario: &TckScenarioInstance) -> bool {
    if feature != "expressions/comparison/Comparison2.feature" {
        return false;
    }
    match scenario_number(scenario) {
        Some(1 | 2) => true,
        Some(3) => example_is(scenario, 95, &[1, 2, 3, 4]),
        Some(5) => example_is(scenario, 132, &[4]),
        Some(6) => example_is(scenario, 150, &[3, 4]),
        _ => false,
    }
}

fn is_exponentiation_supersession(feature: &str, scenario: &TckScenarioInstance) -> bool {
    if feature != "expressions/precedence/Precedence2.feature" {
        return false;
    }
    match scenario_number(scenario) {
        Some(2) => example_is(scenario, 80, &[1, 2, 3]),
        Some(3) => example_is(scenario, 99, &[1, 2]),
        _ => false,
    }
}

fn execute_scenario(repo_root: &Path, scenario: &TckScenarioInstance) -> Result<(), String> {
    let mut state = ScenarioState {
        connection: fresh_storage()?,
        parameters: BTreeMap::new(),
        last_attempt: None,
    };
    for step in &scenario.steps {
        execute_step(repo_root, &mut state, step)
            .map_err(|message| format!("step line {} {:?}: {message}", step.line, step.text))?;
    }
    Ok(())
}

fn execute_step(repo_root: &Path, state: &mut ScenarioState, step: &TckStep) -> Result<(), String> {
    match step.text.as_str() {
        "an empty graph" | "any graph" => Ok(()),
        "parameters are:" => load_parameters(state, step),
        "having executed:" => execute_setup(state, step),
        "executing query:" | "executing control query:" => {
            let query = step_doc_string(step)?;
            state.last_attempt = Some(execute_query(&state.connection, query, &state.parameters));
            Ok(())
        }
        "the result should be empty" => assert_empty_result(state),
        "the result should be, in any order:" => assert_table_result(state, step, false, true),
        "the result should be, in order:" => assert_table_result(state, step, false, false),
        "the result should be (ignoring element order for lists):" => {
            assert_table_result(state, step, true, true)
        }
        "the result should be, in order (ignoring element order for lists):" => {
            assert_table_result(state, step, true, false)
        }
        "no side effects" => assert_no_side_effects(state),
        "the side effects should be:" => assert_side_effects(state, step),
        text if text.starts_with("the binary-tree-") && text.ends_with(" graph") => {
            load_named_graph(repo_root, state, text)
        }
        text if text.contains(" should be raised at ") => assert_expected_error(state, text),
        other => Err(format!("unsupported applicable TCK step {other:?}")),
    }
}

fn fresh_storage() -> Result<Connection, String> {
    let connection = Connection::open_in_memory().map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            r#"CREATE TABLE main._lithograph_meta(
                 id INTEGER PRIMARY KEY CHECK(id=1),
                 magic TEXT NOT NULL,
                 database_id TEXT NOT NULL,
                 storage_format INTEGER NOT NULL
             );
             INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000010', 2);"#,
        )
        .map_err(|error| error.to_string())?;
    create_storage_schema(&connection).map_err(|error| error.to_string())?;
    initialize_root(&connection).map_err(|error| error.to_string())?;
    initialize_connection_state(&connection).map_err(|error| error.to_string())?;
    Ok(connection)
}

fn execute_query(
    connection: &Connection,
    query: &str,
    parameters: &BTreeMap<String, Value>,
) -> QueryAttempt {
    let prepared = match prepare(
        connection,
        query,
        parameters.clone(),
        ExecutionOptions::default(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return QueryAttempt::Error {
                stage: prepare_error_stage(query, parameters, &error),
                error,
            };
        }
    };
    let labels_before = match visible_label_names(connection) {
        Ok(labels) => labels,
        Err(message) => {
            return QueryAttempt::Error {
                stage: ErrorStage::Runtime,
                error: QueryError::internal(message),
            };
        }
    };
    let columns = prepared.columns.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = match cursor.next_batch(connection, 256) {
            Ok(batch) => batch,
            Err(error) => {
                return QueryAttempt::Error {
                    stage: ErrorStage::Runtime,
                    error,
                };
            }
        };
        rows.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    match cursor.complete(connection) {
        Ok(summary) => match visible_label_names(connection) {
            Ok(labels_after) => QueryAttempt::Success {
                columns,
                rows,
                summary: Box::new(summary),
                labels_added: labels_after.difference(&labels_before).count() as u64,
                labels_removed: labels_before.difference(&labels_after).count() as u64,
            },
            Err(message) => QueryAttempt::Error {
                stage: ErrorStage::Runtime,
                error: QueryError::internal(message),
            },
        },
        Err(error) => QueryAttempt::Error {
            stage: ErrorStage::Runtime,
            error,
        },
    }
}

fn prepare_error_stage(
    query: &str,
    parameters: &BTreeMap<String, Value>,
    error: &QueryError,
) -> ErrorStage {
    let upper = query.to_ascii_uppercase();
    if !parameters.is_empty()
        && matches!(
            error.kind,
            QueryErrorKind::Type | QueryErrorKind::InvalidArgument
        )
        && ((upper.contains("SKIP $") && error.message.starts_with("SKIP"))
            || (upper.contains("LIMIT $") && error.message.starts_with("LIMIT")))
    {
        ErrorStage::Runtime
    } else {
        ErrorStage::Compile
    }
}

fn visible_label_names(connection: &Connection) -> Result<BTreeSet<String>, String> {
    let branch = active_branch(connection).map_err(|error| error.to_string())?;
    let commit = branch_head(connection, &branch).map_err(|error| error.to_string())?;
    let snapshot = Snapshot::resolve(connection, commit).map_err(|error| error.to_string())?;
    let mut label_ids = BTreeSet::new();
    snapshot
        .visit_labels(|_, label_id| {
            label_ids.insert(label_id);
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    label_ids
        .into_iter()
        .map(|label_id| {
            label_name(connection, label_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("visible label id {label_id} has no dictionary name"))
        })
        .collect()
}

fn execute_setup(state: &ScenarioState, step: &TckStep) -> Result<(), String> {
    let query = step_doc_string(step)?;
    match execute_query(&state.connection, query, &state.parameters) {
        QueryAttempt::Success { .. } => Ok(()),
        QueryAttempt::Error { stage, error } => Err(format!(
            "setup query failed at {stage:?} with {:?}: {}; query={query:?}",
            error.kind, error.message
        )),
    }
}

fn load_named_graph(repo_root: &Path, state: &ScenarioState, text: &str) -> Result<(), String> {
    let graph_name = text
        .strip_prefix("the ")
        .and_then(|value| value.strip_suffix(" graph"))
        .ok_or_else(|| format!("invalid graph fixture step {text:?}"))?;
    let path = repo_root
        .join("vendor/opencypher-tck/graphs")
        .join(graph_name)
        .join(format!("{graph_name}.cypher"));
    let query = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    match execute_query(&state.connection, &query, &state.parameters) {
        QueryAttempt::Success { .. } => Ok(()),
        QueryAttempt::Error { stage, error } => Err(format!(
            "graph fixture failed at {stage:?} with {:?}: {}",
            error.kind, error.message
        )),
    }
}

fn load_parameters(state: &mut ScenarioState, step: &TckStep) -> Result<(), String> {
    for row in step_table(step)? {
        if row.len() != 2 {
            return Err(format!(
                "parameter row must contain name/value, got {row:?}"
            ));
        }
        state.parameters.insert(
            row[0].clone(),
            parse_parameter_value(&row[1])
                .map_err(|error| format!("parameter {}: {error}", row[0]))?,
        );
    }
    Ok(())
}

fn assert_empty_result(state: &ScenarioState) -> Result<(), String> {
    match last_attempt(state)? {
        QueryAttempt::Success { rows, .. } if rows.is_empty() => Ok(()),
        QueryAttempt::Success { rows, .. } => Err(format!(
            "expected empty result, got {} row(s): {rows:?}",
            rows.len()
        )),
        attempt => Err(format_attempt_error(attempt)),
    }
}

fn assert_table_result(
    state: &ScenarioState,
    step: &TckStep,
    ignore_list_order: bool,
    any_row_order: bool,
) -> Result<(), String> {
    let QueryAttempt::Success { columns, rows, .. } = last_attempt(state)? else {
        return Err(format_attempt_error(last_attempt(state)?));
    };
    let table = step_table(step)?;
    let Some(expected_columns) = table.first() else {
        return Err("result assertion table is empty".to_owned());
    };
    if columns != expected_columns {
        return Err(format!(
            "result columns differ: expected {expected_columns:?}, got {columns:?}"
        ));
    }
    let expected_rows = table[1..]
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| parse_expected_value(cell))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() != expected_rows.len() {
        return Err(format!(
            "result row count differs: expected {}, got {}; expected={expected_rows:?}; actual={rows:?}",
            expected_rows.len(),
            rows.len()
        ));
    }
    if any_row_order {
        let mut used = vec![false; rows.len()];
        for expected in &expected_rows {
            let Some(index) = rows.iter().enumerate().find_map(|(index, actual)| {
                (!used[index] && row_matches(actual, expected, ignore_list_order)).then_some(index)
            }) else {
                return Err(format!(
                    "expected row not found: {expected:?}; actual rows={rows:?}"
                ));
            };
            used[index] = true;
        }
        return Ok(());
    }
    for (index, (actual, expected)) in rows.iter().zip(&expected_rows).enumerate() {
        if !row_matches(actual, expected, ignore_list_order) {
            return Err(format!(
                "ordered result row {index} differs: expected {expected:?}, got {actual:?}"
            ));
        }
    }
    Ok(())
}

fn row_matches(actual: &[Value], expected: &[ExpectedValue], ignore_list_order: bool) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| value_matches_expected(actual, expected, ignore_list_order))
}

fn assert_no_side_effects(state: &ScenarioState) -> Result<(), String> {
    let QueryAttempt::Success {
        summary,
        labels_added,
        labels_removed,
        ..
    } = last_attempt(state)?
    else {
        return Err(format_attempt_error(last_attempt(state)?));
    };
    if summary.counters.nodes_created == 0
        && summary.counters.nodes_deleted == 0
        && summary.counters.relationships_created == 0
        && summary.counters.relationships_deleted == 0
        && summary.counters.properties_set == 0
        && summary.counters.properties_removed == 0
        && *labels_added == 0
        && *labels_removed == 0
    {
        Ok(())
    } else {
        Err(format!(
            "expected no side effects, got counters={:?}, labelTokensAdded={labels_added}, labelTokensRemoved={labels_removed}",
            summary.counters,
        ))
    }
}

fn assert_side_effects(state: &ScenarioState, step: &TckStep) -> Result<(), String> {
    let QueryAttempt::Success {
        summary,
        labels_added,
        labels_removed,
        ..
    } = last_attempt(state)?
    else {
        return Err(format_attempt_error(last_attempt(state)?));
    };
    let mut expected = BTreeMap::new();
    for row in step_table(step)? {
        if row.len() != 2 {
            return Err(format!("side effect row must have name/count, got {row:?}"));
        }
        expected.insert(
            row[0].as_str(),
            row[1]
                .parse::<u64>()
                .map_err(|_| format!("invalid side effect count {:?}", row[1]))?,
        );
    }
    let counters = &summary.counters;
    let actual = BTreeMap::from([
        ("+nodes", counters.nodes_created),
        ("-nodes", counters.nodes_deleted),
        ("+relationships", counters.relationships_created),
        ("-relationships", counters.relationships_deleted),
        ("+properties", counters.properties_set),
        ("-properties", counters.properties_removed),
        ("+labels", *labels_added),
        ("-labels", *labels_removed),
    ]);
    for (name, actual_count) in actual {
        let expected_count = expected.remove(name).unwrap_or(0);
        if actual_count != expected_count {
            return Err(format!(
                "side effect {name} differs: expected {expected_count}, got {actual_count}; counters={counters:?}"
            ));
        }
    }
    if expected.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown expected side effect(s): {expected:?}"))
    }
}

fn assert_expected_error(state: &ScenarioState, text: &str) -> Result<(), String> {
    let QueryAttempt::Error { stage, error } = last_attempt(state)? else {
        return Err("expected query to fail, but it succeeded".to_owned());
    };
    let (category, timing) = text
        .split_once(" should be raised at ")
        .ok_or_else(|| format!("invalid error expectation {text:?}"))?;
    let timing = timing
        .split_once(':')
        .map_or(timing, |(timing, _)| timing)
        .trim();
    let expected_stage = match timing {
        "compile time" => Some(ErrorStage::Compile),
        "runtime" => Some(ErrorStage::Runtime),
        "any time" => None,
        other => return Err(format!("unknown TCK error timing {other:?}")),
    };
    if expected_stage.is_some_and(|expected| expected != *stage) {
        return Err(format!(
            "error stage differs: expected {timing}, got {stage:?} ({:?}: {})",
            error.kind, error.message
        ));
    }
    let category_matches = match category.trim() {
        "a SyntaxError" => matches!(
            error.kind,
            QueryErrorKind::Parse
                | QueryErrorKind::Semantic
                | QueryErrorKind::Type
                | QueryErrorKind::Schema
                | QueryErrorKind::InvalidArgument
        ),
        "a TypeError" => matches!(
            error.kind,
            QueryErrorKind::Type | QueryErrorKind::InvalidArgument
        ),
        "a ArgumentError" => matches!(
            error.kind,
            QueryErrorKind::InvalidArgument | QueryErrorKind::Type
        ),
        "a EntityNotFound" => matches!(
            error.kind,
            QueryErrorKind::Semantic | QueryErrorKind::InvalidArgument
        ),
        "a SemanticError" => matches!(
            error.kind,
            QueryErrorKind::Semantic | QueryErrorKind::Constraint
        ),
        "a ProcedureError" => matches!(
            error.kind,
            QueryErrorKind::Semantic | QueryErrorKind::InvalidArgument
        ),
        "a ParameterMissing" => matches!(
            error.kind,
            QueryErrorKind::Semantic | QueryErrorKind::InvalidArgument
        ),
        "a ConstraintVerificationFailed" => error.kind == QueryErrorKind::Constraint,
        other => return Err(format!("unknown TCK error category {other:?}")),
    };
    if category_matches {
        Ok(())
    } else {
        Err(format!(
            "error category differs: expected {category}, got {:?}: {}",
            error.kind, error.message
        ))
    }
}

fn last_attempt(state: &ScenarioState) -> Result<&QueryAttempt, String> {
    state
        .last_attempt
        .as_ref()
        .ok_or_else(|| "assertion appeared before an executing query step".to_owned())
}

fn format_attempt_error(attempt: &QueryAttempt) -> String {
    match attempt {
        QueryAttempt::Success { .. } => "query unexpectedly succeeded".to_owned(),
        QueryAttempt::Error { stage, error } => format!(
            "query failed at {stage:?} with {:?}: {}",
            error.kind, error.message
        ),
    }
}

fn step_doc_string(step: &TckStep) -> Result<&str, String> {
    match &step.argument {
        Some(TckStepArgument::DocString { value }) => Ok(value),
        other => Err(format!("expected DocString argument, got {other:?}")),
    }
}

fn step_table(step: &TckStep) -> Result<&[Vec<String>], String> {
    match &step.argument {
        Some(TckStepArgument::Table { rows }) => Ok(rows),
        other => Err(format!("expected DataTable argument, got {other:?}")),
    }
}

fn collect_features(root: &Path, output: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_features(&path, output)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("feature") {
            output.push(path);
        }
    }
    Ok(())
}
