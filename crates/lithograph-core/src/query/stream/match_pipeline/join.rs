use super::*;
use crate::query::plan::ProfileRole;
use crate::query::plan::join::{
    JoinPlan, ParameterTemplate, ProbeAccess, ProbeTarget, StageAccess,
};
use crate::query::schema::{BoundStandardIndexRead, StandardIndexPage};

/// Owned by a clause source, not by its repeatedly restarted outer frame.
#[derive(Debug, Default)]
pub(crate) struct MatchReadBindings {
    identity: Option<HashId>,
    indexes: Vec<Option<BoundStandardIndexRead>>,
    pub(super) hashes: Vec<Option<hash::HashBuildState>>,
    memory: Option<MemoryReservation>,
}

impl MatchReadBindings {
    pub(super) fn prepare(
        &mut self,
        plan: &JoinPlan,
        snapshot: &Snapshot<'_>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.identity == Some(snapshot.cache_identity()) {
            return Ok(());
        }
        self.indexes = Vec::new();
        self.hashes = Vec::new();
        self.memory = None;
        self.identity = None;
        let hashes = if plan
            .stages
            .iter()
            .any(|stage| matches!(stage.access, StageAccess::Hash { .. }))
        {
            plan.stages.len()
        } else {
            0
        };
        let bytes = crate::query::resources::checked_add(
            crate::query::resources::allocation_bytes::<Option<BoundStandardIndexRead>>(
                plan.templates.len(),
            )?,
            crate::query::resources::allocation_bytes::<Option<hash::HashBuildState>>(hashes)?,
        )?;
        let memory = control.reserve(bytes)?;
        self.indexes = Vec::with_capacity(plan.templates.len());
        self.indexes.resize_with(plan.templates.len(), || None);
        self.hashes = Vec::with_capacity(hashes);
        self.hashes.resize_with(hashes, || None);
        for (index, stage) in plan.stages.iter().enumerate().take(hashes) {
            if matches!(stage.access, StageAccess::Hash { .. }) {
                self.hashes[index] = Some(hash::HashBuildState::new());
            }
        }
        self.memory = Some(memory);
        self.identity = Some(snapshot.cache_identity());
        Ok(())
    }

    pub(in crate::query) fn finish(
        &mut self,
        step: &MatchStep,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<()> {
        let Some(plan) = &step.join else {
            return Ok(());
        };
        for (depth, state) in self.hashes.iter_mut().enumerate() {
            if let Some(state) = state {
                state.finish(metrics, hash::hash_slot(metrics, plan, depth))?;
            }
        }
        Ok(())
    }

    fn bind(
        &mut self,
        plan: &JoinPlan,
        template: usize,
        snapshot: &Snapshot<'_>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        self.prepare(plan, snapshot, control)?;
        if self.indexes[template].is_none() {
            let ProbeAccess::IndexEquality {
                index_name,
                definition_hash,
                ..
            } = &plan.templates[template].access
            else {
                return Err(QueryError::internal(
                    "non-Index template requested an Index binding",
                ));
            };
            let bound = crate::query::schema::bind_standard_index_read(
                snapshot,
                index_name,
                *definition_hash,
                control,
            )?;
            let expected = if plan.templates[template].target == ProbeTarget::Relationship {
                crate::storage::OwnerKind::Relationship
            } else {
                crate::storage::OwnerKind::Node
            };
            if bound.owner_kind() != expected {
                return Err(QueryError::internal(
                    "parameterized Index owner kind changed",
                ));
            }
            self.indexes[template] = Some(bound);
        }
        Ok(())
    }
}

