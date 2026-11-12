//! Narrowing access must not suppress an error/volatile call in a residual.
//! Type proof for a single key alone does not prove the whole WHERE total,
//! especially NULL AND an expression that throws.

use super::*;

pub(super) fn safe_candidates(
    step: &MatchStep,
    schema: &SchemaState,
    predicate: &Expr,
    evaluation: &SeekEvaluation<'_, '_>,
) -> Vec<(String, String, StandardIndexPredicate)> {
    let mut candidates = Vec::new();
    if collect_total(step, schema, predicate, evaluation, &mut candidates) {
        // Unindexed residuals participate in totality, but must not multiply
        // the selected indexes' candidate-copy workspace reservation.
        candidates.retain(|(variable, property, predicate)| {
            evaluation.allows_property(property)
                && property_scope(step, variable, evaluation).is_some_and(|scope| {
                    schema
                        .indexes
                        .values()
                        .take_while(|_| evaluation.check_work())
                        .any(|index| {
                            scoped_index_properties(index, scope)
                                .is_some_and(|properties| properties.contains(property))
                                && predicate_supported(
                                    schema, index, property, predicate, evaluation,
                                )
                        })
                })
        });
        candidates
    } else {
        Vec::new()
    }
}

fn collect_total(
    step: &MatchStep,
    schema: &SchemaState,
    predicate: &Expr,
    evaluation: &SeekEvaluation<'_, '_>,
    output: &mut Vec<(String, String, StandardIndexPredicate)>,
) -> bool {
    if !evaluation.check_work() {
        return false;
    }
    if let Expr::Binary(BinaryOp::And, left, right) = predicate {
        return collect_total(step, schema, left, evaluation, output)
            && collect_total(step, schema, right, evaluation, output);
    }
    let candidates = predicate_candidates(predicate, evaluation);
    if candidates.is_empty() {
        return total_without_candidate(step, predicate, evaluation);
    }
    if !candidates
        .iter()
        .all(|candidate| total_candidate(step, schema, candidate, evaluation))
    {
        return false;
    }
    output.extend(candidates);
    true
}

fn total_candidate(
    step: &MatchStep,
    schema: &SchemaState,
    candidate: &(String, String, StandardIndexPredicate),
    evaluation: &SeekEvaluation<'_, '_>,
) -> bool {
    let (variable, property, predicate) = candidate;
    let Some(scope) = property_scope(step, variable, evaluation) else {
        return false;
    };
    if !evaluation.allows_property(property) {
        // Query masking makes this binding's property NULL. Prove only that
        // NULL behavior, without consulting its canonical type or indexes;
        // another, public property may still supply a safe access path.
        return matches!(
            predicate,
            StandardIndexPredicate::Equal(_)
                | StandardIndexPredicate::In(_)
                | StandardIndexPredicate::Less(_, _)
                | StandardIndexPredicate::Greater(_, _)
                | StandardIndexPredicate::IsNotNull
                | StandardIndexPredicate::StartsWith(_)
                | StandardIndexPredicate::EndsWith(_)
                | StandardIndexPredicate::Contains(_)
                | StandardIndexPredicate::WithinBBox { .. }
                | StandardIndexPredicate::Distance { .. }
        );
    }
    // Prove the residual against the binding's versioned type constraints.
    // Whether that property has a usable physical index is a separate question
    // answered by choose_property_seek/predicate_supported after this proof.
    let proves = |accepts: &dyn Fn(&PropertyType) -> bool| {
        property_type_proves_scope(schema, scope, property, accepts, evaluation)
    };
    match predicate {
        StandardIndexPredicate::Equal(Value::Null)
        | StandardIndexPredicate::IsNotNull
        | StandardIndexPredicate::Less(Value::Null, _)
        | StandardIndexPredicate::Greater(Value::Null, _) => true,
        StandardIndexPredicate::Equal(value) => proves(&|property_type| {
            property_type_equality_compatible(property_type, value, evaluation)
        }),
        StandardIndexPredicate::In(values) => {
            let compatible = |property_type: &PropertyType| {
                values.iter().all(|value| {
                    evaluation.check_work()
                        && (matches!(value, Value::Null)
                            || property_type_equality_compatible(property_type, value, evaluation))
                })
            };
            values
                .iter()
                .all(|value| evaluation.check_work() && matches!(value, Value::Null))
                || proves(&compatible)
        }
        StandardIndexPredicate::Less(value, _) | StandardIndexPredicate::Greater(value, _) => {
            proves(&|property_type| property_type_order_compatible(property_type, value))
        }
        // String predicates return NULL for other property families. That is
        // total even when a Text/Range seek cannot cover those families.
        StandardIndexPredicate::StartsWith(_)
        | StandardIndexPredicate::EndsWith(_)
        | StandardIndexPredicate::Contains(_) => true,
        StandardIndexPredicate::WithinBBox {
            lower: Value::Point(_),
            upper: Value::Point(_),
        }
        | StandardIndexPredicate::Distance {
            center: Value::Point(_),
            radius: Value::Integer(_) | Value::Float(_),
            ..
        } => proves(&|property_type| matches!(property_type, PropertyType::Point)),
        _ => false,
    }
}

