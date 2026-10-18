use super::*;

fn set_secret(connection: &Connection, value: Value, options: &str) {
    execute_params(
        connection,
        "MATCH (n:Item) SET n.secret=$value FINISH",
        BTreeMap::from([("value".into(), value)]),
        options,
    )
    .expect("set canonical property");
}

fn secret(connection: &Connection, options: &str) -> Value {
    execute(connection, "MATCH (n:Item) RETURN n.secret", options)
        .expect("read canonical property")
        .0[0][0]
        .clone()
}

fn assert_same_value(actual: &Value, expected: &Value) {
    assert_eq!(
        lithograph_core::cypher::encode_json_text(actual),
        lithograph_core::cypher::encode_json_text(expected)
    );
}

fn patch(connection: &Connection, before: HashId, after: HashId) -> Value {
    super::patch::diff(connection, before, after)
}

fn operations(patch: &Value) -> &[Value] {
    let Value::Map(map) = patch else {
        panic!("Patch map")
    };
    let Value::List(operations) = &map["operations"] else {
        panic!("operations")
    };
    operations
}

fn apply(connection: &Connection, patch: Value) -> Result<QuerySummary, QueryError> {
    super::patch::apply(connection, patch, "{}")
}

fn float_values() -> Vec<(Value, Value)> {
    use lithograph_core::cypher::{PointValue, VectorCoordinateType, VectorValue, VectorValues};

    vec![
        (Value::Float(0.0), Value::Float(-0.0)),
        (
            Value::List(vec![Value::Float(0.0)]),
            Value::List(vec![Value::Float(-0.0)]),
        ),
        (
            Value::Point(PointValue::new("cartesian", vec![0.0, 1.0]).expect("point")),
            Value::Point(PointValue::new("cartesian", vec![-0.0, 1.0]).expect("point")),
        ),
        (
            Value::Vector(
                VectorValue::new(VectorCoordinateType::F32, VectorValues::F32(vec![0.0]))
                    .expect("F32 vector"),
            ),
            Value::Vector(
                VectorValue::new(VectorCoordinateType::F32, VectorValues::F32(vec![-0.0]))
                    .expect("F32 vector"),
            ),
        ),
        (
            Value::Vector(
                VectorValue::new(VectorCoordinateType::F64, VectorValues::F64(vec![0.0]))
                    .expect("F64 vector"),
            ),
            Value::Vector(
                VectorValue::new(VectorCoordinateType::F64, VectorValues::F64(vec![-0.0]))
                    .expect("F64 vector"),
            ),
        ),
    ]
}

