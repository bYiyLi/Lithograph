use super::*;
use lithograph_core::cypher::UuidValue;

#[test]
fn write_projection_order_by_uses_cypher_total_value_ordering() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Source {sort: true}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("boolean source");
    execute(
        &connection,
        "CREATE (:Source {sort: 'z'}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("string source");

    let before = branch_head(&connection, "main").expect("head before write");
    let (rows, summary) = execute(
        &connection,
        "MATCH (source:Source) CREATE (:Copy {sort: source.sort}) RETURN source.sort AS sort ORDER BY sort",
        ExecutionOptions::default(),
    )
    .expect("ordered write projection");

    assert_eq!(
        rows,
        vec![
            vec![Value::String("z".to_owned())],
            vec![Value::Boolean(true)],
        ]
    );
    assert_eq!(summary.counters.nodes_created, 2);
    assert_ne!(
        branch_head(&connection, "main").expect("head after write"),
        before
    );
}

#[test]
fn unsupported_write_projection_ordering_rolls_back_the_mutation() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before write");
    let params = BTreeMap::from([(
        "id".to_owned(),
        Value::Uuid(UuidValue::parse("018f1f6e-7a5b-7c3d-8e9f-0123456789ab").expect("UUID")),
    )]);

    let error = execute_with_params(
        &connection,
        "CREATE (:Created {id: $id}) RETURN $id AS id ORDER BY id",
        params,
        ExecutionOptions::default(),
    )
    .expect_err("UUID ORDER BY remains unsupported");

    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(
        branch_head(&connection, "main").expect("head after error"),
        before
    );
    assert_eq!(
        read_rows(&connection, "MATCH (n:Created) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn write_distinct_uses_grouping_equality_for_nulls() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Source), (:Source) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed source nodes");

    let (rows, summary) = execute(
        &connection,
        "MATCH (:Source) CREATE (:Copy) RETURN DISTINCT null AS value",
        ExecutionOptions::default(),
    )
    .expect("write with DISTINCT null projection");

    assert_eq!(rows, vec![vec![Value::Null]]);
    assert_eq!(summary.counters.nodes_created, 2);
}

#[test]
fn create_property_expressions_cannot_read_variables_created_by_the_same_clause() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before rejected CREATE");

    for query in [
        "CREATE (a {value:1}), (b {copied:a.value}) FINISH",
        "CREATE (a {value:1})-[r:R {copied:a.value}]->() FINISH",
    ] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("same-clause CREATE property reference must be rejected");
        assert_eq!(error.kind, QueryErrorKind::Semantic);
    }

    assert_eq!(
        branch_head(&connection, "main").expect("head after rejected CREATE"),
        before
    );
}

