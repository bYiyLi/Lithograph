//! Compile fixed MATCH clauses to the same MatchStep consumed by all cursors.
//! After a write or transaction/version boundary only stable identity structure
//! may describe fresh runtime access; no base-Snapshot cost/type proof is reused.

use super::*;
use crate::query::plan::{OperatorSite, ProfileBinding, ProfileRole};
use crate::query::resources::{ExecutionControl, MemoryReservation};
use crate::storage::Snapshot;

pub(super) fn append_program_match_operators(
    clause: &AstNode,
    bound: &mut std::collections::BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
    sites: &mut Vec<ProfileBinding>,
) {
    if clause.kind == AstKind::Clause(ClauseKind::OptionalMatch) {
        operators.push(LogicalOperator::Optional);
    }
    if let Some(search) = clause
        .descendants()
        .find(|node| node.kind == AstKind::Search)
    {
        let variable = decoded_descendant_name(search, AstKind::Variable)
            .unwrap_or_else(|| "_search".to_owned());
        let index = decoded_descendant_name(search, AstKind::IndexName)
            .unwrap_or_else(|| "_vector_index".to_owned());
        push_program_access(
            LogicalOperator::IndexSeek {
                variable,
                index,
                kind: crate::storage::StandardIndexKind::Vector,
            },
            OperatorSite {
                clause_start: clause.span.start,
                part: Some(0),
                role: ProfileRole::NodeAccess,
            },
            operators,
            sites,
        );
    }
    for (part_id, part) in clause
        .descendants()
        .filter(|node| node.kind == AstKind::PatternPart)
        .enumerate()
    {
        append_original_part(clause.span.start, part_id, part, bound, operators, sites);
    }
    if clause.descendants().any(|node| node.kind == AstKind::Where) {
        push_program_access(
            LogicalOperator::Filter,
            OperatorSite {
                clause_start: clause.span.start,
                part: None,
                role: ProfileRole::EarlyFilter,
            },
            operators,
            sites,
        );
    }
}

fn append_original_part(
    clause: usize,
    part_id: usize,
    part: &AstNode,
    bound: &mut std::collections::BTreeSet<String>,
    operators: &mut Vec<LogicalOperator>,
    sites: &mut Vec<ProfileBinding>,
) {
    let nodes = part
        .descendants()
        .filter(|node| node.kind == AstKind::NodePattern)
        .collect::<Vec<_>>();
    let Some(start) = nodes.first() else { return };
    let variable = pattern_variable(start, "_anon");
    let named = start
        .descendants()
        .find(|node| node.kind == AstKind::PatternVariable)
        .and_then(|node| node.text.as_ref());
    let bound_start = named.is_some_and(|name| bound.contains(name));
    let relationships = part
        .descendants()
        .filter(|node| node.kind == AstKind::RelationshipPattern);
    let has_relationships = part
        .descendants()
        .any(|node| node.kind == AstKind::RelationshipPattern);
    if !bound_start || !has_relationships {
        let operator = original_node_operator(start, variable, bound_start);
        push_program_access(
            operator,
            OperatorSite {
                clause_start: clause,
                part: Some(part_id),
                role: ProfileRole::NodeAccess,
            },
            operators,
            sites,
        );
    }
    for (index, _) in relationships.enumerate() {
        push_program_access(
            LogicalOperator::ExpandAll {
                from: pattern_variable(nodes[index], "_anon"),
                relationship: None,
                to: pattern_variable(nodes[index + 1], "_anon_target"),
            },
            OperatorSite {
                clause_start: clause,
                part: Some(part_id),
                role: ProfileRole::RelationshipAccess,
            },
            operators,
            sites,
        );
    }
    for name in part
        .descendants()
        .filter(|node| {
            matches!(
                node.kind,
                AstKind::PatternVariable | AstKind::RelationshipVariable
            )
        })
        .filter_map(|node| node.text.as_ref())
    {
        bound.insert(name.clone());
    }
}

