use super::*;

pub(super) fn build_logical(
    matches: &[MatchStep],
    sorted: bool,
    skipped: bool,
    limited: bool,
    distinct: bool,
    aggregate: bool,
) -> LogicalPlan {
    let mut operators = Vec::new();
    let mut bound = BTreeSet::new();
    for step in matches {
        if step.optional {
            operators.push(LogicalOperator::Optional);
        }
        append_pattern_operators(step, &mut bound, &mut operators);
        if step.predicate.is_some() {
            operators.push(LogicalOperator::Filter);
        }
    }
    if aggregate {
        operators.push(LogicalOperator::Aggregate);
    }
    if distinct {
        operators.push(LogicalOperator::Distinct);
    }
    operators.push(LogicalOperator::Project);
    if sorted {
        operators.push(LogicalOperator::Sort);
    }
    if skipped {
        operators.push(LogicalOperator::Skip);
    }
    if limited {
        operators.push(LogicalOperator::Limit);
    }
    LogicalPlan { operators }
}

pub(crate) fn append_pattern_operators(
    step: &MatchStep,
    bound: &mut BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
) {
    for (index, part) in step.parts.iter().enumerate() {
        if index > 0 {
            operators.push(LogicalOperator::Cartesian);
        }
        append_pattern_operator(part, bound, operators);
    }
}

fn append_pattern_operator(
    part: &PatternPart,
    bound: &mut BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
) {
    let start = part.start.variable.clone();
    if start
        .as_ref()
        .is_none_or(|variable| !bound.contains(variable))
    {
        let variable = start.clone().unwrap_or_else(|| "_anon".to_owned());
        operators.push(match &part.start.scan_label_name {
            Some(label) => LogicalOperator::LabelScan {
                variable: variable.clone(),
                label: label.clone(),
            },
            None => LogicalOperator::NodeScan {
                variable: variable.clone(),
            },
        });
        if part.start.variable.is_some() {
            bound.insert(variable);
        }
    }
    let (Some(rel), Some(end)) = (&part.relationship, &part.end) else {
        return;
    };
    let start = start.unwrap_or_else(|| "_anon".to_owned());
    let target = end.variable.clone();
    operators.push(
        if target
            .as_ref()
            .is_some_and(|variable| bound.contains(variable))
        {
            LogicalOperator::ExpandInto {
                from: start,
                relationship: rel.variable.clone(),
                to: target.clone().unwrap_or_default(),
            }
        } else {
            LogicalOperator::ExpandAll {
                from: start,
                relationship: rel.variable.clone(),
                to: target.clone().unwrap_or_else(|| "_anon_target".to_owned()),
            }
        },
    );
    if let Some(target) = target {
        bound.insert(target);
    }
}

pub(super) fn build_physical(
    logical: &LogicalPlan,
    matches: &[MatchStep],
    sorted: bool,
) -> PhysicalPlan {
    let mut operators = Vec::new();
    let mut relationships = matches
        .iter()
        .flat_map(|step| &step.parts)
        .filter_map(|part| part.relationship.as_ref());
    for operator in &logical.operators {
        match operator {
            LogicalOperator::NodeScan { variable } => operators.push(PhysicalOperator::NodeScan {
                variable: variable.clone(),
            }),
            LogicalOperator::LabelScan { variable, label } => {
                operators.push(PhysicalOperator::LabelIndexScan {
                    variable: variable.clone(),
                    label: label.clone(),
                })
            }
            LogicalOperator::ExpandAll { from, .. } | LogicalOperator::ExpandInto { from, .. } => {
                let relationship = relationships.next();
                operators.push(PhysicalOperator::AdjacencySeek {
                    from: from.clone(),
                    relationship_type: relationship.and_then(|rel| rel.type_name.clone()),
                    direction: relationship.map_or(Direction::Outgoing, |rel| rel.direction),
                });
            }
            LogicalOperator::RelationshipScan { variable } => {
                operators.push(PhysicalOperator::RelationshipScan {
                    variable: variable.clone(),
                })
            }
            LogicalOperator::Filter => operators.push(PhysicalOperator::Filter),
            LogicalOperator::Project => operators.push(PhysicalOperator::Project),
            LogicalOperator::Sort => operators.push(PhysicalOperator::ExternalSort),
            LogicalOperator::Skip => operators.push(PhysicalOperator::Skip),
            LogicalOperator::Limit => operators.push(PhysicalOperator::Limit),
            LogicalOperator::Aggregate => operators.push(PhysicalOperator::Aggregate),
            LogicalOperator::Distinct => operators.push(PhysicalOperator::Distinct),
            LogicalOperator::Optional => operators.push(PhysicalOperator::Optional),
            LogicalOperator::Cartesian => operators.push(PhysicalOperator::Cartesian),
            LogicalOperator::Let => operators.push(PhysicalOperator::Let),
            LogicalOperator::Unwind => operators.push(PhysicalOperator::Unwind),
            LogicalOperator::Union { distinct } => operators.push(PhysicalOperator::Union {
                distinct: *distinct,
            }),
            LogicalOperator::Subquery => operators.push(PhysicalOperator::Subquery),
            LogicalOperator::When => operators.push(PhysicalOperator::When),
            LogicalOperator::Next => operators.push(PhysicalOperator::Next),
            LogicalOperator::Eager => operators.push(PhysicalOperator::Eager),
            LogicalOperator::Mutation { kind } => {
                operators.push(PhysicalOperator::Mutation { kind: *kind })
            }
            LogicalOperator::Commit => operators.push(PhysicalOperator::Commit),
            LogicalOperator::TypeSeek { relationship_type } => {
                operators.push(PhysicalOperator::AdjacencySeek {
                    from: String::new(),
                    relationship_type: Some(relationship_type.clone()),
                    direction: Direction::Outgoing,
                })
            }
        }
    }
    if sorted
        && !operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::ExternalSort))
    {
        operators.push(PhysicalOperator::ExternalSort);
    }
    PhysicalPlan { operators }
}