#[test]
fn merge_skips_relationship_candidates_with_the_wrong_bound_endpoint() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:A {name:'start'}), (other:B {name:'other'}), (target:B {name:'target'}) CREATE (a)-[:R]->(other), (a)-[:R]->(target) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed competing Relationship candidates");
    let before = branch_head(&connection, "main").expect("head before MERGE");

    let (_, summary) = execute(
        &connection,
        "MATCH (a:A), (target:B) WHERE target.name = 'target' MERGE (a)-[:R]->(target) FINISH",
        ExecutionOptions::default(),
    )
    .expect("MERGE must continue past a Relationship to a different bound endpoint");

    assert_ne!(
        branch_head(&connection, "main").expect("head after MERGE"),
        before,
        "the successful no-op MERGE still creates a Commit"
    );
    assert_eq!(summary.counters.relationships_created, 0);
    assert_eq!(
        read_rows(&connection, "MATCH ()-[r:R]->() RETURN count(r)"),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn create_can_reuse_a_decorated_node_without_redecorating_the_repetition() {
    let connection = fresh_storage();

    let (_, summary) = execute(
        &connection,
        "CREATE (root:Root {name:'root'}), (child:Child {name:'child'}), (root)-[:LINK]->(child) FINISH",
        ExecutionOptions::default(),
    )
    .expect("same CREATE may reuse bare references to newly created Nodes");

    assert_eq!(summary.counters.nodes_created, 2);
    assert_eq!(summary.counters.relationships_created, 1);
    assert_eq!(
        read_rows(
            &connection,
            "MATCH (root:Root)-[:LINK]->(child:Child) RETURN root.name, child.name"
        ),
        vec![vec![
            Value::String("root".to_owned()),
            Value::String("child".to_owned())
        ]]
    );

    let before = branch_head(&connection, "main").expect("head before invalid repetition");
    let error = execute(
        &connection,
        "CREATE (node:Base), (node:Extra)-[:INVALID]->() FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("a repeated Node variable cannot gain another label");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("head after invalid repetition"),
        before
    );
}

#[test]
fn delete_removes_relationships_from_all_rows_before_deleting_nodes() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (a:A), (b:B), (c:C) CREATE (a)-[:R]->(b), (a)-[:R]->(c) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed a Node with two Relationships");

    let (_, summary) = execute(
        &connection,
        "MATCH (a:A)-[relationship:R]->() DELETE relationship, a FINISH",
        ExecutionOptions::default(),
    )
    .expect("the DELETE clause must materialize all explicit Relationship deletions first");

    assert_eq!(summary.counters.relationships_deleted, 2);
    assert_eq!(summary.counters.nodes_deleted, 1);
    assert_eq!(
        read_rows(&connection, "MATCH (n) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        read_rows(&connection, "MATCH ()-[r:R]->() RETURN count(r)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn write_name_expressions_accept_dynamic_names_and_reject_non_concrete_names() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before unsupported names");

    for query in ["CREATE (:A|B) FINISH", "CREATE ()-[:!NEGATED]->() FINISH"] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("unsupported write name expression must be rejected");
        assert_eq!(error.kind, QueryErrorKind::Semantic, "query: {query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after rejected name"),
            before,
            "query: {query}"
        );
    }

    for label in ["A", "B"] {
        assert!(
            lithograph_core::storage::find_label(&connection, label)
                .expect("find rejected label")
                .is_none()
        );
    }
    assert!(
        lithograph_core::storage::find_relationship_type(&connection, "NEGATED")
            .expect("find rejected Relationship Type")
            .is_none()
    );

    let (_, summary) = execute(
        &connection,
        "CREATE ()-[:$('DYNAMIC')]->() FINISH",
        ExecutionOptions::default(),
    )
    .expect("dynamic Relationship Type");
    assert_eq!(summary.counters.relationships_created, 1);
    assert!(
        lithograph_core::storage::find_relationship_type(&connection, "DYNAMIC")
            .expect("find dynamic Relationship Type")
            .is_some()
    );
}

#[test]
fn unsupported_inline_write_predicates_fail_instead_of_being_ignored() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before inline predicates");

    for query in [
        "CREATE (node:RejectedNode WHERE false) FINISH",
        "CREATE ()-[relationship:REJECTED_REL WHERE false]->() FINISH",
        "MERGE (node:RejectedMerge WHERE false) FINISH",
    ] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("an inline write predicate must not be ignored");
        assert_eq!(error.kind, QueryErrorKind::Semantic, "query: {query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after inline predicate"),
            before,
            "query: {query}"
        );
    }

    for label in ["RejectedNode", "RejectedMerge"] {
        assert!(
            lithograph_core::storage::find_label(&connection, label)
                .expect("find rejected inline-predicate label")
                .is_none()
        );
    }
    assert!(
        lithograph_core::storage::find_relationship_type(&connection, "REJECTED_REL")
            .expect("find rejected inline-predicate Relationship Type")
            .is_none()
    );
}

#[test]
fn unsupported_write_path_modifiers_fail_instead_of_being_ignored() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before path modifiers");

    for query in [
        "CREATE SHORTEST 1 PATH ()-[:SELECTED]->() FINISH",
        "CREATE WALK ()-[:MODED]->() FINISH",
        "CREATE (()-[:QUANTIFIED]->()){2} FINISH",
    ] {
        let error = execute(&connection, query, ExecutionOptions::default())
            .expect_err("an unsupported write path modifier must not be ignored");
        assert_eq!(error.kind, QueryErrorKind::Semantic, "query: {query}");
        assert_eq!(
            branch_head(&connection, "main").expect("head after path modifier"),
            before,
            "query: {query}"
        );
    }

    for relationship_type in ["SELECTED", "MODED", "QUANTIFIED"] {
        assert!(
            lithograph_core::storage::find_relationship_type(&connection, relationship_type)
                .expect("find rejected path-modifier Relationship Type")
                .is_none()
        );
    }
}

