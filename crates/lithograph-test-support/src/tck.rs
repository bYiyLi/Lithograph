use serde::Serialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TckStepArgument {
    DocString { value: String },
    Table { rows: Vec<Vec<String>> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckStep {
    pub keyword: String,
    pub text: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument: Option<TckStepArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckExamples {
    pub name: String,
    pub line: usize,
    pub tags: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckScenario {
    pub name: String,
    pub line: usize,
    pub outline: bool,
    pub tags: Vec<String>,
    pub steps: Vec<TckStep>,
    pub examples: Vec<TckExamples>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckFeature {
    pub name: String,
    pub tags: Vec<String>,
    pub background: Vec<TckStep>,
    pub scenarios: Vec<TckScenario>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckScenarioInstance {
    pub name: String,
    pub line: usize,
    pub tags: Vec<String>,
    pub source_outline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub examples_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example_row: Option<usize>,
    pub steps: Vec<TckStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TckSuiteInventory {
    pub features: usize,
    pub scenarios: usize,
    pub outlines: usize,
    pub executable_scenarios: usize,
    pub steps: usize,
    pub doc_strings: usize,
    pub data_tables: usize,
    pub examples: usize,
}

pub fn read_feature(path: &Path) -> io::Result<TckFeature> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let mut feature_name = None;
    let mut feature_tags = Vec::new();
    let mut background = Vec::new();
    let mut scenarios = Vec::new();
    let mut current_scenario: Option<TckScenario> = None;
    let mut pending_tags = Vec::new();
    let mut in_background = false;
    let mut index = 0;

    while index < lines.len() {
        let raw_line = lines[index];
        let line = raw_line.trim();

        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }

        if line.starts_with('@') {
            pending_tags.extend(line.split_whitespace().map(str::to_string));
            index += 1;
            continue;
        }

        if let Some(name) = line.strip_prefix("Feature:") {
            feature_name = Some(name.trim().to_string());
            feature_tags = std::mem::take(&mut pending_tags);
            index += 1;
            continue;
        }

        if line.starts_with("Background:") {
            flush_scenario(&mut current_scenario, &mut scenarios);
            pending_tags.clear();
            in_background = true;
            index += 1;
            continue;
        }

        if let Some(name) = line.strip_prefix("Scenario Outline:") {
            flush_scenario(&mut current_scenario, &mut scenarios);
            current_scenario = Some(new_scenario(
                name.trim(),
                index + 1,
                true,
                std::mem::take(&mut pending_tags),
            ));
            in_background = false;
            index += 1;
            continue;
        }

        if let Some(name) = line.strip_prefix("Scenario:") {
            flush_scenario(&mut current_scenario, &mut scenarios);
            current_scenario = Some(new_scenario(
                name.trim(),
                index + 1,
                false,
                std::mem::take(&mut pending_tags),
            ));
            in_background = false;
            index += 1;
            continue;
        }

        if let Some(name) = line.strip_prefix("Examples:") {
            let scenario = current_scenario.as_mut().ok_or_else(|| {
                invalid_data(path, index + 1, "Examples block is outside a scenario")
            })?;
            if !scenario.outline {
                return Err(invalid_data(
                    path,
                    index + 1,
                    "Examples block belongs to a non-outline scenario",
                ));
            }

            let (rows, next_index) = read_following_table(path, &lines, index + 1)?;
            scenario.examples.push(TckExamples {
                name: name.trim().to_string(),
                line: index + 1,
                tags: std::mem::take(&mut pending_tags),
                rows,
            });
            index = next_index;
            continue;
        }

        if let Some((keyword, text)) = parse_step(line) {
            let (argument, next_index) = read_step_argument(path, &lines, index + 1)?;
            let step = TckStep {
                keyword: keyword.to_string(),
                text: text.to_string(),
                line: index + 1,
                argument,
            };

            if let Some(scenario) = current_scenario.as_mut() {
                scenario.steps.push(step);
            } else if in_background {
                background.push(step);
            } else {
                return Err(invalid_data(
                    path,
                    index + 1,
                    "step is outside a Background or Scenario",
                ));
            }
            index = next_index;
            continue;
        }

        // Free-form feature/scenario descriptions are legal Gherkin and do not
        // affect executable TCK semantics. Preserve the executable structures
        // above and ignore descriptive text here.
        index += 1;
    }

    flush_scenario(&mut current_scenario, &mut scenarios);

    let name =
        feature_name.ok_or_else(|| invalid_data(path, 1, "TCK file has no Feature heading"))?;
    Ok(TckFeature {
        name,
        tags: feature_tags,
        background,
        scenarios,
    })
}

pub fn read_suite_inventory(root: &Path) -> io::Result<TckSuiteInventory> {
    let mut paths = Vec::new();
    collect_feature_paths(root, &mut paths)?;
    paths.sort();

    let mut inventory = TckSuiteInventory {
        features: 0,
        scenarios: 0,
        outlines: 0,
        executable_scenarios: 0,
        steps: 0,
        doc_strings: 0,
        data_tables: 0,
        examples: 0,
    };

    for path in paths {
        let feature = read_feature(&path)?;
        let executable = expand_scenarios(&feature)?;
        inventory.features += 1;
        inventory.steps += feature.background.len();
        inventory.scenarios += feature.scenarios.len();
        inventory.executable_scenarios += executable.len();
        inventory.outlines += feature
            .scenarios
            .iter()
            .filter(|scenario| scenario.outline)
            .count();
        inventory.steps += feature
            .scenarios
            .iter()
            .map(|scenario| scenario.steps.len())
            .sum::<usize>();
        for step in feature.background.iter().chain(
            feature
                .scenarios
                .iter()
                .flat_map(|scenario| &scenario.steps),
        ) {
            match &step.argument {
                Some(TckStepArgument::DocString { .. }) => inventory.doc_strings += 1,
                Some(TckStepArgument::Table { .. }) => inventory.data_tables += 1,
                None => {}
            }
        }
        inventory.examples += feature
            .scenarios
            .iter()
            .map(|scenario| scenario.examples.len())
            .sum::<usize>();
    }

    Ok(inventory)
}

pub fn expand_scenarios(feature: &TckFeature) -> io::Result<Vec<TckScenarioInstance>> {
    let mut instances = Vec::new();

    for scenario in &feature.scenarios {
        if !scenario.outline {
            instances.push(TckScenarioInstance {
                name: scenario.name.clone(),
                line: scenario.line,
                tags: joined_tags(&feature.tags, &scenario.tags, &[]),
                source_outline: false,
                examples_line: None,
                example_row: None,
                steps: joined_steps(&feature.background, &scenario.steps),
            });
            continue;
        }

        if scenario.examples.is_empty() {
            return Err(invalid_feature(
                feature,
                scenario.line,
                "Scenario Outline has no Examples block",
            ));
        }

        for examples in &scenario.examples {
            let Some(header) = examples.rows.first() else {
                return Err(invalid_feature(
                    feature,
                    examples.line,
                    "Examples table has no header row",
                ));
            };
            if header.is_empty() {
                return Err(invalid_feature(
                    feature,
                    examples.line,
                    "Examples header row is empty",
                ));
            }

            for (row_index, row) in examples.rows.iter().enumerate().skip(1) {
                if row.len() != header.len() {
                    return Err(invalid_feature(
                        feature,
                        examples.line,
                        &format!(
                            "Examples row {row_index} has {} cells but header has {}",
                            row.len(),
                            header.len()
                        ),
                    ));
                }

                let substitutions: Vec<(&str, &str)> = header
                    .iter()
                    .zip(row)
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect();
                let steps = joined_steps(&feature.background, &scenario.steps)
                    .into_iter()
                    .map(|step| substitute_step(step, &substitutions))
                    .collect();

                instances.push(TckScenarioInstance {
                    name: substitute_placeholders(&scenario.name, &substitutions),
                    line: scenario.line,
                    tags: joined_tags(&feature.tags, &scenario.tags, &examples.tags),
                    source_outline: true,
                    examples_line: Some(examples.line),
                    example_row: Some(row_index),
                    steps,
                });
            }
        }
    }

    Ok(instances)
}

fn joined_tags(feature: &[String], scenario: &[String], examples: &[String]) -> Vec<String> {
    feature
        .iter()
        .chain(scenario)
        .chain(examples)
        .cloned()
        .collect()
}

fn joined_steps(background: &[TckStep], scenario: &[TckStep]) -> Vec<TckStep> {
    background.iter().chain(scenario).cloned().collect()
}

fn substitute_step(mut step: TckStep, substitutions: &[(&str, &str)]) -> TckStep {
    step.text = substitute_placeholders(&step.text, substitutions);
    step.argument = step.argument.map(|argument| match argument {
        TckStepArgument::DocString { value } => TckStepArgument::DocString {
            value: substitute_placeholders(&value, substitutions),
        },
        TckStepArgument::Table { rows } => TckStepArgument::Table {
            rows: rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| substitute_placeholders(&cell, substitutions))
                        .collect()
                })
                .collect(),
        },
    });
    step
}

fn substitute_placeholders(input: &str, substitutions: &[(&str, &str)]) -> String {
    let mut output = input.to_string();
    for (name, value) in substitutions {
        output = output.replace(&format!("<{name}>"), value);
    }
    output
}

fn new_scenario(name: &str, line: usize, outline: bool, tags: Vec<String>) -> TckScenario {
    TckScenario {
        name: name.to_string(),
        line,
        outline,
        tags,
        steps: Vec::new(),
        examples: Vec::new(),
    }
}

fn flush_scenario(current: &mut Option<TckScenario>, scenarios: &mut Vec<TckScenario>) {
    if let Some(scenario) = current.take() {
        scenarios.push(scenario);
    }
}

fn parse_step(line: &str) -> Option<(&str, &str)> {
    ["Given", "When", "Then", "And", "But"]
        .into_iter()
        .find_map(|keyword| {
            line.strip_prefix(keyword)
                .and_then(|rest| rest.strip_prefix(char::is_whitespace))
                .map(|rest| (keyword, rest.trim_start()))
        })
}

fn read_step_argument(
    path: &Path,
    lines: &[&str],
    start: usize,
) -> io::Result<(Option<TckStepArgument>, usize)> {
    if start >= lines.len() {
        return Ok((None, start));
    }

    let line = lines[start].trim();
    if line == "\"\"\"" {
        let indentation = leading_ascii_whitespace(lines[start]);
        let mut body = Vec::new();
        let mut index = start + 1;
        while index < lines.len() && lines[index].trim() != "\"\"\"" {
            body.push(dedent(lines[index], indentation));
            index += 1;
        }
        if index == lines.len() {
            return Err(invalid_data(
                path,
                start + 1,
                "unterminated step doc string",
            ));
        }
        return Ok((
            Some(TckStepArgument::DocString {
                value: body.join("\n"),
            }),
            index + 1,
        ));
    }

    if line.starts_with('|') {
        let (rows, next_index) = read_table(path, lines, start)?;
        return Ok((Some(TckStepArgument::Table { rows }), next_index));
    }

    Ok((None, start))
}

fn read_following_table(
    path: &Path,
    lines: &[&str],
    start: usize,
) -> io::Result<(Vec<Vec<String>>, usize)> {
    let mut index = start;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
        } else {
            break;
        }
    }
    if index >= lines.len() || !lines[index].trim().starts_with('|') {
        return Err(invalid_data(path, start + 1, "Examples block has no table"));
    }
    read_table(path, lines, index)
}

