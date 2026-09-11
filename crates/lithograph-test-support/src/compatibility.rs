use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum HarnessError {
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidFixture(String),
}

impl Display for HarnessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::InvalidFixture(message) => write!(formatter, "invalid fixture: {message}"),
        }
    }
}

impl Error for HarnessError {}

impl From<std::io::Error> for HarnessError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for HarnessError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypedValue {
    Null,
    Boolean { value: bool },
    Integer { value: i64 },
    Float { value: String },
    String { value: String },
    List { value: Vec<TypedValue> },
    Map { value: BTreeMap<String, TypedValue> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileMetadata {
    pub id: String,
    pub source: String,
    pub version: String,
    pub family: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureExecution {
    Planned,
    Enabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedError {
    pub category: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub column: Option<u32>,
    #[serde(default)]
    pub message_contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpectedOutcome {
    Parse {
        valid: bool,
        #[serde(default)]
        error: Option<ExpectedError>,
    },
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<TypedValue>>,
        ordered: bool,
    },
    Error {
        error: ExpectedError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixture {
    pub schema_version: u32,
    pub id: String,
    pub profile: ProfileMetadata,
    pub execution: FixtureExecution,
    pub query: String,
    #[serde(default)]
    pub params: BTreeMap<String, TypedValue>,
    #[serde(default)]
    pub initial_graph: Vec<String>,
    pub expected: ExpectedOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedError {
    pub category: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedOutcome {
    Parse {
        valid: bool,
        error: Option<ObservedError>,
    },
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<TypedValue>>,
    },
    Error {
        error: ObservedError,
    },
}

pub trait FixtureExecutor {
    fn execute(&mut self, fixture: &Fixture) -> std::result::Result<ObservedOutcome, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureStatus {
    Passed,
    Failed,
    Planned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureResult {
    pub id: String,
    pub profile: String,
    pub family: String,
    pub status: FixtureStatus,
    pub differences: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatibilityReport {
    pub profile: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub planned: usize,
    pub results: Vec<FixtureResult>,
}

impl CompatibilityReport {
    pub fn from_results(profile: impl Into<String>, results: Vec<FixtureResult>) -> Self {
        let passed = results
            .iter()
            .filter(|result| result.status == FixtureStatus::Passed)
            .count();
        let failed = results
            .iter()
            .filter(|result| result.status == FixtureStatus::Failed)
            .count();
        let planned = results
            .iter()
            .filter(|result| result.status == FixtureStatus::Planned)
            .count();
        Self {
            profile: profile.into(),
            total: results.len(),
            passed,
            failed,
            planned,
            results,
        }
    }
}

pub fn load_fixture(path: &Path) -> Result<Fixture, HarnessError> {
    let fixture: Fixture = serde_json::from_slice(&fs::read(path)?)?;
    validate_fixture(&fixture)?;
    Ok(fixture)
}

pub fn load_fixture_directory(path: &Path) -> Result<Vec<Fixture>, HarnessError> {
    let mut paths = Vec::new();
    collect_json_paths(path, &mut paths)?;
    paths.sort();
    let fixtures = paths
        .iter()
        .map(|path| load_fixture(path))
        .collect::<Result<Vec<_>, _>>()?;
    validate_fixture_set(&fixtures)?;
    Ok(fixtures)
}

pub fn inventory_report(fixtures: &[Fixture]) -> Result<CompatibilityReport, HarnessError> {
    validate_fixture_set(fixtures)?;
    let profile = fixtures[0].profile.id.clone();
    let results = fixtures
        .iter()
        .map(|fixture| FixtureResult {
            id: fixture.id.clone(),
            profile: fixture.profile.id.clone(),
            family: fixture.profile.family.clone(),
            status: FixtureStatus::Planned,
            differences: Vec::new(),
        })
        .collect();
    Ok(CompatibilityReport::from_results(profile, results))
}

pub fn run_fixture<E: FixtureExecutor>(fixture: &Fixture, executor: &mut E) -> FixtureResult {
    if fixture.execution == FixtureExecution::Planned {
        return result_for(fixture, FixtureStatus::Planned, Vec::new());
    }

    match executor.execute(fixture) {
        Ok(observed) => {
            let differences = compare_outcome(&fixture.expected, &observed);
            if differences.is_empty() {
                result_for(fixture, FixtureStatus::Passed, differences)
            } else {
                result_for(fixture, FixtureStatus::Failed, differences)
            }
        }
        Err(message) => result_for(fixture, FixtureStatus::Failed, vec![message]),
    }
}

fn result_for(fixture: &Fixture, status: FixtureStatus, differences: Vec<String>) -> FixtureResult {
    FixtureResult {
        id: fixture.id.clone(),
        profile: fixture.profile.id.clone(),
        family: fixture.profile.family.clone(),
        status,
        differences,
    }
}

fn validate_fixture(fixture: &Fixture) -> Result<(), HarnessError> {
    if fixture.schema_version != 1 {
        return Err(HarnessError::InvalidFixture(format!(
            "{} has unsupported schema_version {}",
            fixture.id, fixture.schema_version
        )));
    }
    if fixture.id.trim().is_empty() {
        return Err(HarnessError::InvalidFixture("fixture id is empty".into()));
    }
    if fixture.profile.id.trim().is_empty()
        || fixture.profile.source.trim().is_empty()
        || fixture.profile.version.trim().is_empty()
        || fixture.profile.family.trim().is_empty()
    {
        return Err(HarnessError::InvalidFixture(format!(
            "{} has incomplete profile metadata",
            fixture.id
        )));
    }
    if fixture.query.trim().is_empty() {
        return Err(HarnessError::InvalidFixture(format!(
            "{} has an empty query",
            fixture.id
        )));
    }

    match &fixture.expected {
        ExpectedOutcome::Parse { valid, error } => match (*valid, error) {
            (true, Some(_)) => {
                return Err(HarnessError::InvalidFixture(format!(
                    "{} expects a valid parse but also declares an error",
                    fixture.id
                )));
            }
            (false, None) => {
                return Err(HarnessError::InvalidFixture(format!(
                    "{} expects an invalid parse but declares no error",
                    fixture.id
                )));
            }
            (_, Some(error)) => validate_expected_error(&fixture.id, error)?,
            (_, None) => {}
        },
        ExpectedOutcome::Rows { columns, rows, .. } => {
            for (row_index, row) in rows.iter().enumerate() {
                if row.len() != columns.len() {
                    return Err(HarnessError::InvalidFixture(format!(
                        "{} row {} has {} values for {} columns",
                        fixture.id,
                        row_index + 1,
                        row.len(),
                        columns.len()
                    )));
                }
            }
        }
        ExpectedOutcome::Error { error } => validate_expected_error(&fixture.id, error)?,
    }
    Ok(())
}

fn validate_fixture_set(fixtures: &[Fixture]) -> Result<(), HarnessError> {
    let Some(first) = fixtures.first() else {
        return Err(HarnessError::InvalidFixture(
            "fixture directory contains no JSON fixtures".into(),
        ));
    };

    let expected_profile = &first.profile.id;
    let mut ids = BTreeSet::new();
    for fixture in fixtures {
        if !ids.insert(fixture.id.as_str()) {
            return Err(HarnessError::InvalidFixture(format!(
                "duplicate fixture id: {}",
                fixture.id
            )));
        }
        if fixture.profile.id != *expected_profile {
            return Err(HarnessError::InvalidFixture(format!(
                "mixed compatibility profiles: expected {}, found {} in {}",
                expected_profile, fixture.profile.id, fixture.id
            )));
        }
    }
    Ok(())
}

fn validate_expected_error(fixture_id: &str, error: &ExpectedError) -> Result<(), HarnessError> {
    if error.category.trim().is_empty() {
        return Err(HarnessError::InvalidFixture(format!(
            "{fixture_id} has an empty error category"
        )));
    }
    if error.line == Some(0) {
        return Err(HarnessError::InvalidFixture(format!(
            "{fixture_id} has invalid error line 0"
        )));
    }
    if error.column == Some(0) {
        return Err(HarnessError::InvalidFixture(format!(
            "{fixture_id} has invalid error column 0"
        )));
    }
    if error
        .message_contains
        .as_ref()
        .is_some_and(|text| text.is_empty())
    {
        return Err(HarnessError::InvalidFixture(format!(
            "{fixture_id} has an empty error message fragment"
        )));
    }
    Ok(())
}

fn collect_json_paths(path: &Path, paths: &mut Vec<PathBuf>) -> Result<(), HarnessError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            collect_json_paths(&entry_path, paths)?;
        } else if entry_path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            paths.push(entry_path);
        }
    }
    Ok(())
}

fn compare_outcome(expected: &ExpectedOutcome, observed: &ObservedOutcome) -> Vec<String> {
    match (expected, observed) {
        (
            ExpectedOutcome::Parse {
                valid: expected_valid,
                error: expected_error,
            },
            ObservedOutcome::Parse {
                valid: actual_valid,
                error: actual_error,
            },
        ) => {
            let mut differences = Vec::new();
            if expected_valid != actual_valid {
                differences.push(format!(
                    "parse validity differs: expected {expected_valid}, got {actual_valid}"
                ));
            }
            differences.extend(compare_optional_error(
                expected_error.as_ref(),
                actual_error.as_ref(),
            ));
            differences
        }
        (
            ExpectedOutcome::Rows {
                columns: expected_columns,
                rows: expected_rows,
                ordered,
            },
            ObservedOutcome::Rows {
                columns: actual_columns,
                rows: actual_rows,
            },
        ) => compare_rows(
            expected_columns,
            expected_rows,
            *ordered,
            actual_columns,
            actual_rows,
        ),
        (ExpectedOutcome::Error { error: expected }, ObservedOutcome::Error { error: actual }) => {
            compare_error(expected, actual)
        }
        (expected, actual) => vec![format!(
            "outcome kind differs: expected {}, got {}",
            outcome_kind_expected(expected),
            outcome_kind_actual(actual)
        )],
    }
}

fn compare_rows(
    expected_columns: &[String],
    expected_rows: &[Vec<TypedValue>],
    ordered: bool,
    actual_columns: &[String],
    actual_rows: &[Vec<TypedValue>],
) -> Vec<String> {
    let mut differences = Vec::new();
    if expected_columns != actual_columns {
        differences.push(format!(
            "columns differ: expected {expected_columns:?}, got {actual_columns:?}"
        ));
    }

    if ordered {
        if expected_rows != actual_rows {
            differences.push(format!(
                "ordered rows differ: expected {expected_rows:?}, got {actual_rows:?}"
            ));
        }
    } else {
        let expected_counts = row_counts(expected_rows);
        let actual_counts = row_counts(actual_rows);
        if expected_counts != actual_counts {
            differences.push(format!(
                "unordered row multiplicities differ: expected {expected_counts:?}, got {actual_counts:?}"
            ));
        }
    }
    differences
}

fn row_counts(rows: &[Vec<TypedValue>]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for row in rows {
        let key = serde_json::to_string(row).expect("typed rows must serialize");
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

fn compare_optional_error(
    expected: Option<&ExpectedError>,
    actual: Option<&ObservedError>,
) -> Vec<String> {
    match (expected, actual) {
        (None, None) => Vec::new(),
        (Some(expected), Some(actual)) => compare_error(expected, actual),
        (None, Some(actual)) => vec![format!("unexpected error: {}", actual.category)],
        (Some(expected), None) => vec![format!("missing expected error: {}", expected.category)],
    }
}

fn compare_error(expected: &ExpectedError, actual: &ObservedError) -> Vec<String> {
    let mut differences = Vec::new();
    if expected.category != actual.category {
        differences.push(format!(
            "error category differs: expected {}, got {}",
            expected.category, actual.category
        ));
    }
    if expected.line.is_some() && expected.line != actual.line {
        differences.push(format!(
            "error line differs: expected {:?}, got {:?}",
            expected.line, actual.line
        ));
    }
    if expected.column.is_some() && expected.column != actual.column {
        differences.push(format!(
            "error column differs: expected {:?}, got {:?}",
            expected.column, actual.column
        ));
    }
    if let Some(fragment) = &expected.message_contains
        && !actual.message.contains(fragment)
    {
        differences.push(format!(
            "error message does not contain expected fragment {fragment:?}: {:?}",
            actual.message
        ));
    }
    differences
}

fn outcome_kind_expected(outcome: &ExpectedOutcome) -> &'static str {
    match outcome {
        ExpectedOutcome::Parse { .. } => "parse",
        ExpectedOutcome::Rows { .. } => "rows",
        ExpectedOutcome::Error { .. } => "error",
    }
}

fn outcome_kind_actual(outcome: &ObservedOutcome) -> &'static str {
    match outcome {
        ObservedOutcome::Parse { .. } => "parse",
        ObservedOutcome::Rows { .. } => "rows",
        ObservedOutcome::Error { .. } => "error",
    }
}

pub fn self_check_report() -> CompatibilityReport {
    struct SelfCheckExecutor {
        outcomes: HashMap<String, ObservedOutcome>,
    }

    impl FixtureExecutor for SelfCheckExecutor {
        fn execute(&mut self, fixture: &Fixture) -> std::result::Result<ObservedOutcome, String> {
            self.outcomes
                .get(&fixture.id)
                .cloned()
                .ok_or_else(|| format!("no scripted outcome for {}", fixture.id))
        }
    }

    let expected_rows = ExpectedOutcome::Rows {
        columns: vec!["value".into()],
        rows: vec![vec![TypedValue::Integer { value: 1 }]],
        ordered: true,
    };
    let make_fixture = |id: &str| Fixture {
        schema_version: 1,
        id: id.into(),
        profile: ProfileMetadata {
            id: "HARNESS-SELF-CHECK".into(),
            source: "Lithograph test-support".into(),
            version: "1".into(),
            family: "harness".into(),
        },
        execution: FixtureExecution::Enabled,
        query: format!("SELF_CHECK {id}"),
        params: BTreeMap::new(),
        initial_graph: Vec::new(),
        expected: expected_rows.clone(),
    };
    let fixtures = [
        make_fixture("intentional-pass"),
        make_fixture("intentional-fail"),
    ];

    let mut executor = SelfCheckExecutor {
        outcomes: HashMap::from([
            (
                "intentional-pass".into(),
                ObservedOutcome::Rows {
                    columns: vec!["value".into()],
                    rows: vec![vec![TypedValue::Integer { value: 1 }]],
                },
            ),
            (
                "intentional-fail".into(),
                ObservedOutcome::Rows {
                    columns: vec!["value".into()],
                    rows: vec![vec![TypedValue::Integer { value: 2 }]],
                },
            ),
        ]),
    };
    let results = fixtures
        .iter()
        .map(|fixture| run_fixture(fixture, &mut executor))
        .collect();
    CompatibilityReport::from_results("HARNESS-SELF-CHECK", results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str, profile: &str, expected: ExpectedOutcome) -> Fixture {
        Fixture {
            schema_version: 1,
            id: id.into(),
            profile: ProfileMetadata {
                id: profile.into(),
                source: "test-source".into(),
                version: "test-version".into(),
                family: "test-family".into(),
            },
            execution: FixtureExecution::Planned,
            query: "RETURN 1".into(),
            params: BTreeMap::new(),
            initial_graph: Vec::new(),
            expected,
        }
    }

    #[test]
    fn self_check_reports_one_pass_and_one_intentional_failure() {
        let report = self_check_report();
        assert_eq!(report.total, 2);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.planned, 0);
        assert!(!report.results[1].differences.is_empty());
    }

    #[test]
    fn unordered_rows_preserve_multiplicity_and_type() {
        let columns = vec!["x".into()];
        let expected = vec![
            vec![TypedValue::Integer { value: 1 }],
            vec![TypedValue::Integer { value: 1 }],
            vec![TypedValue::String { value: "1".into() }],
        ];
        let actual = vec![
            vec![TypedValue::String { value: "1".into() }],
            vec![TypedValue::Integer { value: 1 }],
            vec![TypedValue::Integer { value: 1 }],
        ];
        assert!(compare_rows(&columns, &expected, false, &columns, &actual).is_empty());

        let missing_duplicate = vec![
            vec![TypedValue::String { value: "1".into() }],
            vec![TypedValue::Integer { value: 1 }],
        ];
        assert!(!compare_rows(&columns, &expected, false, &columns, &missing_duplicate).is_empty());
    }

    #[test]
    fn error_comparison_checks_location_and_category() {
        let expected = ExpectedError {
            category: "SyntaxError".into(),
            line: Some(2),
            column: Some(4),
            message_contains: Some("RETURN".into()),
        };
        let actual = ObservedError {
            category: "SyntaxError".into(),
            line: Some(2),
            column: Some(4),
            message: "unexpected RETURN".into(),
        };
        assert!(compare_error(&expected, &actual).is_empty());
    }

    #[test]
    fn fixture_validation_rejects_row_width_mismatch() {
        let fixture = fixture(
            "bad-row",
            "PROFILE",
            ExpectedOutcome::Rows {
                columns: vec!["a".into(), "b".into()],
                rows: vec![vec![TypedValue::Integer { value: 1 }]],
                ordered: true,
            },
        );
        let error = validate_fixture(&fixture).expect_err("row width mismatch must be rejected");
        assert!(error.to_string().contains("1 values for 2 columns"));
    }

    #[test]
    fn fixture_validation_rejects_inconsistent_parse_expectation() {
        let invalid_without_error = fixture(
            "invalid-without-error",
            "PROFILE",
            ExpectedOutcome::Parse {
                valid: false,
                error: None,
            },
        );
        assert!(validate_fixture(&invalid_without_error).is_err());

        let valid_with_error = fixture(
            "valid-with-error",
            "PROFILE",
            ExpectedOutcome::Parse {
                valid: true,
                error: Some(ExpectedError {
                    category: "SyntaxError".into(),
                    line: Some(1),
                    column: Some(1),
                    message_contains: None,
                }),
            },
        );
        assert!(validate_fixture(&valid_with_error).is_err());
    }

    #[test]
    fn fixture_validation_rejects_invalid_error_location() {
        let invalid_location = fixture(
            "invalid-error-location",
            "PROFILE",
            ExpectedOutcome::Error {
                error: ExpectedError {
                    category: "SyntaxError".into(),
                    line: Some(0),
                    column: Some(0),
                    message_contains: None,
                },
            },
        );
        let error = validate_fixture(&invalid_location)
            .expect_err("zero-based error locations must be rejected");
        assert!(error.to_string().contains("error line 0"));
    }

    #[test]
    fn fixture_set_rejects_empty_inventory() {
        let fixtures: [Fixture; 0] = [];
        let error =
            validate_fixture_set(&fixtures).expect_err("empty fixture inventory must be rejected");
        assert!(error.to_string().contains("contains no JSON fixtures"));
        assert!(inventory_report(&fixtures).is_err());
    }

    #[test]
    fn fixture_set_rejects_duplicate_ids() {
        let expected = ExpectedOutcome::Parse {
            valid: true,
            error: None,
        };
        let fixtures = [
            fixture("duplicate", "PROFILE", expected.clone()),
            fixture("duplicate", "PROFILE", expected),
        ];
        let error = validate_fixture_set(&fixtures).expect_err("duplicate IDs must be rejected");
        assert!(error.to_string().contains("duplicate fixture id"));
        assert!(inventory_report(&fixtures).is_err());
    }

    #[test]
    fn fixture_set_rejects_mixed_profiles() {
        let expected = ExpectedOutcome::Parse {
            valid: true,
            error: None,
        };
        let fixtures = [
            fixture("one", "PROFILE-A", expected.clone()),
            fixture("two", "PROFILE-B", expected),
        ];
        let error = validate_fixture_set(&fixtures).expect_err("mixed profiles must be rejected");
        assert!(error.to_string().contains("mixed compatibility profiles"));
        assert!(inventory_report(&fixtures).is_err());
    }

    #[test]
    fn cypher25_fixture_inventory_is_valid() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/cypher25");
        let fixtures = load_fixture_directory(&root).expect("CY25 fixture inventory should load");
        assert_eq!(fixtures.len(), 15);
        assert!(
            fixtures
                .iter()
                .all(|fixture| fixture.profile.id == "CY25-2026.08")
        );
    }
}