fn original_node_operator(node: &AstNode, variable: String, bound: bool) -> LogicalOperator {
    if bound {
        return LogicalOperator::NodeIdSeek { variable };
    }
    match node
        .descendants()
        .find(|node| node.kind == AstKind::LabelName)
        .and_then(|node| node.text.as_deref())
    {
        Some(label) => LogicalOperator::LabelScan {
            variable,
            label: crate::cypher::unescape_identifier(label),
        },
        None => LogicalOperator::NodeScan { variable },
    }
}

fn push_program_access(
    operator: LogicalOperator,
    site: OperatorSite,
    operators: &mut Vec<LogicalOperator>,
    sites: &mut Vec<ProfileBinding>,
) {
    sites.push(ProfileBinding {
        site,
        physical_slot: operators.len(),
    });
    operators.push(operator);
}

impl PreparedProgram {
    pub(crate) fn plan_fixed_matches(
        &mut self,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        params: &BTreeMap<String, Value>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        let mut memory = control.reserve(0)?;
        memory.merge(self.prepare_write_pipeline(snapshot, params, control)?)?;
        let Some(single) = Self::fixed_match_query(&self.root) else {
            return Ok(memory);
        };
        let cost_streamable = self.cost_matches_streamable()?;
        // Do not infer a LIMIT's upstream demand through arbitrary WITH,
        // projections or writes. Only complete-consumer costs are proved here.
        let complete_consumer = !single.descendants().any(|node| node.kind == AstKind::Limit);
        let mut budget = super::super::plan::PlanningBudget::default();
        let mut identity_display = BTreeMap::new();
        let mut changed = false;
        let mut columns = Vec::new();
        for node in &single.children {
            control.work(1)?;
            changed |= is_write(node);
            if budget.exhausted() {
                break;
            }
            if self.match_plans.len() + identity_display.len() >= 128 {
                budget.stop();
                break;
            }
            let incoming = columns;
            let Ok(next) = infer_clause_columns(node, &incoming, &self.source) else {
                budget.stop();
                break;
            };
            columns = next;
            let AstKind::Clause(ClauseKind::Match | ClauseKind::OptionalMatch) = node.kind else {
                continue;
            };
            if changed || !cost_streamable {
                plan_identity_display(
                    &mut identity_display,
                    snapshot,
                    node,
                    params,
                    &incoming,
                    &mut budget,
                )?;
                continue;
            }
            memory.merge(Self::plan_fixed_match(
                &mut self.match_plans,
                snapshot,
                view,
                params,
                control,
                &mut budget,
                (node, &incoming, &columns, complete_consumer),
            )?)?;
        }
        self.install_identity_permissions(identity_display, control, &mut memory)?;
        Ok(memory)
    }

    fn fixed_match_query(root: &AstNode) -> Option<&AstNode> {
        let mut queries = root
            .descendants()
            .filter(|node| node.kind == AstKind::SingleQuery);
        let single = queries.next()?;
        if queries.next().is_some()
            || root
                .descendants()
                .any(|node| node.kind == AstKind::Clause(ClauseKind::Call))
        {
            None
        } else {
            Some(single)
        }
    }

    fn install_identity_permissions(
        &mut self,
        mut identity_display: BTreeMap<usize, super::super::plan::MatchStep>,
        control: &ExecutionControl<'_>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        // Only the display refresh may see these base-lowered structural
        // descriptions. Remove them on both success and error, so read-stream
        // builders can never reuse old impossible/LabelId/type state after a
        // write. Runtime identity access always lowers the current clause anew.
        let keys_memory = match control.reserve(crate::query::resources::allocation_bytes::<usize>(
            identity_display.len(),
        )?) {
            Ok(memory) => memory,
            Err(error) if error.kind == super::super::QueryErrorKind::Resource => {
                // Frontend/plan descriptions use PlanningBudget, not the
                // executor quota. Only this retained runtime permission vector
                // needs a lease; inability to keep it disables the optional
                // identities instead of introducing a preparation error.
                identity_display.clear();
                control.reserve(0)?
            }
            Err(error) => return Err(error),
        };
        let keys = identity_display.keys().copied().collect::<Vec<_>>();
        self.match_plans.append(&mut identity_display);
        let refreshed = self.refresh_match_plan(control, memory.bytes());
        for key in &keys {
            self.match_plans.remove(key);
        }
        refreshed?;
        self.identity_match_spans = keys;
        if let Some(write) = self.streamed_write.as_mut() {
            write.constrain_fresh_identity_matches(&self.identity_match_spans);
        }
        memory.merge(keys_memory)?;
        Ok(())
    }