fn read_table(path: &Path, lines: &[&str], start: usize) -> io::Result<(Vec<Vec<String>>, usize)> {
    let mut rows = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.starts_with('#') {
            index += 1;
            continue;
        }
        if !line.starts_with('|') {
            break;
        }
        rows.push(parse_table_row(path, index + 1, line)?);
        index += 1;
    }
    if rows.is_empty() {
        return Err(invalid_data(path, start + 1, "table has no rows"));
    }
    Ok((rows, index))
}

fn parse_table_row(path: &Path, line_number: usize, line: &str) -> io::Result<Vec<String>> {
    if line == "|" {
        return Ok(Vec::new());
    }
    if !line.ends_with('|') {
        return Err(invalid_data(
            path,
            line_number,
            "table row must start and end with '|'",
        ));
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line[1..line.len() - 1].chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '|' => {
                cells.push(cell.trim().to_string());
                cell.clear();
            }
            '\\' => match chars.next() {
                Some('n') => cell.push('\n'),
                Some('|') => cell.push('|'),
                Some('\\') => cell.push('\\'),
                Some(other) => {
                    cell.push('\\');
                    cell.push(other);
                }
                None => cell.push('\\'),
            },
            other => cell.push(other),
        }
    }
    cells.push(cell.trim().to_string());
    Ok(cells)
}