#[test]
fn postfix_expressions_write_and_return_their_computed_values() {
    let connection = fresh_storage();
    let (rows, summary) = execute(
        &connection,
        "CREATE (node:Postfix) SET node.item = [1,2][0], node.slice = [1,2][0..1] RETURN node.item, node.slice, node:Postfix AS labeled, node::NODE AS typed",
        ExecutionOptions::default(),
    )
    .expect("postfix expressions");
    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::List(vec![Value::Integer(1)]),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn nested_not_is_not_counted_again_by_its_parent_expression() {
    let connection = fresh_storage();

    let (rows, summary) = execute(
        &connection,
        "CREATE (node:Logic {flag: NOT (NOT true)}) RETURN node.flag",
        ExecutionOptions::default(),
    )
    .expect("nested NOT write expression");

    assert_eq!(rows, vec![vec![Value::Boolean(true)]]);
    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(summary.counters.properties_set, 1);
}

#[test]
fn outer_postfix_lookup_does_not_reapply_property_keys_from_its_parenthesized_base() {
    let connection = fresh_storage();
    let params = BTreeMap::from([(
        "payload".to_owned(),
        Value::Map(BTreeMap::from([(
            "outer".to_owned(),
            Value::Map(BTreeMap::from([("value".to_owned(), Value::Integer(7))])),
        )])),
    )]);

    let (rows, summary) = execute_with_params(
        &connection,
        "CREATE (node:Nested {value: ($payload.outer).value}) RETURN node.value",
        params,
        ExecutionOptions::default(),
    )
    .expect("parenthesized nested property lookup");

    assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    assert_eq!(summary.counters.properties_set, 1);
}

#[test]
fn chained_comparison_applies_every_bound_before_mutation() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Range {num:4}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed out-of-range Node");

    let (_, summary) = execute(
        &connection,
        "MATCH (node:Range) WHERE 1 < node.num < 3 DELETE node FINISH",
        ExecutionOptions::default(),
    )
    .expect("chained comparison mutation");

    assert_eq!(summary.counters.nodes_deleted, 0);
    assert_eq!(
        read_rows(&connection, "MATCH (node:Range) RETURN count(node)"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn write_count_expression_preserves_its_argument_and_null_semantics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Counted {present:1}), (:Counted) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed nullable count input");

    let (rows, summary) = execute(
        &connection,
        "MATCH (node:Counted) SET node.touched = true RETURN count(node.missing), count(node.present)",
        ExecutionOptions::default(),
    )
    .expect("write count expressions");

    assert_eq!(rows, vec![vec![Value::Integer(0), Value::Integer(1)]]);
    assert_eq!(summary.counters.properties_set, 2);
}

#[test]
fn aggregate_write_order_expressions_are_validated_before_commit() {
    let connection = fresh_storage();

    let (rows, summary) = execute(
        &connection,
        "CREATE (node:ValidAggregateOrder) RETURN count(node) AS total ORDER BY count(node), total + 1",
        ExecutionOptions::default(),
    )
    .expect("projected aggregate expressions and aliases are valid ORDER BY inputs");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(summary.counters.nodes_created, 1);

    let (rows, summary) = execute(
        &connection,
        "CREATE (:ValidAggregateFunction) RETURN count(*) AS total ORDER BY toString(1)",
        ExecutionOptions::default(),
    )
    .expect("constant function is a valid aggregate ORDER BY expression");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(summary.counters.nodes_created, 1);

    let before = branch_head(&connection, "main").expect("head before invalid aggregate order");
    let query =
        "CREATE (node:RejectedAggregateScope) RETURN count(node) AS total ORDER BY node.missing";
    let error = execute(&connection, query, ExecutionOptions::default())
        .expect_err("ungrouped aggregate ORDER BY must fail before commit");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("head after invalid aggregate order"),
        before
    );
    assert_eq!(
        read_rows(
            &connection,
            "MATCH (node:RejectedAggregateScope) RETURN count(node)"
        ),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn supported_write_functions_are_validated_even_when_the_projection_has_no_rows() {
    let connection = fresh_storage();
    let before = branch_head(&connection, "main").expect("head before zero-row write");
    let (rows, summary) = execute(
        &connection,
        "MATCH (:Missing) CREATE (:ZeroRowFunction) RETURN toString(1)",
        ExecutionOptions::default(),
    )
    .expect("supported function in a zero-row write projection");
    assert!(rows.is_empty());
    assert_eq!(summary.counters.nodes_created, 0);
    assert_ne!(
        branch_head(&connection, "main").expect("head after zero-row mutation intent"),
        before
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "ZeroRowFunction")
            .expect("find zero-row label")
            .is_none()
    );

    let before_explain = branch_head(&connection, "main").expect("head before EXPLAIN");
    execute(
        &connection,
        "EXPLAIN CREATE (:ExplainFunction) RETURN toString(1)",
        ExecutionOptions::default(),
    )
    .expect("supported function in mutating EXPLAIN");
    assert_eq!(
        branch_head(&connection, "main").expect("head after EXPLAIN"),
        before_explain
    );
    assert!(
        lithograph_core::storage::find_label(&connection, "ExplainFunction")
            .expect("find EXPLAIN label")
            .is_none()
    );
}

#[test]
fn supported_write_functions_preserve_arguments_and_propagate_null() {
    let connection = fresh_storage();

    let (rows, summary) = execute(
        &connection,
        "CREATE (node:FunctionProbe)-[relationship:FUNCTION_REL]->() RETURN elementId(node), labels(node), type(relationship), size(labels(node)), elementId(null), labels(null), type(null), size(null)",
        ExecutionOptions::default(),
    )
    .expect("supported structural functions and null propagation");

    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::String(ref value) if value.starts_with("n:")));
    assert_eq!(
        rows[0][1..],
        [
            Value::List(vec![Value::String("FunctionProbe".to_owned())]),
            Value::String("FUNCTION_REL".to_owned()),
            Value::Integer(1),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    );
    assert_eq!(summary.counters.nodes_created, 2);
    assert_eq!(summary.counters.relationships_created, 1);
}