    fn plan_fixed_match(
        match_plans: &mut BTreeMap<usize, super::super::plan::MatchStep>,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        params: &BTreeMap<String, Value>,
        control: &ExecutionControl<'_>,
        budget: &mut super::super::plan::PlanningBudget,
        clause_scope: (&AstNode, &[String], &[String], bool),
    ) -> QueryResult<MemoryReservation> {
        let (clause, incoming, declared, complete_consumer) = clause_scope;
        let Some(mut step) =
            execute::identity::lower_pattern(snapshot.connection_for_query(), clause)?
        else {
            return control.reserve(0);
        };
        if !super::super::plan::constrain_match_scope(&mut step, incoming) {
            return control.reserve(0);
        }
        let (_, selected) = super::super::plan::plan_access(
            snapshot,
            view,
            std::slice::from_mut(&mut step),
            params,
            control,
            budget,
            Some(&super::super::plan::join::JoinClauseScope {
                complete_consumer,
                clause_start: clause.span.start,
                snapshot_identity: snapshot.cache_identity(),
                incoming,
                declared_columns: declared,
            }),
        )?;
        if budget.exhausted() && step.join.is_none() {
            return control.reserve(0);
        }
        match_plans.insert(clause.span.start, step);
        Ok(selected)
    }

    fn prepare_write_pipeline(
        &mut self,
        snapshot: &Snapshot<'_>,
        params: &BTreeMap<String, Value>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        let mut memory = control.reserve(0)?;
        if self.writes
            && !self.version_operation
            && self.transaction_options.is_none()
            && let Some((write, selected)) =
                super::super::mutation::prepare_streamed_write(snapshot, self, params, control)?
        {
            self.streamed_write = Some(write);
            memory.merge(selected)?;
        }
        Ok(memory)
    }

    fn cost_matches_streamable(&self) -> QueryResult<bool> {
        if Self::fixed_match_query(&self.root).is_none() {
            return Ok(false);
        }
        if self.writes {
            Ok(self.streamed_write.is_some())
        } else {
            Ok(super::super::stream::program::supports_read_stream(self))
        }
    }

    fn refresh_match_plan(
        &mut self,
        control: &ExecutionControl<'_>,
        bytes: u64,
    ) -> QueryResult<()> {
        let (operators, sites) =
            logical_operators_with_sites(&self.root, &self.match_plans, &self.source)?;
        self.logical.operators = operators;
        self.profile_sites = sites;
        let _copy_memory = control.reserve(bytes)?;
        let matches = self.match_plans.values().cloned().collect::<Vec<_>>();
        if self
            .root
            .descendants()
            .filter(|node| {
                matches!(
                    node.kind,
                    AstKind::Clause(ClauseKind::Match | ClauseKind::OptionalMatch)
                )
            })
            .count()
            == matches.len()
        {
            self.physical = super::super::plan::build_physical(&self.logical, &matches, false);
        } else {
            self.physical.operators = self
                .logical
                .operators
                .iter()
                .map(physical_operator)
                .collect();
        }
        bind_program_relationships(
            &self.root,
            &self.match_plans,
            &self.logical.operators,
            &mut self.physical.operators,
        );
        Ok(())
    }
}

fn plan_identity_display(
    display: &mut BTreeMap<usize, super::super::plan::MatchStep>,
    snapshot: &Snapshot<'_>,
    clause: &AstNode,
    params: &BTreeMap<String, Value>,
    incoming: &[String],
    budget: &mut super::super::plan::PlanningBudget,
) -> QueryResult<()> {
    let Some((states, bytes)) = execute::identity::identity_clause_size(clause) else {
        return Ok(());
    };
    // Reserve the structural description and display copy in the planner's
    // existing ledger before lowering. This is not a Schema/type proof.
    if !budget.work(states, bytes.saturating_mul(2)) {
        return Ok(());
    }
    if let Some(step) = execute::identity::lower_identity_match(
        snapshot.connection_for_query(),
        clause,
        params,
        incoming,
    )? {
        display.insert(clause.span.start, step);
    }
    Ok(())
}