fn leading_ascii_whitespace(line: &str) -> usize {
    line.as_bytes()
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count()
}

fn dedent(line: &str, indentation: usize) -> String {
    let bytes = line.as_bytes();
    let mut end = 0;
    while end < bytes.len() && end < indentation && matches!(bytes[end], b' ' | b'\t') {
        end += 1;
    }
    line[end..].to_string()
}

fn collect_feature_paths(root: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_feature_paths(&path, paths)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "feature")
        {
            paths.push(path);
        }
    }
    Ok(())
}

fn invalid_data(path: &Path, line: usize, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{line}: {message}", path.display()),
    )
}

fn invalid_feature(feature: &TckFeature, line: usize, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{line}: {message}", feature.name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reads_a_vendored_opencypher_tck_feature() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/opencypher-tck/features/clauses/return/Return1.feature");
        let feature = read_feature(&path).expect("vendored TCK feature should parse");
        assert!(!feature.name.is_empty());
        assert_eq!(feature.scenarios.len(), 2);
        assert_eq!(feature.scenarios[0].steps.len(), 5);
        assert!(matches!(
            &feature.scenarios[0].steps[1].argument,
            Some(TckStepArgument::DocString { value }) if value.contains("CREATE")
        ));
        assert!(matches!(
            &feature.scenarios[0].steps[3].argument,
            Some(TckStepArgument::Table { rows }) if rows.len() == 2
        ));
    }

    #[test]
    fn reads_outline_examples() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/opencypher-tck/features/clauses/with-orderBy/WithOrderBy1.feature");
        let feature = read_feature(&path).expect("outline feature should parse");
        let outline = feature
            .scenarios
            .iter()
            .find(|scenario| scenario.name.starts_with("[23]"))
            .expect("scenario outline [23] should exist");
        assert!(outline.outline);
        assert_eq!(outline.examples.len(), 1);
        assert_eq!(outline.examples[0].rows.len(), 4);

        let expanded = expand_scenarios(&feature).expect("scenario outlines should expand");
        let expanded_23: Vec<_> = expanded
            .iter()
            .filter(|scenario| scenario.name.starts_with("[23]"))
            .collect();
        assert_eq!(expanded_23.len(), 3);
        assert_eq!(expanded_23[0].example_row, Some(1));
        assert_eq!(expanded_23[1].example_row, Some(2));
        assert_eq!(expanded_23[2].example_row, Some(3));

        let queries: Vec<_> = expanded_23
            .iter()
            .filter_map(|scenario| {
                scenario.steps.iter().find_map(|step| match &step.argument {
                    Some(TckStepArgument::DocString { value }) if value.contains("ORDER BY") => {
                        Some(value.as_str())
                    }
                    _ => None,
                })
            })
            .collect();
        assert_eq!(queries.len(), 3);
        assert!(
            queries
                .iter()
                .any(|query| query.contains("ORDER BY bool\n"))
        );
        assert!(
            queries
                .iter()
                .any(|query| query.contains("ORDER BY bool ASC\n"))
        );
        assert!(
            queries
                .iter()
                .any(|query| query.contains("ORDER BY bool ASCENDING\n"))
        );
    }

    #[test]
    fn decodes_gherkin_data_table_escapes() {
        let path = Path::new("fixture.feature");
        let row = parse_table_row(
            path,
            1,
            r"| 'Foo\nFoo' | left\|right | slash\\value | quote\' |",
        )
        .expect("escaped table row should parse");

        assert_eq!(row[0], "'Foo\nFoo'");
        assert_eq!(row[1], "left|right");
        assert_eq!(row[2], r"slash\value");
        assert_eq!(row[3], r"quote\'");
    }

    #[test]
    fn vendored_table_escape_is_decoded_like_cucumber() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/opencypher-tck/features/expressions/string/String10.feature");
        let feature = read_feature(&path).expect("vendored escaped table should parse");
        let has_decoded_newline = feature
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.steps)
            .any(|step| match &step.argument {
                Some(TckStepArgument::Table { rows }) => {
                    rows.iter().flatten().any(|cell| cell == "'Foo\nFoo'")
                }
                _ => false,
            });
        assert!(has_decoded_newline);
    }

    #[test]
    fn examples_tables_ignore_comment_lines_between_rows() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../vendor/opencypher-tck/features/expressions/precedence/Precedence1.feature",
        );
        let feature = read_feature(&path).expect("commented examples table should parse");
        let outline = feature
            .scenarios
            .iter()
            .find(|scenario| scenario.name.starts_with("[21]"))
            .expect("scenario outline [21] should exist");
        assert_eq!(outline.examples.len(), 1);
        assert_eq!(outline.examples[0].rows.len(), 15);
    }

    #[test]
    fn expanded_scenarios_include_background_steps() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/opencypher-tck/features/clauses/match/Match5.feature");
        let feature = read_feature(&path).expect("feature with background should parse");
        assert_eq!(feature.background.len(), 2);
        let expanded = expand_scenarios(&feature).expect("feature should expand");
        let first = expanded.first().expect("feature should contain scenarios");
        assert_eq!(first.steps[0].text, "an empty graph");
        assert_eq!(first.steps[1].text, "having executed:");
        assert!(first.steps[2].text.starts_with("executing query:"));
    }

    #[test]
    fn scenario_tags_are_preserved_in_executable_instances() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/opencypher-tck/features/clauses/match/Match4.feature");
        let feature = read_feature(&path).expect("tagged feature should parse");
        let expanded = expand_scenarios(&feature).expect("tagged feature should expand");
        let tagged = expanded
            .iter()
            .find(|scenario| scenario.name.starts_with("[9]"))
            .expect("tagged scenario [9] should exist");
        assert!(tagged.tags.iter().any(|tag| tag == "@skipGrammarCheck"));
    }

    #[test]
    fn reads_entire_vendored_suite() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/opencypher-tck/features");
        let inventory = read_suite_inventory(&root).expect("vendored TCK suite should parse");
        assert_eq!(inventory.features, 220);
        assert_eq!(inventory.scenarios, 1_615);
        assert_eq!(inventory.outlines, 276);
        assert_eq!(inventory.executable_scenarios, 3_897);
        assert_eq!(inventory.steps, 7_104);
        assert_eq!(inventory.doc_strings, 2_370);
        assert_eq!(inventory.data_tables, 1_687);
        assert_eq!(inventory.examples, 276);
    }
}
