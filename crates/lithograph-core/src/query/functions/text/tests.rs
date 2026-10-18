use std::cell::Cell;

use super::*;
use crate::query::resources::QueryResources;

fn string(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn interrupt_on_second_check() -> impl Fn() -> bool {
    let checks = Cell::new(0);
    move || {
        checks.set(checks.get() + 1);
        checks.get() >= 2
    }
}

#[test]
fn replacement_preserves_empty_pattern_and_limit_semantics() {
    for input in ["", "a", "abc", "人物😀人物"] {
        for search in ["", "a", "人物", "😀", "missing"] {
            for replacement in ["", "x", "😀"] {
                let arguments = [string(input), string(search), string(replacement)];
                assert_eq!(
                    evaluate("replace", &arguments).unwrap().unwrap(),
                    Value::String(input.replace(search, replacement)),
                );
                for limit in [0, 1, 2, 10] {
                    let mut arguments = arguments.to_vec();
                    arguments.push(Value::Integer(limit));
                    assert_eq!(
                        evaluate("replace", &arguments).unwrap().unwrap(),
                        Value::String(input.replacen(search, replacement, limit as usize)),
                    );
                }
            }
        }
    }
}

#[test]
fn chunked_search_finds_unicode_boundary_and_large_patterns() {
    let input = format!("{}人物😀tail", "a".repeat(TEXT_CHUNK - 1));
    assert_eq!(
        find(&input, "人物😀", 0, None).unwrap(),
        Some(TEXT_CHUNK - 1)
    );
    assert_eq!(find(&input, "absent", 0, None).unwrap(), None);
    let pattern = format!("{}z", "a".repeat(TEXT_CHUNK + 3));
    let input = format!("{}{}", "前", pattern);
    assert_eq!(find(&input, &pattern, 0, None).unwrap(), Some(3));
}

#[test]
fn string_slices_keep_character_offsets_without_character_vectors() {
    let input = "a人😀b";
    let characters = input.chars().collect::<Vec<_>>();
    for length in 0..=6 {
        let left = characters.iter().take(length).collect::<String>();
        let right = characters
            .iter()
            .skip(characters.len().saturating_sub(length))
            .collect::<String>();
        assert_eq!(
            evaluate("left", &[string(input), Value::Integer(length as i64)])
                .unwrap()
                .unwrap(),
            Value::String(left)
        );
        assert_eq!(
            evaluate("right", &[string(input), Value::Integer(length as i64)])
                .unwrap()
                .unwrap(),
            Value::String(right)
        );
        for start in 0..=6 {
            let expected = characters
                .iter()
                .skip(start)
                .take(length)
                .collect::<String>();
            assert_eq!(
                evaluate(
                    "substring",
                    &[
                        string(input),
                        Value::Integer(start as i64),
                        Value::Integer(length as i64),
                    ]
                )
                .unwrap()
                .unwrap(),
                Value::String(expected)
            );
        }
    }
    assert_eq!(
        evaluate("trim", &[string("\u{2003}人 😀 \u{2003}")])
            .unwrap()
            .unwrap(),
        string("人 😀")
    );
    assert_eq!(
        evaluate("trim", &[string("人人😀人"), string("人")])
            .unwrap()
            .unwrap(),
        string("😀")
    );
}

#[test]
fn unicode_case_and_normalization_preserve_existing_results() {
    for input in ["ΟΣ", "ΟΣΑ", "ΣΟΣ", "AΣ\u{0301}", "İßẞ人物😀", ""] {
        assert_eq!(
            evaluate("lower", &[string(input)]).unwrap().unwrap(),
            Value::String(input.to_lowercase()),
        );
        assert_eq!(
            evaluate("upper", &[string(input)]).unwrap().unwrap(),
            Value::String(input.to_uppercase()),
        );
    }
    let input = "A\u{0301}\u{0323}Å㍍人";
    for (form, expected) in [
        ("NFC", input.nfc().collect::<String>()),
        ("NFD", input.nfd().collect()),
        ("NFKC", input.nfkc().collect()),
        ("NFKD", input.nfkd().collect()),
    ] {
        assert_eq!(
            evaluate("normalize", &[string(input), string(form)])
                .unwrap()
                .unwrap(),
            Value::String(expected),
        );
    }
}

#[test]
fn unicode_lowercase_contractions_reserve_builder_capacity_before_allocation() {
    let input = "K".repeat(1024);
    let arguments = [string(&input)];
    let resources = QueryResources::new(Some(output_bytes(2 * 1024).unwrap()));
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    let mut memory = None;
    let error = evaluate_with_memory("lower", &arguments, Some(&control), &mut memory)
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.peak(), 0, "reject the builder's initial capacity");
    assert_eq!(resources.used(), 0);
    assert!(memory.is_none());

    let resources = QueryResources::new(Some(output_bytes(input.len()).unwrap()));
    let control = ExecutionControl::new(&resources, &interrupted);
    let output = evaluate_with_memory("lower", &arguments, Some(&control), &mut memory)
        .unwrap()
        .unwrap();
    assert_eq!(output, string(&"k".repeat(1024)));
    assert!(
        memory.as_ref().unwrap().bytes() >= crate::query::resources::value_bytes(&output).unwrap()
    );
    drop(output);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn unicode_uppercase_reserves_exact_output_for_contractions_and_expansions() {
    let input = "ſıﬃΐ".repeat(1024);
    let expected = input.to_uppercase();
    let resources = QueryResources::new(Some(output_bytes(expected.len()).unwrap()));
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    let mut memory = None;
    let output = evaluate_with_memory("upper", &[string(&input)], Some(&control), &mut memory)
        .unwrap()
        .unwrap();
    assert_eq!(output, string(&expected));
    assert!(
        memory.as_ref().unwrap().bytes() >= crate::query::resources::value_bytes(&output).unwrap()
    );
    drop(output);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn normalization_predicate_streams_without_materializing_normalized_output() {
    for input in [
        "",
        "abc",
        "人😀",
        "A\u{0301}\u{0323}Å㍍",
        "\u{0301}\u{0323}",
    ] {
        for (form, expected) in [
            (NormalizationForm::Nfc, unicode_normalization::is_nfc(input)),
            (NormalizationForm::Nfd, unicode_normalization::is_nfd(input)),
            (
                NormalizationForm::Nfkc,
                unicode_normalization::is_nfkc(input),
            ),
            (
                NormalizationForm::Nfkd,
                unicode_normalization::is_nfkd(input),
            ),
        ] {
            let resources = QueryResources::new(Some(1024));
            let interrupted = || false;
            let control = ExecutionControl::new(&resources, &interrupted);
            assert_eq!(
                normalized_predicate_controlled(input, form, Some(&control)).unwrap(),
                expected
            );
            assert_eq!(resources.used(), 0);
        }
    }
    let input = "a".repeat(100_000);
    let resources = QueryResources::new(Some(1024));
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    assert!(
        normalized_predicate_controlled(&input, NormalizationForm::Nfc, Some(&control)).unwrap()
    );
    assert!(resources.peak() < 1024);
    let interrupted = interrupt_on_second_check();
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = normalized_predicate_controlled(&input, NormalizationForm::Nfc, Some(&control))
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}

#[test]
fn regex_expansion_preserves_capture_reference_semantics() {
    let pattern = regex::Regex::new("(?P<letter>a)(b)?").unwrap();
    for replacement in [
        "",
        "$0",
        "$1",
        "$2",
        "$letter",
        "${letter}",
        "${1}x",
        "$1x",
        "$$",
        "$$$1",
        "$",
        "${",
        "${}",
        "${missing}",
        "${letter",
        "$999999999999999999999999",
        "人物${letter}😀$$$2",
    ] {
        let input = "ab a人物";
        let result = evaluate(
            "string.regexreplace",
            &[string(input), string(pattern.as_str()), string(replacement)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            result,
            Value::String(pattern.replace_all(input, replacement).into_owned()),
            "replacement: {replacement}",
        );
    }
}

#[test]
fn string_outputs_refuse_budget_before_allocation_and_release_leases() {
    let resources = QueryResources::new(Some(1024));
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    for (name, arguments) in [
        (
            "replace",
            vec![
                string(&"a".repeat(512)),
                string("a"),
                string(&"b".repeat(512)),
            ],
        ),
        ("split", vec![string(&"a".repeat(512)), string("")]),
        (
            "string.join",
            vec![Value::List(vec![string(&"a".repeat(512)); 4]), string("-")],
        ),
    ] {
        let error = evaluate_controlled(name, &arguments, Some(&control))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource, "function: {name}");
        assert_eq!(resources.used(), 0);
    }
    assert_eq!(
        evaluate_controlled(
            "replace",
            &[string("abc"), string("b"), string("x")],
            Some(&control)
        )
        .unwrap()
        .unwrap(),
        string("axc"),
    );
    assert_eq!(resources.used(), 0);
}

#[test]
fn returned_string_transfers_its_live_reservation_without_a_budget_gap() {
    let expected_bytes = output_bytes(512).unwrap();
    let resources = QueryResources::new(Some(expected_bytes));
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    let mut result_memory = None;
    let output = evaluate_with_memory(
        "upper",
        &[string(&"a".repeat(512))],
        Some(&control),
        &mut result_memory,
    )
    .unwrap()
    .unwrap();
    assert_eq!(resources.used(), expected_bytes);
    assert_eq!(result_memory.as_ref().unwrap().bytes(), expected_bytes);
    assert_eq!(
        resources.reserve(1).unwrap_err().kind,
        QueryErrorKind::Resource
    );
    assert_eq!(output, string(&"A".repeat(512)));
    drop(output);
    drop(result_memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn every_allocated_text_result_transfers_its_output_reservation() {
    let resources = QueryResources::new(Some(32 * 1024 * 1024));
    let interrupted = || false;
    let control = ExecutionControl::new(&resources, &interrupted);
    for (name, arguments) in [
        ("lower", vec![string("ABC")]),
        ("upper", vec![string("abc")]),
        ("trim", vec![string(" abc ")]),
        ("left", vec![string("abc"), Value::Integer(2)]),
        ("right", vec![string("abc"), Value::Integer(2)]),
        ("substring", vec![string("abc"), Value::Integer(1)]),
        ("replace", vec![string("abc"), string("b"), string("x")]),
        ("split", vec![string("abc"), string("")]),
        (
            "string.join",
            vec![Value::List(vec![string("a"), string("b")]), string("-")],
        ),
        (
            "string.regexreplace",
            vec![string("abc"), string("b"), string("x")],
        ),
        ("normalize", vec![string("a\u{0301}")]),
    ] {
        let mut result_memory = None;
        let output = evaluate_with_memory(name, &arguments, Some(&control), &mut result_memory)
            .unwrap()
            .unwrap();
        let lease = result_memory.expect("allocated result owns its reservation");
        assert!(
            lease.bytes() >= crate::query::resources::value_bytes(&output).unwrap(),
            "function: {name}"
        );
        assert_eq!(resources.used(), lease.bytes(), "function: {name}");
        drop(output);
        drop(lease);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn long_text_functions_observe_deterministic_cancellation() {
    for name in [
        "char_length",
        "upper",
        "normalize",
        "replace",
        "split",
        "string.indexof",
    ] {
        let resources = QueryResources::default();
        let interrupted = interrupt_on_second_check();
        let control = ExecutionControl::new(&resources, &interrupted);
        let input = string(&"人".repeat(100_000));
        let arguments = match name {
            "replace" => vec![input, string("missing"), string("x")],
            "split" => vec![input, string("")],
            "string.indexof" => vec![input, string("missing")],
            _ => vec![input],
        };
        let error = evaluate_controlled(name, &arguments, Some(&control))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted, "function: {name}");
        assert_eq!(resources.used(), 0);
    }
}