#[test]
fn squash_preserves_protected_signed_zero_in_all_float_bearing_property_types() {
    for (positive, negative) in float_values() {
        let connection = fresh_storage();
        call(&connection, "CREATE (:Item) FINISH");
        set_secret(&connection, positive, "{}");
        let base = branch_head(&connection, "main").expect("positive base");
        set_secret(&connection, negative.clone(), "{}");
        let old = branch_head(&connection, "main").expect("negative head");
        execute(
            &connection,
            &format!(
                "CALL lithograph.squash('{}') YIELD commit RETURN commit",
                descriptor(base)
            ),
            r#"{"writeAccess":{"denyProperties":["secret"]}}"#,
        )
        .expect("Squash preserves protected state");
        assert_ne!(branch_head(&connection, "main").expect("squashed"), old);
        assert_same_value(&secret(&connection, "{}"), &negative);
        assert_same_value(
            &secret(&connection, &format!(r#"{{"at":"{}"}}"#, descriptor(old))),
            &negative,
        );
    }
}

#[test]
fn reverse_and_cross_branch_diff_preserve_signed_zero_and_ignore_unchanged_nan() {
    for (positive, negative) in float_values() {
        let connection = fresh_storage();
        call(
            &connection,
            "CREATE (:Item {unchanged:toFloat('NaN')}) FINISH",
        );
        set_secret(&connection, positive.clone(), "{}");
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "other", base);
        set_secret(&connection, negative, "{}");
        let changed = branch_head(&connection, "main").expect("changed");
        let inverse = patch(&connection, changed, base);
        assert_eq!(operations(&inverse).len(), 1, "only signed zero changed");
        apply(&connection, inverse).expect("reverse Patch applies with canonical NaN untouched");
        assert_same_value(&secret(&connection, "{}"), &positive);
        execute(
            &connection,
            "MATCH (n:Item) SET n.title='other' FINISH",
            r#"{"branch":"other"}"#,
        )
        .expect("divergence");
        let other = branch_head(&connection, "other").expect("other");
        let cross = patch(&connection, changed, other);
        assert_eq!(
            operations(&cross).len(),
            2,
            "zero and public title, no unchanged NaN"
        );
    }
}

#[test]
fn revert_of_public_change_succeeds_with_unchanged_nan_and_preserves_history() {
    for nan in [
        Value::Float(f64::NAN),
        Value::List(vec![Value::Float(f64::NAN)]),
    ] {
        let connection = fresh_storage();
        call(&connection, "CREATE (:Item {title:'base'}) FINISH");
        set_secret(&connection, nan.clone(), "{}");
        let base = branch_head(&connection, "main").expect("base");
        call(&connection, "MATCH (n:Item) SET n.title='public' FINISH");
        let changed = branch_head(&connection, "main").expect("changed");
        execute(
            &connection,
            &format!(
                "CALL lithograph.revert('{}') YIELD commit RETURN commit",
                descriptor(changed)
            ),
            r#"{"writeAccess":{"denyProperties":["secret"]}}"#,
        )
        .expect("public Revert must not touch canonical NaN");
        assert_same_value(&secret(&connection, "{}"), &nan);
        assert_eq!(
            call(&connection, "MATCH (n:Item) RETURN n.title"),
            vec![vec![Value::String("base".into())]]
        );
        assert!(
            operations(&patch(
                &connection,
                base,
                branch_head(&connection, "main").expect("head")
            ))
            .is_empty()
        );
    }
}

#[test]
fn patch_before_condition_matches_canonical_nan_and_distinguishes_signed_zero() {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {secret:toFloat('NaN')}) FINISH");
    let base = branch_head(&connection, "main").expect("NaN base");
    set_secret(&connection, Value::Float(1.0), "{}");
    let changed = branch_head(&connection, "main").expect("changed");
    let forward = patch(&connection, base, changed);
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(base)
        ),
    );
    apply(&connection, forward).expect("canonical NaN satisfies a legitimate before condition");
    for (positive, negative) in float_values() {
        set_secret(&connection, positive.clone(), "{}");
        let before = branch_head(&connection, "main").expect("positive");
        set_secret(&connection, Value::String("replacement".into()), "{}");
        let after = branch_head(&connection, "main").expect("after");
        let change = patch(&connection, before, after);
        set_secret(&connection, negative.clone(), "{}");
        let original = branch_head(&connection, "main").expect("negative");
        let commits = commit_count(&connection);
        let error = apply(&connection, change)
            .expect_err("numeric equality must not satisfy canonical before");
        assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
        assert_eq!(
            branch_head(&connection, "main").expect("unchanged"),
            original
        );
        assert_eq!(commit_count(&connection), commits);
        assert_same_value(&secret(&connection, "{}"), &negative);
    }
}