#[test]
fn set_property_target_must_be_a_direct_or_parenthesized_variable() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Target) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed SET target");

    execute(
        &connection,
        "MATCH (node:Target) SET (node).supported = true FINISH",
        ExecutionOptions::default(),
    )
    .expect("a parenthesized direct variable is supported");
    let before = branch_head(&connection, "main").expect("head before computed SET target");

    let error = execute(
        &connection,
        "MATCH (node:Target) SET (CASE WHEN false THEN node ELSE null END).rejected = true FINISH",
        ExecutionOptions::default(),
    )
    .expect_err("Phase 05 must not reduce a computed SET target to its first variable");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("head after computed SET target"),
        before
    );
    assert_eq!(
        read_rows(
            &connection,
            "MATCH (node:Target) RETURN node.supported, node.rejected"
        ),
        vec![vec![Value::Boolean(true), Value::Null]]
    );
}

#[test]
fn distinct_order_by_cannot_reintroduce_a_removed_variable_after_write() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Group {name:'A', rank:100}), (:Group {name:'B', rank:50}), (:Group {name:'A', rank:1}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("seed duplicate projection groups");
    let before = branch_head(&connection, "main").expect("head before invalid ordering");
    let read_error = execute(
        &connection,
        "MATCH (node:Group) RETURN DISTINCT node.name AS name ORDER BY node.rank",
        ExecutionOptions::default(),
    )
    .expect_err("DISTINCT must remove the unprojected node variable");
    assert_eq!(read_error.kind, QueryErrorKind::Semantic);

    let write_error = execute(
        &connection,
        "MATCH (node:Group) SET node.touched = true RETURN DISTINCT node.name AS name ORDER BY node.rank",
        ExecutionOptions::default(),
    )
    .expect_err("an invalid write projection must fail before committing");
    assert_eq!(write_error.kind, QueryErrorKind::Semantic);
    assert_eq!(
        branch_head(&connection, "main").expect("head after invalid ordering"),
        before
    );

    assert_eq!(
        read_rows(
            &connection,
            "MATCH (node:Group) RETURN DISTINCT node.name AS name ORDER BY name"
        ),
        vec![
            vec![Value::String("A".to_owned())],
            vec![Value::String("B".to_owned())]
        ]
    );
}