fn is_write(node: &AstNode) -> bool {
    matches!(
        node.kind,
        AstKind::Clause(
            ClauseKind::Create
                | ClauseKind::Insert
                | ClauseKind::Merge
                | ClauseKind::Set
                | ClauseKind::Remove
                | ClauseKind::Delete
                | ClauseKind::DetachDelete
                | ClauseKind::Foreach
        )
    )
}

/// Even when optional optimization exhausted its budget, the original AST
/// still describes the fallback's Type and direction. Keep its expansion
/// metadata in the same order as the actual planned or original parts.
pub(super) fn bind_program_relationships(
    root: &AstNode,
    matches: &BTreeMap<usize, super::super::plan::MatchStep>,
    logical: &[LogicalOperator],
    physical: &mut [PhysicalOperator],
) {
    let mut descriptions = root
        .descendants()
        .filter(|node| {
            matches!(
                node.kind,
                AstKind::Clause(ClauseKind::Match | ClauseKind::OptionalMatch)
            )
        })
        .flat_map(|clause| {
            let planned = matches.get(&clause.span.start);
            let selected = planned
                .into_iter()
                .flat_map(|step| {
                    (0..step.parts.len()).filter_map(move |depth| {
                        crate::query::plan::join::expansion_metadata(step, depth)
                    })
                })
                .map(|(name, direction)| (name, false, direction));
            let fallback = clause
                .descendants()
                .filter(move |node| planned.is_none() && node.kind == AstKind::PatternPart)
                .flat_map(|part| {
                    part.descendants()
                        .filter(|node| node.kind == AstKind::RelationshipPattern)
                })
                .map(|node| {
                    let (name, direction) = relationship_description(node);
                    (name, true, direction)
                });
            selected.chain(fallback)
        });
    for (logical, physical) in logical.iter().zip(physical) {
        if matches!(
            logical,
            LogicalOperator::ExpandAll { .. } | LogicalOperator::ExpandInto { .. }
        ) && let Some((name, encoded, selected)) = descriptions.next()
            && let PhysicalOperator::AdjacencySeek {
                relationship_type,
                direction,
                ..
            } = physical
        {
            *relationship_type = name.map(|name| {
                if encoded {
                    crate::cypher::unescape_identifier(name)
                } else {
                    name.to_owned()
                }
            });
            *direction = selected;
        }
    }
}

fn relationship_description(node: &AstNode) -> (Option<&str>, crate::query::plan::Direction) {
    use crate::query::plan::Direction;
    let mut names = node
        .descendants()
        .filter(|node| node.kind == AstKind::RelationshipTypeName)
        .filter_map(|node| node.text.as_deref());
    let first = names.next();
    let name = if names.next().is_none() { first } else { None };
    let left = node
        .descendants()
        .any(|node| node.kind == AstKind::RelationshipLeftArrow);
    let right = node
        .descendants()
        .any(|node| node.kind == AstKind::RelationshipRightArrow);
    let direction = match (left, right) {
        (false, true) => Direction::Outgoing,
        (true, false) => Direction::Incoming,
        _ => Direction::Undirected,
    };
    (name, direction)
}