#[test]
fn rebase_replays_protected_signed_zero_and_unchanged_nan_without_false_conflicts() {
    for (positive, negative) in float_values() {
        let connection = fresh_storage();
        call(
            &connection,
            "CREATE (:Item {unchanged:toFloat('NaN')}) FINISH",
        );
        set_secret(&connection, positive, "{}");
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "topic", base);
        call(&connection, "MATCH (n:Item) SET n.title='public' FINISH");
        set_secret(&connection, negative.clone(), r#"{"branch":"topic"}"#);
        let rows = execute(
            &connection,
            "CALL lithograph.rebase('branch/main') YIELD status RETURN status",
            r#"{"branch":"topic","writeAccess":{"denyProperties":["secret","unchanged"]}}"#,
        )
        .expect("Rebase preserves protected topic properties")
        .0;
        assert_eq!(rows, vec![vec![Value::String("rebased".into())]]);
        assert_same_value(&secret(&connection, r#"{"at":"branch/topic"}"#), &negative);
    }
}

fn merge_start(connection: &Connection) -> (String, i64, String) {
    let rows = call(
        connection,
        "CALL lithograph.merge.start('branch/source') YIELD session,revision,status RETURN session,revision,status",
    );
    (
        string(&rows[0][0]).into(),
        integer(&rows[0][1]),
        string(&rows[0][2]).into(),
    )
}

fn finalize(connection: &Connection, session: &str, revision: i64) {
    call(
        connection,
        &format!(
            "CALL lithograph.merge.finalize('{session}',{revision}) YIELD status RETURN status"
        ),
    );
}

#[test]
fn sparse_and_full_merge_distinguish_signed_zero_and_auto_merge_canonical_nan() {
    for full in [false, true] {
        for (positive, negative) in float_values() {
            let connection = fresh_storage();
            call(
                &connection,
                "CREATE (:Item {unchanged:toFloat('NaN')}) FINISH",
            );
            set_secret(&connection, positive, "{}");
            let base = branch_head(&connection, "main").expect("base");
            branch(&connection, "source", base);
            call(
                &connection,
                if full {
                    "CREATE (:Extra) FINISH"
                } else {
                    "MATCH (n:Item) SET n.title='public' FINISH"
                },
            );
            set_secret(&connection, negative.clone(), r#"{"branch":"source"}"#);
            let (session, revision, status) = merge_start(&connection);
            assert_eq!(status, "ready", "unchanged NaN must not create conflict");
            finalize(&connection, &session, revision);
            assert_same_value(&secret(&connection, "{}"), &negative);
        }
    }
}

#[test]
fn explicit_merge_resolution_keeps_canonical_float_and_revision_semantics() {
    for replacement in [
        Value::Float(-0.0),
        Value::Float(f64::NAN),
        Value::List(vec![Value::Float(f64::NAN)]),
    ] {
        let connection = fresh_storage();
        call(&connection, "CREATE (:Item {secret:0.0}) FINISH");
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "source", base);
        set_secret(&connection, Value::Float(1.0), "{}");
        set_secret(&connection, Value::Float(2.0), r#"{"branch":"source"}"#);
        let (session, revision, status) = merge_start(&connection);
        assert_eq!(status, "conflicted");
        let conflict = call(
            &connection,
            &format!(
                "CALL lithograph.merge.conflicts('{session}',10) YIELD conflictId RETURN conflictId"
            ),
        );
        let query = format!(
            "CALL lithograph.merge.resolve('{session}',$revision,[{{conflictId:'{}',choice:'value',value:$value}}]) YIELD revision RETURN revision",
            string(&conflict[0][0])
        );
        let mut revision = revision;
        for expected in [revision + 1, revision + 1] {
            let rows = execute_params(
                &connection,
                &query,
                BTreeMap::from([
                    ("revision".into(), Value::Integer(revision)),
                    ("value".into(), replacement.clone()),
                ]),
                "{}",
            )
            .expect("resolve canonical replacement")
            .0;
            assert_eq!(
                rows,
                vec![vec![Value::Integer(expected)]],
                "identical NaN resolution must not increment revision twice"
            );
            revision = expected;
        }
        finalize(&connection, &session, revision);
        assert_same_value(&secret(&connection, "{}"), &replacement);
    }
}

#[test]
fn delete_modify_dependencies_use_canonical_node_and_relationship_property_states() {
    for relationship in [false, true] {
        for changed in [false, true] {
            let connection = fresh_storage();
            let initial = if changed { "0.0" } else { "toFloat('NaN')" };
            let create = if relationship {
                format!("CREATE (:Item)-[:LINK {{secret:{initial}}}]->(:Other) FINISH")
            } else {
                format!("CREATE (:Item {{secret:{initial}}}) FINISH")
            };
            call(&connection, &create);
            let base = branch_head(&connection, "main").expect("base");
            branch(&connection, "source", base);
            call(&connection, "MATCH (n:Item) DETACH DELETE n FINISH");
            let change = if changed {
                if relationship {
                    "MATCH ()-[r:LINK]->() SET r.secret=-0.0 FINISH"
                } else {
                    "MATCH (n:Item) SET n.secret=-0.0 FINISH"
                }
            } else {
                "CREATE (:Extra) FINISH"
            };
            execute(&connection, change, r#"{"branch":"source"}"#).expect("source change");
            let (session, revision, status) = merge_start(&connection);
            if changed {
                assert_eq!(
                    status, "conflicted",
                    "a canonical dependent property changed"
                );
                let slots = call(
                    &connection,
                    &format!(
                        "CALL lithograph.merge.conflicts('{session}',20) YIELD slot RETURN slot"
                    ),
                );
                assert!(slots.contains(&vec![Value::String("node/1".into())]));
            } else {
                assert_eq!(status, "ready", "unchanged NaN is not a modification");
                finalize(&connection, &session, revision);
                assert_eq!(
                    call(&connection, "MATCH (n:Item) RETURN count(n)"),
                    vec![vec![Value::Integer(0)]]
                );
            }
        }
    }
}

fn finish_ours_merge(connection: &Connection, source: HashId, options: &str) {
    let rows = execute(
        connection,
        &format!(
            "CALL lithograph.merge.start('{}') YIELD session,revision RETURN session,revision",
            descriptor(source)
        ),
        options,
    )
    .expect("start historical merge")
    .0;
    let session = string(&rows[0][0]);
    let mut revision = integer(&rows[0][1]);
    let conflicts = call(
        connection,
        &format!(
            "CALL lithograph.merge.conflicts('{session}',20) YIELD conflictId RETURN conflictId"
        ),
    );
    if !conflicts.is_empty() {
        let resolutions = Value::List(
            conflicts
                .iter()
                .map(|row| {
                    Value::Map(BTreeMap::from([
                        ("conflictId".into(), row[0].clone()),
                        ("choice".into(), Value::String("ours".into())),
                    ]))
                })
                .collect(),
        );
        let rows = execute_params(connection, &format!("CALL lithograph.merge.resolve('{session}',{revision},$resolutions) YIELD revision RETURN revision"), BTreeMap::from([("resolutions".into(), resolutions)]), "{}").expect("resolve ours").0;
        revision = integer(&rows[0][0]);
    }
    finalize(connection, session, revision);
}

#[test]
fn criss_cross_virtual_base_uses_canonical_nan_and_signed_zero_states() {
    for final_same in [false, true] {
        let connection = fresh_storage();
        call(
            &connection,
            "CREATE (:Item {secret:0.0, unchanged:toFloat('NaN')}) FINISH",
        );
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "source", base);
        set_secret(&connection, Value::Float(1.0), "{}");
        let ours = branch_head(&connection, "main").expect("ours");
        set_secret(&connection, Value::Float(2.0), r#"{"branch":"source"}"#);
        let theirs = branch_head(&connection, "source").expect("theirs");
        finish_ours_merge(&connection, theirs, "{}");
        finish_ours_merge(&connection, ours, r#"{"branch":"source"}"#);
        set_secret(
            &connection,
            if final_same {
                Value::Float(f64::NAN)
            } else {
                Value::Float(0.0)
            },
            "{}",
        );
        set_secret(
            &connection,
            if final_same {
                Value::Float(f64::NAN)
            } else {
                Value::Float(-0.0)
            },
            r#"{"branch":"source"}"#,
        );
        let (session, revision, status) = merge_start(&connection);
        if final_same {
            assert_eq!(status, "ready", "both sides have the same canonical NaN");
            finalize(&connection, &session, revision);
            assert_same_value(&secret(&connection, "{}"), &Value::Float(f64::NAN));
        } else {
            assert_eq!(
                status, "conflicted",
                "unknown virtual base cannot conflate signed zeros"
            );
            let slots = call(
                &connection,
                &format!("CALL lithograph.merge.conflicts('{session}',20) YIELD slot RETURN slot"),
            );
            assert_eq!(
                slots,
                vec![vec![Value::String("node/1/property/secret".into())]],
                "unchanged NaN is not another conflict"
            );
        }
    }
}

#[test]
fn signed_zero_resolution_change_invalidates_the_previously_validated_candidate_revision() {
    for (positive, negative) in float_values() {
        let connection = fresh_storage();
        call(&connection, "CREATE (:Item {secret:'base'}) FINISH");
        let base = branch_head(&connection, "main").expect("base");
        branch(&connection, "source", base);
        set_secret(&connection, Value::String("ours".into()), "{}");
        set_secret(
            &connection,
            Value::String("theirs".into()),
            r#"{"branch":"source"}"#,
        );
        let (session, revision, status) = merge_start(&connection);
        assert_eq!(status, "conflicted");
        let conflict = call(
            &connection,
            &format!(
                "CALL lithograph.merge.conflicts('{session}',10) YIELD conflictId RETURN conflictId"
            ),
        );
        let query = format!(
            "CALL lithograph.merge.resolve('{session}',$revision,[{{conflictId:'{}',choice:'value',value:$value}}]) YIELD revision RETURN revision",
            string(&conflict[0][0])
        );
        let resolve = |value, revision| {
            execute_params(
                &connection,
                &query,
                BTreeMap::from([
                    ("revision".into(), Value::Integer(revision)),
                    ("value".into(), value),
                ]),
                "{}",
            )
            .expect("resolve float state")
            .0
        };
        assert_eq!(
            resolve(positive.clone(), revision),
            vec![vec![Value::Integer(2)]]
        );
        let candidate = format!(r#"{{"mergeSession":{{"id":"{session}","revision":2}}}}"#);
        assert_same_value(&secret(&connection, &candidate), &positive);
        assert_eq!(resolve(negative.clone(), 2), vec![vec![Value::Integer(3)]]);
        let head = branch_head(&connection, "main").expect("before stale finalize");
        let count = commit_count(&connection);
        let stale = execute(
            &connection,
            &format!("CALL lithograph.merge.finalize('{session}',2) YIELD status RETURN status"),
            "{}",
        )
        .expect_err("old validation token is invalid");
        assert_eq!(stale.kind, QueryErrorKind::MergeSessionChanged);
        assert_eq!(branch_head(&connection, "main").expect("preserved"), head);
        assert_eq!(commit_count(&connection), count);
        let stale = execute(&connection, "MATCH (n:Item) RETURN n.secret", &candidate)
            .expect_err("old candidate token is invalid");
        assert_eq!(stale.kind, QueryErrorKind::MergeSessionChanged);
        finalize(&connection, &session, 3);
        assert_same_value(&secret(&connection, "{}"), &negative);
    }
}
