use super::*;

pub(super) fn build_logical(
    matches: &[MatchStep],
    sorted: bool,
    skipped: bool,
    limited: bool,
    distinct: bool,
    aggregate: bool,
) -> (LogicalPlan, Vec<ProfileBinding>) {
    let mut operators = Vec::new();
    let mut sites = Vec::new();
    let mut bound = BTreeSet::new();
    for step in matches {
        if step.optional {
            operators.push(LogicalOperator::Optional);
        }
        append_pattern_operators_with_sites(step, &mut bound, &mut operators, &mut sites);
        append_match_residual(step, &mut operators, &mut sites);
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
    (LogicalPlan { operators }, sites)
}

pub(crate) fn append_pattern_operators_with_sites(
    step: &MatchStep,
    bound: &mut BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
    sites: &mut Vec<ProfileBinding>,
) {
    for depth in 0..step.parts.len() {
        let (ordinal, part) = if step.join.is_some() {
            let (ordinal, part) = join::execution_part(step, depth);
            (ordinal, std::borrow::Cow::Owned(part))
        } else {
            (depth, std::borrow::Cow::Borrowed(&step.parts[depth]))
        };
        append_join_link(step, depth, ordinal, operators, sites);
        let start = operators.len();
        append_pattern_operator(&part, bound, operators);
        {
            let relationship_producer = part
                .relationship
                .as_ref()
                .is_some_and(|rel| rel.id_seek.is_some() || rel.index_seek.is_some());
            for (offset, operator) in operators[start..].iter().enumerate() {
                let role = if relationship_producer
                    || matches!(
                        operator,
                        LogicalOperator::ExpandAll { .. } | LogicalOperator::ExpandInto { .. }
                    ) {
                    ProfileRole::RelationshipAccess
                } else {
                    ProfileRole::NodeAccess
                };
                sites.push(ProfileBinding {
                    site: OperatorSite {
                        clause_start: step.clause_start,
                        part: Some(ordinal),
                        role,
                    },
                    physical_slot: start + offset,
                });
            }
        }
        let filtered = step.join.as_ref().map_or_else(
            || {
                step.early_predicates
                    .get(depth)
                    .is_some_and(|items| !items.is_empty())
            },
            |plan| !plan.stages[depth].predicates_after.is_empty(),
        );
        if filtered {
            {
                sites.push(ProfileBinding {
                    site: OperatorSite {
                        clause_start: step.clause_start,
                        part: Some(ordinal),
                        role: ProfileRole::EarlyFilter,
                    },
                    physical_slot: operators.len(),
                });
            }
            operators.push(LogicalOperator::Filter);
        }
    }
}

fn append_join_link(
    step: &MatchStep,
    depth: usize,
    ordinal: usize,
    operators: &mut Vec<LogicalOperator>,
    sites: &mut Vec<ProfileBinding>,
) {
    if depth == 0 {
        return;
    }
    let operator = match step.join.as_ref().map(|plan| &plan.stages[depth]) {
        Some(stage) if matches!(stage.access, join::StageAccess::Hash { .. }) => {
            LogicalOperator::HashJoin
        }
        Some(stage) if stage.cartesian => LogicalOperator::Cartesian,
        Some(stage) if matches!(stage.access, join::StageAccess::Probe(_)) => {
            LogicalOperator::IndexNestedLoop
        }
        Some(_) => LogicalOperator::NestedLoop,
        None => LogicalOperator::Cartesian,
    };
    {
        sites.push(ProfileBinding {
            site: OperatorSite {
                clause_start: step.clause_start,
                part: Some(ordinal),
                role: ProfileRole::Join,
            },
            physical_slot: operators.len(),
        });
    }
    operators.push(operator);
}

pub(crate) fn append_match_residual(
    step: &MatchStep,
    operators: &mut Vec<LogicalOperator>,
    sites: &mut Vec<ProfileBinding>,
) {
    if step.predicate.is_none() {
        return;
    }
    {
        sites.push(ProfileBinding {
            site: OperatorSite {
                clause_start: step.clause_start,
                part: None,
                role: ProfileRole::EarlyFilter,
            },
            physical_slot: operators.len(),
        });
    }
    operators.push(LogicalOperator::Filter);
}

fn append_pattern_operator(
    part: &PatternPart,
    bound: &mut BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
) {
    if append_relationship_id_seek(part, bound, operators)
        || append_relationship_index_seek(part, bound, operators)
    {
        return;
    }
    let start = part.start.variable.clone();
    if part.relationship.is_none() && start.as_ref().is_some_and(|name| bound.contains(name)) {
        operators.push(LogicalOperator::NodeIdSeek {
            variable: start.unwrap_or_default(),
        });
        return;
    }
    if start
        .as_ref()
        .is_none_or(|variable| !bound.contains(variable))
    {
        let variable = start.clone().unwrap_or_else(|| "_anon".to_owned());
        operators.push(if part.start.id_seek.is_some() {
            LogicalOperator::NodeIdSeek {
                variable: variable.clone(),
            }
        } else if let Some(seek) = &part.start.index_seek {
            LogicalOperator::IndexSeek {
                variable: variable.clone(),
                index: seek.index_name.clone(),
                kind: seek.kind,
            }
        } else {
            match &part.start.scan_label_name {
                Some(label) => LogicalOperator::LabelScan {
                    variable: variable.clone(),
                    label: label.clone(),
                },
                None => LogicalOperator::NodeScan {
                    variable: variable.clone(),
                },
            }
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

fn append_relationship_id_seek(
    part: &PatternPart,
    bound: &mut BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
) -> bool {
    let Some(relationship) = part.relationship.as_ref() else {
        return false;
    };
    if relationship.id_seek.is_none() {
        return false;
    }
    operators.push(LogicalOperator::RelationshipIdSeek {
        variable: relationship
            .variable
            .clone()
            .unwrap_or_else(|| "_anon_rel".to_owned()),
    });
    for variable in [
        part.start.variable.as_ref(),
        relationship.variable.as_ref(),
        part.end.as_ref().and_then(|end| end.variable.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        bound.insert(variable.clone());
    }
    true
}

fn append_relationship_index_seek(
    part: &PatternPart,
    bound: &mut BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
) -> bool {
    let Some(relationship) = &part.relationship else {
        return false;
    };
    let Some(seek) = &relationship.index_seek else {
        return false;
    };
    operators.push(LogicalOperator::IndexSeek {
        variable: relationship
            .variable
            .clone()
            .unwrap_or_else(|| "_anon_rel".to_owned()),
        index: seek.index_name.clone(),
        kind: seek.kind,
    });
    for variable in [
        part.start.variable.as_ref(),
        relationship.variable.as_ref(),
        part.end.as_ref().and_then(|end| end.variable.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        bound.insert(variable.clone());
    }
    true
}

pub(crate) fn build_physical(
    logical: &LogicalPlan,
    matches: &[MatchStep],
    sorted: bool,
) -> PhysicalPlan {
    let mut operators = Vec::new();
    let mut relationships = relationship_expansions(matches);
    for operator in &logical.operators {
        match operator {
            LogicalOperator::NodeScan { variable } => operators.push(PhysicalOperator::NodeScan {
                variable: variable.clone(),
            }),
            LogicalOperator::NodeIdSeek { variable } => {
                operators.push(PhysicalOperator::NodeIdSeek {
                    variable: variable.clone(),
                })
            }
            LogicalOperator::RelationshipIdSeek { variable } => {
                operators.push(PhysicalOperator::RelationshipIdSeek {
                    variable: variable.clone(),
                })
            }
            LogicalOperator::LabelScan { variable, label } => {
                operators.push(PhysicalOperator::LabelIndexScan {
                    variable: variable.clone(),
                    label: label.clone(),
                })
            }
            LogicalOperator::IndexSeek {
                variable,
                index,
                kind,
            } => {
                if *kind == StandardIndexKind::Vector {
                    operators.push(PhysicalOperator::VectorSearch {
                        variable: variable.clone(),
                        index: index.clone(),
                    });
                } else {
                    operators.push(PhysicalOperator::IndexSeek {
                        variable: variable.clone(),
                        index: index.clone(),
                        kind: *kind,
                    });
                }
            }
            LogicalOperator::ExpandAll { from, .. } | LogicalOperator::ExpandInto { from, .. } => {
                let (name, direction) = relationships.next().unwrap_or((None, Direction::Outgoing));
                operators.push(PhysicalOperator::AdjacencySeek {
                    from: from.clone(),
                    relationship_type: name.map(str::to_owned),
                    direction,
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
            LogicalOperator::NestedLoop => operators.push(PhysicalOperator::NestedLoop),
            LogicalOperator::IndexNestedLoop => operators.push(PhysicalOperator::IndexNestedLoop),
            LogicalOperator::HashJoin => operators.push(PhysicalOperator::HashJoin),
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
            LogicalOperator::Schema { kind } => {
                operators.push(PhysicalOperator::Schema { kind: *kind })
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
    ensure_sort(&mut operators, sorted);
    PhysicalPlan { operators }
}

fn ensure_sort(operators: &mut Vec<PhysicalOperator>, sorted: bool) {
    if sorted
        && !operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::ExternalSort))
    {
        operators.push(PhysicalOperator::ExternalSort);
    }
}

fn relationship_expansions(
    matches: &[MatchStep],
) -> impl Iterator<Item = (Option<&str>, Direction)> {
    matches.iter().flat_map(|step| {
        (0..step.parts.len()).filter_map(move |depth| join::expansion_metadata(step, depth))
    })
}