pub(super) fn physical_operator(operator: &LogicalOperator) -> PhysicalOperator {
    match operator {
        LogicalOperator::Union { distinct } => PhysicalOperator::Union {
            distinct: *distinct,
        },
        LogicalOperator::Next => PhysicalOperator::Next,
        LogicalOperator::When => PhysicalOperator::When,
        LogicalOperator::Let => PhysicalOperator::Let,
        LogicalOperator::Unwind => PhysicalOperator::Unwind,
        LogicalOperator::Subquery => PhysicalOperator::Subquery,
        LogicalOperator::Aggregate => PhysicalOperator::Aggregate,
        LogicalOperator::NodeScan { variable } => PhysicalOperator::NodeScan {
            variable: variable.clone(),
        },
        LogicalOperator::NodeIdSeek { variable } => PhysicalOperator::NodeIdSeek {
            variable: variable.clone(),
        },
        LogicalOperator::RelationshipIdSeek { variable } => PhysicalOperator::RelationshipIdSeek {
            variable: variable.clone(),
        },
        LogicalOperator::LabelScan { variable, label } => PhysicalOperator::LabelIndexScan {
            variable: variable.clone(),
            label: label.clone(),
        },
        LogicalOperator::IndexSeek {
            variable,
            index,
            kind,
        } if *kind == crate::storage::StandardIndexKind::Vector => PhysicalOperator::VectorSearch {
            variable: variable.clone(),
            index: index.clone(),
        },
        LogicalOperator::IndexSeek {
            variable,
            index,
            kind,
        } => PhysicalOperator::IndexSeek {
            variable: variable.clone(),
            index: index.clone(),
            kind: *kind,
        },
        LogicalOperator::ExpandAll { from, .. } | LogicalOperator::ExpandInto { from, .. } => {
            PhysicalOperator::AdjacencySeek {
                from: from.clone(),
                relationship_type: None,
                direction: crate::query::plan::Direction::Outgoing,
            }
        }
        LogicalOperator::Filter => PhysicalOperator::Filter,
        LogicalOperator::Distinct => PhysicalOperator::Distinct,
        LogicalOperator::Project => PhysicalOperator::Project,
        LogicalOperator::Sort => PhysicalOperator::ExternalSort,
        LogicalOperator::Skip => PhysicalOperator::Skip,
        LogicalOperator::Limit => PhysicalOperator::Limit,
        LogicalOperator::Eager => PhysicalOperator::Eager,
        LogicalOperator::Optional => PhysicalOperator::Optional,
        LogicalOperator::Cartesian => PhysicalOperator::Cartesian,
        LogicalOperator::NestedLoop => PhysicalOperator::NestedLoop,
        LogicalOperator::IndexNestedLoop => PhysicalOperator::IndexNestedLoop,
        LogicalOperator::HashJoin => PhysicalOperator::HashJoin,
        LogicalOperator::FactorizedAccumulate => PhysicalOperator::FactorizedAccumulate,
        LogicalOperator::ASPJoin => PhysicalOperator::ASPJoin,
        LogicalOperator::Flatten => PhysicalOperator::Flatten,
        LogicalOperator::Mutation { kind } => PhysicalOperator::Mutation { kind: *kind },
        LogicalOperator::Schema { kind } => PhysicalOperator::Schema { kind: *kind },
        LogicalOperator::Commit => PhysicalOperator::Commit,
        LogicalOperator::RelationshipScan { variable } => PhysicalOperator::RelationshipScan {
            variable: variable.clone(),
        },
        LogicalOperator::TypeSeek { relationship_type } => PhysicalOperator::AdjacencySeek {
            from: String::new(),
            relationship_type: Some(relationship_type.clone()),
            direction: crate::query::plan::Direction::Outgoing,
        },
    }
}

#[cfg(test)]
mod fallback_display_tests {
    use super::*;

    #[test]
    fn original_fallback_keeps_relationship_types_and_both_arrow_directions() {
        let source = "MATCH (a)<-[:`T``X`]-(b),(c)-[:OUT]->(d),(e)--(f) RETURN a";
        let ast = crate::cypher::parse(source).unwrap();
        let matches = BTreeMap::new();
        let logical = logical_operators_with_sites(&ast.root, &matches, source)
            .unwrap()
            .0;
        let mut physical = logical.iter().map(physical_operator).collect::<Vec<_>>();
        bind_program_relationships(&ast.root, &matches, &logical, &mut physical);
        let actual = physical
            .iter()
            .filter_map(|operator| {
                if let PhysicalOperator::AdjacencySeek {
                    relationship_type,
                    direction,
                    ..
                } = operator
                {
                    Some((relationship_type.as_deref(), *direction))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        use crate::query::plan::Direction;
        assert_eq!(
            actual,
            [
                (Some("T`X"), Direction::Incoming),
                (Some("OUT"), Direction::Outgoing),
                (None, Direction::Undirected)
            ]
        );
    }
}

#[cfg(test)]
mod lower_errors_tests;
