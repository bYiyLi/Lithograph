//! Bounded traversal while translating the existing FTS query syntax.
//! The caller retains the metadata/query scratch reservation throughout.

use super::*;
use crate::query::functions::{append_text_controlled as append, find_text_controlled as find};

pub(super) fn translate_fulltext_query(
    index: &IndexDefinition,
    query: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    let properties = index_properties(index)?;
    let mut query = translate_lucene_query(query, control)?;
    for (ordinal, property) in properties.iter().enumerate() {
        checkpoint(control)?;
        let mut pattern = copy(property, control)?;
        pattern.push(':');
        query = replace(&query, &pattern, &format!("c{ordinal}:"), control)?;
    }
    Ok(query)
}

fn checkpoint(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    control.map_or(Ok(()), |control| control.work(1))
}

fn copy(input: &str, control: Option<&ExecutionControl<'_>>) -> QueryResult<String> {
    let mut output = String::with_capacity(input.len());
    append(&mut output, input, control)?;
    Ok(output)
}

fn replace(
    input: &str,
    pattern: &str,
    replacement: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    let mut output = String::new();
    let mut offset = 0;
    while let Some(found) = find(input, pattern, offset, control)? {
        checkpoint(control)?;
        append(&mut output, &input[offset..found], control)?;
        append(&mut output, replacement, control)?;
        offset = found + pattern.len();
    }
    append(&mut output, &input[offset..], control)?;
    Ok(output)
}

#[derive(Clone, Copy)]
enum LuceneClauseMode {
    Optional,
    Required,
    Prohibited,
}

fn translate_lucene_query(
    query: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    let Some(tokens) = split_query(query, control)? else {
        return copy(query, control);
    };
    for token in &tokens {
        checkpoint(control)?;
        if ["AND", "OR", "NOT", "&&", "||", "!"]
            .iter()
            .any(|keyword| token.eq_ignore_ascii_case(keyword))
        {
            return copy(query, control);
        }
    }
    let mut clauses = Vec::with_capacity(tokens.len());
    for token in tokens {
        checkpoint(control)?;
        let Some(clause) = lucene_clause(&token, control)? else {
            return copy(query, control);
        };
        clauses.push(clause);
    }
    render_clauses(&clauses, control)
}

fn split_query(
    query: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<Vec<String>>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in query.chars() {
        checkpoint(control)?;
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            token.push(character);
            escaped = true;
            continue;
        }
        if character == '"' {
            token.push(character);
            quoted = !quoted;
            continue;
        }
        if !quoted && matches!(character, '(' | ')') {
            return Ok(None);
        }
        if !quoted && character.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
            continue;
        }
        token.push(character);
    }
    if quoted || escaped {
        return Ok(None);
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(Some(tokens))
}

fn lucene_clause(
    token: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<(LuceneClauseMode, String)>> {
    let (mode, clause) = match token.as_bytes().first().copied() {
        Some(b'+') => (LuceneClauseMode::Required, &token[1..]),
        Some(b'-') => (LuceneClauseMode::Prohibited, &token[1..]),
        _ => (LuceneClauseMode::Optional, token),
    };
    if clause.is_empty() {
        return Ok(None);
    }
    Ok(Some((mode, copy(clause, control)?)))
}

fn render_clauses(
    clauses: &[(LuceneClauseMode, String)],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    let mut required = Vec::new();
    let mut optional = Vec::new();
    let mut prohibited = Vec::new();
    for (mode, clause) in clauses {
        checkpoint(control)?;
        match mode {
            LuceneClauseMode::Required => required.push(clause),
            LuceneClauseMode::Optional => optional.push(clause),
            LuceneClauseMode::Prohibited => prohibited.push(clause),
        }
    }
    let (selected, separator) = if required.is_empty() {
        (optional, " OR ")
    } else {
        (required, " AND ")
    };
    let mut output = String::new();
    for (index, clause) in selected.iter().enumerate() {
        if index > 0 {
            append(&mut output, separator, control)?;
        }
        append(&mut output, clause, control)?;
    }
    if output.is_empty() {
        return Ok(output);
    }
    for clause in prohibited {
        let mut next = String::new();
        next.push('(');
        append(&mut next, &output, control)?;
        append(&mut next, ") NOT ", control)?;
        append(&mut next, clause, control)?;
        output = next;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryResources;
    use std::cell::Cell;

    #[test]
    fn controlled_translation_keeps_boolean_quoting_and_property_replacement() {
        for (query, expected) in [
            ("first second", "first OR second"),
            ("+人物 -消失", "(人物) NOT 消失"),
            ("field:x AND field:y", "field:x AND field:y"),
            ("\"人物 带空格\"", "\"人物 带空格\""),
            ("(first OR second)", "(first OR second)"),
        ] {
            assert_eq!(
                translate_lucene_query(query, None).expect("translate"),
                expected
            );
        }
        assert_eq!(
            replace("人物:甲 人物:乙", "人物:", "c0:", None).expect("replace"),
            "c0:甲 c0:乙"
        );
    }

    #[test]
    fn long_fulltext_query_checks_cancellation_while_tokenizing() {
        let query = "字".repeat(131_072);
        let resources = QueryResources::default();
        let checks = Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() == 3
        };
        let control = ExecutionControl::new(&resources, &interrupted);
        let error =
            translate_lucene_query(&query, Some(&control)).expect_err("interrupt long token");
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), 3);
    }
}
