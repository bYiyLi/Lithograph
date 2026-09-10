use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use lithograph_core::cypher::{parse, validate};
use lithograph_test_support::tck::{TckStepArgument, expand_scenarios, read_feature};

#[test]
fn all_vendored_opencypher_valid_queries_parse() {
    let root = repo_root().join("vendor/opencypher-tck/features");
    let features = feature_paths(&root);
    let mut queries = 0_usize;
    let mut failures = Vec::new();

    for path in features {
        let feature = read_feature(&path).expect("TCK feature must parse");
        for scenario in expand_scenarios(&feature).expect("outline expansion") {
            if expects_compile_time_syntax_error(&scenario.steps) {
                continue;
            }
            for step in &scenario.steps {
                if !matches!(step.text.as_str(), "executing query:" | "having executed:") {
                    continue;
                }
                let Some(TckStepArgument::DocString { value: query }) = &step.argument else {
                    continue;
                };
                queries += 1;
                if let Err(error) = parse(query) {
                    failures.push(format!(
                        "{} :: {} @ {}:{}: {}\n{}",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        scenario.name,
                        error.line,
                        error.column,
                        error.message,
                        query
                    ));
                }
            }
        }
    }

    assert_eq!(
        queries, 4_224,
        "vendored TCK parser query inventory changed"
    );
    assert!(
        failures.is_empty(),
        "{} inherited valid query/query-precondition parser failure(s):\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

#[test]
fn inherited_non_compile_error_queries_validate_in_the_frontend() {
    let root = repo_root().join("vendor/opencypher-tck/features");
    let features = feature_paths(&root);
    let mut queries = 0_usize;
    let mut failures = Vec::new();

    for path in features {
        let feature = read_feature(&path).expect("TCK feature must parse");
        for scenario in expand_scenarios(&feature).expect("outline expansion") {
            if expects_compile_time_syntax_error(&scenario.steps) {
                continue;
            }
            for step in &scenario.steps {
                if step.text != "executing query:" {
                    continue;
                }
                let Some(TckStepArgument::DocString { value: query }) = &step.argument else {
                    continue;
                };
                queries += 1;
                if let Err(error) = validate(query) {
                    failures.push(format!(
                        "{} :: {} @ {}:{} [{:?}]: {}\n{}",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        scenario.name,
                        error.line,
                        error.column,
                        error.kind,
                        error.message,
                        query
                    ));
                }
            }
        }
    }

    assert_eq!(
        queries, 3_312,
        "vendored TCK frontend query inventory changed"
    );
    assert!(
        failures.is_empty(),
        "{} inherited query frontend validation failure(s):\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

#[test]
fn phase03_owned_compile_time_errors_are_rejected() {
    let root = repo_root().join("vendor/opencypher-tck/features");
    let features = feature_paths(&root);
    let mut queries = 0_usize;
    let mut deferred = BTreeSet::new();
    let mut failures = Vec::new();

    for path in features {
        let feature = read_feature(&path).expect("TCK feature must parse");
        for scenario in expand_scenarios(&feature).expect("outline expansion") {
            if !expects_compile_time_syntax_error(&scenario.steps) {
                continue;
            }
            for step in &scenario.steps {
                if step.text != "executing query:" {
                    continue;
                }
                let Some(TckStepArgument::DocString { value: query }) = &step.argument else {
                    continue;
                };
                queries += 1;
                if validate(query).is_err() {
                    continue;
                }
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                if deferred_to_later_phase(relative, &scenario.name) {
                    deferred.insert(format!("{} :: {}", relative.display(), scenario.name));
                    continue;
                }
                failures.push(format!(
                    "{} :: {}\n{}",
                    relative.display(),
                    scenario.name,
                    query
                ));
            }
        }
    }

    assert_eq!(queries, 585, "vendored TCK compile-error inventory changed");
    assert_eq!(
        deferred,
        expected_deferred_scenarios(),
        "Phase 03 deferred frontend scenario inventory changed"
    );
    assert!(
        failures.is_empty(),
        "{} Phase 03-owned compile-time error(s) were accepted:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

fn expected_deferred_scenarios() -> BTreeSet<String> {
    [
        "clauses/call/Call1.feature :: [10] In-query call to procedure should fail if too many explicit argument are given",
        "clauses/call/Call1.feature :: [7] Standalone call to procedure should fail if explicit argument is missing",
        "clauses/call/Call1.feature :: [8] In-query call to procedure should fail if explicit argument is missing",
        "clauses/call/Call1.feature :: [9] Standalone call to procedure should fail if too many explicit argument are given",
        "clauses/call/Call2.feature :: [4] In-query call to procedure that takes arguments fails when trying to pass them implicitly",
        "clauses/call/Call2.feature :: [5] Standalone call to procedure should fail if input type is wrong",
        "clauses/call/Call2.feature :: [6] In-query call to procedure should fail if input type is wrong",
        "clauses/return-orderby/ReturnOrderBy2.feature :: [13] Fail when sorting on variable removed by DISTINCT",
        "clauses/return-orderby/ReturnOrderBy2.feature :: [14] Fail on aggregation in ORDER BY after RETURN",
        "clauses/return-orderby/ReturnOrderBy6.feature :: [4] Fail if not returned variables are used inside an order by item which contains an aggregation expression",
        "clauses/return-orderby/ReturnOrderBy6.feature :: [5] Fail if more complex expressions, even if returned, are used inside an order by item which contains an aggregation expression",
        "clauses/return/Return2.feature :: [18] Fail on projecting a non-existent function",
        "clauses/with-orderBy/WithOrderBy2.feature :: [25] Fail on sorting by an aggregation",
        "clauses/with-orderBy/WithOrderBy4.feature :: [13] Fail on sorting by a non-projected aggregation on a variable",
        "clauses/with-orderBy/WithOrderBy4.feature :: [14] Fail on sorting by a non-projected aggregation on an expression",
        "clauses/with-orderBy/WithOrderBy4.feature :: [19] Fail if not projected variables are used inside an order by item which contains an aggregation expression",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn expects_compile_time_syntax_error(steps: &[lithograph_test_support::tck::TckStep]) -> bool {
    steps.iter().any(|step| {
        step.text
            .contains("SyntaxError should be raised at compile time")
    })
}

fn deferred_to_later_phase(path: &Path, scenario_name: &str) -> bool {
    let path = path.to_string_lossy();
    if path.starts_with("clauses/call/") {
        // Procedure catalog/signature resolution is owned by the procedure phases.
        return true;
    }
    if path == "clauses/return/Return2.feature" && scenario_name.contains("non-existent function") {
        // Complete built-in function inventory is owned by Phase 06.
        return true;
    }
    let order_by = path.to_ascii_lowercase().contains("orderby");
    let aggregate_rule = scenario_name.to_ascii_lowercase().contains("aggregat");
    let distinct_visibility = scenario_name.contains("removed by DISTINCT");
    order_by && (aggregate_rule || distinct_visibility)
}

fn feature_paths(root: &Path) -> Vec<PathBuf> {
    let mut features = Vec::new();
    collect_features(root, &mut features);
    features.sort();
    features
}

fn collect_features(root: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("TCK feature directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_features(&path, output);
        } else if path.extension().and_then(|value| value.to_str()) == Some("feature") {
            output.push(path);
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}