impl StepCursor {
    pub(super) fn frame(
        &self,
        depth: usize,
        row: &BindingRow,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<PartCursor> {
        let (snapshot, view) = scope;
        if let Some(plan) = &self.step.join {
            if plan.snapshot_identity != snapshot.cache_identity() {
                return Err(QueryError::internal(
                    "MATCH Join plan belongs to another Snapshot revision",
                ));
            }
            let (ordinal, part) = crate::query::plan::join::execution_part(&self.step, depth);
            let mut cursor = PartCursor::new(&part, row, control)?;
            cursor.profile = metrics.match_profile_slots(plan.clause_start, ordinal);
            if let StageAccess::Probe(template) = plan.stages[depth].access {
                cursor.bind_probe(plan, template, snapshot, view, bindings, control)?;
            }
            return Ok(cursor);
        }
        let mut cursor = PartCursor::new(&self.step.parts[depth], row, control)?;
        cursor.profile = metrics.match_profile_slots(self.step.clause_start, depth);
        Ok(cursor)
    }

    pub(super) fn join_prefix_rejects(
        &self,
        depth: usize,
        row: &BindingRow,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let Some(plan) = &self.step.join else {
            return match self.step.early_predicates.get(depth) {
                Some(predicates) => {
                    early_prefix_rejects(predicates, snapshot, view, row, params, control)
                }
                None => Ok(false),
            };
        };
        for predicate in &plan.stages[depth].predicates_after {
            // This selected key equality already succeeded inside the Hash
            // boundary. Keep the complete WHERE; only avoid its duplicate
            // scheduled early evaluation after that same boundary.
            if let StageAccess::Hash { equality, .. } = plan.stages[depth].access
                && *predicate == plan.hashes[equality].predicate
            {
                continue;
            }
            if !expression::predicate(
                expression::evaluate_with_access_control(
                    &plan.predicates[*predicate].expression,
                    snapshot,
                    row,
                    params,
                    &view.query_access,
                    control,
                )?
                .value,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn accept_prefix(
        &self,
        depth: usize,
        row: &BindingRow,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let (snapshot, view) = scope;
        let slots = self
            .stack
            .last()
            .and_then(|frame| frame.profile(&self.step, depth, metrics));
        if let Some(slots) = slots {
            metrics.record_profile_rows(slots.slot(ProfileRole::Join), 1);
            let ordinal = self
                .step
                .join
                .as_ref()
                .map_or(depth, |plan| plan.stages[depth].part);
            if self.step.parts[ordinal].relationship.is_some()
                && !self.step.join.as_ref().is_some_and(|plan| {
                    matches!(plan.stages[depth].access, StageAccess::Hash { .. })
                })
            {
                metrics.record_profile_rows(slots.slot(ProfileRole::RelationshipAccess), 1);
            }
        }
        if self.join_prefix_rejects(depth, row, snapshot, view, params, control)? {
            return Ok(false);
        }
        if let Some(slots) = slots {
            metrics.record_profile_rows(slots.slot(ProfileRole::EarlyFilter), 1);
        }
        Ok(true)
    }

    pub(super) fn full_match_passes(
        &self,
        row: &BindingRow,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        match &self.step.predicate {
            Some(predicate) => expression::predicate(
                expression::evaluate_with_access_control(
                    predicate,
                    snapshot,
                    row,
                    params,
                    &view.query_access,
                    control,
                )?
                .value,
            ),
            None => Ok(true),
        }
    }

    pub(super) fn restore_declared_order(&self, row: &mut BindingRow) {
        let Some(plan) = &self.step.join else { return };
        row.order.sort_unstable_by_key(|name| {
            plan.declared_columns
                .iter()
                .position(|declared| declared == name)
                .unwrap_or(usize::MAX)
        });
    }
}

impl PartCursor {
    fn bind_probe(
        &mut self,
        plan: &JoinPlan,
        template: usize,
        snapshot: &Snapshot<'_>,
        view: &ResolvedGraphView,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let selected = &plan.templates[template];
        match &selected.access {
            ProbeAccess::Identity { source_variable }
            | ProbeAccess::Adjacency { source_variable } => {
                self.install_identity(selected, source_variable)?;
            }
            ProbeAccess::IndexEquality {
                source_variable,
                source_property,
                ..
            } => {
                let (value, memory) = read_probe_property(
                    snapshot,
                    view,
                    &self.base,
                    source_variable,
                    source_property,
                    control,
                )?;
                self.probe_empty = matches!(value, Value::Null);
                self.probe_value = Some(value);
                self.probe_memory = Some(memory);
                if !self.probe_empty {
                    bindings.bind(plan, template, snapshot, control)?;
                    self.probe_template = Some(template);
                }
            }
        }
        Ok(())
    }

    fn install_identity(&mut self, selected: &ParameterTemplate, source: &str) -> QueryResult<()> {
        let id = match (selected.target, self.base.values.get(source)) {
            (_, Some(BindingValue::Null) | None) => {
                self.probe_empty = true;
                return Ok(());
            }
            (ProbeTarget::Relationship, Some(BindingValue::Relationship(rel))) => rel.id,
            (ProbeTarget::StartNode | ProbeTarget::EndNode, Some(BindingValue::Node(id))) => *id,
            _ => {
                return Err(QueryError::internal(
                    "parameterized identity lost its entity proof",
                ));
            }
        };
        if selected.target == ProbeTarget::Relationship {
            self.part
                .relationship
                .as_mut()
                .ok_or_else(|| QueryError::internal("Relationship probe lacks a Relationship"))?
                .id_seek = Some(id);
        } else {
            self.part.start.id_seek = Some(id);
        }
        Ok(())
    }

    pub(super) fn scan_bound_page(
        &self,
        snapshot: &Snapshot<'_>,
        bindings: &MatchReadBindings,
        cursor: Option<&crate::query::schema::StandardIndexCursor>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<StandardIndexPage>> {
        let Some(template) = self.probe_template else {
            return Ok(None);
        };
        let bound = bindings
            .indexes
            .get(template)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                QueryError::internal("parameterized Index binding was released before its cursor")
            })?;
        let value = self.probe_value.as_ref().ok_or_else(|| {
            QueryError::internal("parameterized Index cursor lacks its concrete key")
        })?;
        crate::query::schema::scan_bound_index_page(
            snapshot,
            bound,
            value,
            cursor,
            PIPELINE_BATCH,
            control,
        )
        .map(Some)
    }

    pub(super) fn record_fallback_relationship_rows(
        &self,
        metrics: &mut QueryMetrics,
        result: &QueryResult<Option<BindingRow>>,
        indexed_only: bool,
    ) {
        let indexed = self
            .part
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.id_seek.is_some() || rel.index_seek.is_some());
        if self.profile.is_none()
            && metrics.has_explicit_profile_sites()
            && (!indexed_only || indexed)
            && matches!(result, Ok(Some(_)))
        {
            metrics.record_active_rows(1);
        }
    }

    pub(super) fn activate_role(
        &self,
        metrics: &mut QueryMetrics,
        role: ProfileRole,
    ) -> Option<Option<usize>> {
        self.profile.and_then(|slots| {
            metrics
                .activate_profile_slot(slots.slot(role).or_else(|| slots.slot(ProfileRole::Join)))
        })
    }

    pub(super) fn activate_frame(&self, metrics: &mut QueryMetrics) -> Option<Option<usize>> {
        if self.profile.is_some() {
            let relationship = self
                .part
                .relationship
                .as_ref()
                .is_some_and(|rel| rel.id_seek.is_some() || rel.index_seek.is_some());
            return self.activate_role(
                metrics,
                if relationship {
                    ProfileRole::RelationshipAccess
                } else {
                    ProfileRole::NodeAccess
                },
            );
        }
        let access = self.profile_access_operator().or_else(|| {
            metrics.has_explicit_profile_sites().then(|| {
                (
                    self.part.start.variable.as_deref().unwrap_or("_anon"),
                    if self.part.start.scan_label.is_some() {
                        "LabelIndexScan"
                    } else {
                        "NodeScan"
                    },
                )
            })
        });
        let token = access.and_then(|(variable, operator)| {
            metrics.activate_access_for_operator(Some(variable), operator)
        });
        token.or_else(|| {
            metrics
                .has_explicit_profile_sites()
                .then(|| metrics.activate_profile_slot(None))
                .flatten()
        })
    }
}

fn read_probe_property(
    snapshot: &Snapshot<'_>,
    view: &ResolvedGraphView,
    row: &BindingRow,
    variable: &str,
    property: &str,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    match row.values.get(variable) {
        Some(BindingValue::Node(id)) => crate::query::graph::node_property_with_control(
            snapshot,
            *id,
            property,
            &view.query_access,
            control,
        ),
        Some(BindingValue::Relationship(rel)) => {
            crate::query::graph::relationship_property_with_control(
                snapshot,
                rel.id,
                property,
                &view.query_access,
                control,
            )
        }
        Some(BindingValue::Null) | None => Ok((Value::Null, control.reserve(0)?)),
        _ => Err(QueryError::internal(
            "parameterized property lost its entity proof",
        )),
    }
}