fn total_without_candidate(
    step: &MatchStep,
    predicate: &Expr,
    evaluation: &SeekEvaluation<'_, '_>,
) -> bool {
    match predicate {
        Expr::Literal(_) | Expr::Parameter(_) => matches!(
            evaluation.value(predicate),
            Some(Value::Boolean(_) | Value::Null)
        ),
        Expr::IsNull { value, .. } => property_name(value)
            .is_some_and(|(variable, _)| property_scope(step, &variable, evaluation).is_some()),
        Expr::Binary(BinaryOp::In, left, right) => {
            property_name(left).is_some_and(|(variable, _)| {
                property_scope(step, &variable, evaluation).is_some()
                    && matches!(evaluation.value(right), Some(Value::Null))
            })
        }
        _ => false,
    }
}

fn property_scope<'a>(
    step: &'a MatchStep,
    variable: &str,
    evaluation: &SeekEvaluation<'_, '_>,
) -> Option<PropertySeekScope<'a>> {
    let mut scope = None;
    for part in &step.parts {
        if !evaluation.check_work() || part.path_variable.as_deref() == Some(variable) {
            return None;
        }
        for node in std::iter::once(&part.start).chain(part.end.iter()) {
            if node.variable.as_deref() == Some(variable)
                && scope
                    .replace(PropertySeekScope::Node(&node.label_names))
                    .is_some()
            {
                return None;
            }
        }
        if let Some(relationship) = &part.relationship
            && relationship.variable.as_deref() == Some(variable)
            && scope
                .replace(PropertySeekScope::Relationship(
                    relationship.type_name.as_deref(),
                ))
                .is_some()
        {
            return None;
        }
    }
    scope
}

fn property_type_proves_scope(
    schema: &SchemaState,
    scope: PropertySeekScope<'_>,
    property: &str,
    accepts: &dyn Fn(&PropertyType) -> bool,
    evaluation: &SeekEvaluation<'_, '_>,
) -> bool {
    schema.constraints.values().take_while(|_| evaluation.check_work()).any(|constraint| {
        constraint.properties.as_slice() == [property]
            && match (&constraint.target, scope) {
                (SchemaTarget::Node { label }, PropertySeekScope::Node(labels)) => {
                    labels.contains(label)
                }
                (
                    SchemaTarget::Relationship { relationship_type },
                    PropertySeekScope::Relationship(Some(name)),
                ) => relationship_type == name,
                _ => false,
            }
            && matches!(
                &constraint.kind,
                ConstraintDefinitionKind::Type { rule }
                    if property_type_all_members_match(&rule.property_type, &accepts, evaluation)
            )
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::query::plan::PlanningBudget;

    #[test]
    fn cancellation_inside_repeated_constraint_in_proof_is_not_budget_fallback() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
        crate::storage::create_storage_schema(&db).unwrap();
        let root = crate::storage::initialize_root(&db).unwrap().root;
        let labels = (0..64)
            .map(|index| format!("Probe{index}"))
            .collect::<Vec<_>>();
        for label in &labels {
            crate::storage::intern_label(&db, label).unwrap();
        }
        let prepared = crate::query::prepare(
            &db,
            &format!("MATCH (n:{}) WHERE n.values=1 RETURN n", labels.join(":")),
            BTreeMap::new(),
            crate::query::ExecutionOptions::default(),
        )
        .unwrap();
        let step = &prepared.matches[0];
        let snapshot = Snapshot::resolve(&db, root).unwrap();
        let mut schema = SchemaState::default();
        for (index, label) in labels.iter().enumerate() {
            let name = format!("values_{index}");
            schema.constraints.insert(
                name.clone(),
                crate::storage::ConstraintDefinition {
                    name,
                    target: SchemaTarget::Node {
                        label: label.clone(),
                    },
                    properties: vec!["values".to_owned()],
                    kind: ConstraintDefinitionKind::Type {
                        rule: crate::storage::PropertyRule {
                            property_type: PropertyType::Integer,
                            required: false,
                        },
                    },
                    origin: None,
                },
            );
        }
        let mut values = (0..64).map(Value::Integer).collect::<Vec<_>>();
        values.push(Value::String("incompatible last member".to_owned()));
        let candidate = (
            "n".to_owned(),
            "values".to_owned(),
            StandardIndexPredicate::In(values),
        );
        let resources = crate::query::QueryResources::default();
        let calls = Cell::new(0);
        let interrupted = || {
            calls.set(calls.get() + 1);
            calls.get() >= 4
        };
        let control = ExecutionControl::new(&resources, &interrupted);
        let params = BTreeMap::new();
        let access = PropertyRules::default();
        let mut budget = PlanningBudget::default();
        let evaluation =
            SeekEvaluation::new(&snapshot, &params, &access, &control, &mut budget).unwrap();
        assert!(!total_candidate(step, &schema, &candidate, &evaluation));
        assert!(!evaluation.exhausted());
        assert_eq!(
            evaluation.check().unwrap_err().kind,
            crate::query::QueryErrorKind::Interrupted
        );
        assert_eq!(calls.get(), 4);
        drop(evaluation);
        assert_eq!(resources.used(), 0);
    }
}
