use crate::cypher::ExecutionMode;

use super::HashJoinRuntimeMetrics;

use super::super::QueryResult;
use super::super::plan::{PhysicalOperator, PhysicalPlan, ProfileBinding, ProfileRole};
use super::super::resources::{MemoryReservation, QueryResources, allocation_bytes, checked_add};

#[derive(Debug, Default)]
pub struct QueryMetrics {
    pub rows: u64,
    pub db_hits: u64,
    pub elapsed_micros: u64,
    profile: Option<QueryProfileState>,
    resources: QueryResources,
    _memory: Option<MemoryReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorRuntimeMetrics {
    pub id: u64,
    pub operator: String,
    pub rows: u64,
    pub db_hits: u64,
    pub hash: Option<HashJoinRuntimeMetrics>,
    pub semi_mask: Option<SemiMaskRuntimeMetrics>,
    pub factorized: Option<FactorizedRuntimeMetrics>,
}

/// Actual work reported by the factor table, ASP phases and final Flatten.
/// Values are cumulative events or live-owned peaks, never planner estimates.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FactorizedRuntimeMetrics {
    pub prefixes: u64,
    pub accumulate_rows: u64,
    pub build_rows: u64,
    pub hash_entries: u64,
    pub probe_rows: u64,
    pub group_members: u64,
    pub flattened_rows: u64,
    pub semijoin_keys: u64,
    pub semijoin_seeks: u64,
    pub memory_peak_bytes: u64,
    pub spill_encoded_bytes: u64,
    pub spill_batches: u64,
    pub spill_vm_steps: u64,
    pub temp_allocated_bytes: u64,
    pub accumulate_micros: u64,
    pub build_micros: u64,
    pub probe_micros: u64,
    pub predicate_evaluations: u64,
    pub expression_evaluations: u64,
}

/// Actual finite-ID advisory work at the existing storage access boundary.
/// The static access name remains valid when another input row falls back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SemiMaskRuntimeMetrics {
    pub keys: u64,
    pub seeks: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct QueryProfileState {
    operators: Vec<OperatorRuntimeMetrics>,
    access_operators: Vec<usize>,
    access_variables: Vec<Option<String>>,
    active_operator: Option<usize>,
    next_access_operator: usize,
    unattributed_db_hits: u64,
    sites: Vec<ProfileBinding>,
    execution_memory_peak_bytes: Option<u64>,
}

impl QueryMetrics {
    pub(crate) fn for_execution_with_resources(
        mode: ExecutionMode,
        physical: &PhysicalPlan,
        resources: &QueryResources,
    ) -> QueryResult<Self> {
        let mut metrics = Self {
            rows: 0,
            db_hits: 0,
            elapsed_micros: 0,
            profile: None,
            resources: resources.clone(),
            _memory: None,
        };
        if mode != ExecutionMode::Profile {
            return Ok(metrics);
        }
        let access_count = physical
            .operators
            .iter()
            .filter(|operator| operator.is_storage_access())
            .count();
        let bytes = initial_profile_bytes(physical, access_count)?;
        let memory = resources.reserve(bytes)?;
        let mut operators = Vec::with_capacity(physical.operators.len());
        let mut access_operators = Vec::with_capacity(access_count);
        let mut access_variables = Vec::with_capacity(access_count);
        for (id, operator) in physical.operators.iter().enumerate() {
            operators.push(OperatorRuntimeMetrics {
                id: u64::try_from(id).unwrap_or(u64::MAX),
                operator: operator.profile_name().to_owned(),
                rows: 0,
                db_hits: 0,
                hash: (operator.profile_name() == "HashJoin").then(HashJoinRuntimeMetrics::default),
                semi_mask: None,
                factorized: matches!(
                    operator.profile_name(),
                    "FactorizedAccumulate" | "ASPJoin" | "Flatten"
                )
                .then(FactorizedRuntimeMetrics::default),
            });
            if operator.is_storage_access() {
                access_operators.push(id);
                access_variables.push(access_variable(operator).map(str::to_owned));
            }
        }
        let active_operator = access_operators
            .first()
            .copied()
            .or((!operators.is_empty()).then_some(0));
        metrics.profile = Some(QueryProfileState {
            operators,
            access_operators,
            access_variables,
            active_operator,
            next_access_operator: 0,
            unattributed_db_hits: 0,
            sites: Vec::new(),
            execution_memory_peak_bytes: None,
        });
        metrics._memory = Some(memory);
        Ok(metrics)
    }

    pub(crate) fn install_profile_sites(
        &mut self,
        sites: impl Iterator<Item = ProfileBinding> + Clone,
    ) -> QueryResult<()> {
        let Some(profile) = self.profile.as_mut() else {
            return Ok(());
        };
        let count = sites.clone().count();
        if count == 0 {
            return Ok(());
        }
        let memory = self
            .resources
            .reserve(allocation_bytes::<ProfileBinding>(count)?)?;
        let mut selected = Vec::with_capacity(count);
        selected.extend(sites);
        profile.sites = selected;
        if let Some(owned) = self._memory.as_mut() {
            owned.merge(memory)?;
        } else {
            self._memory = Some(memory);
        }
        Ok(())
    }

    pub(crate) fn match_profile_slots(
        &self,
        clause: usize,
        part: usize,
    ) -> Option<MatchProfileSlots> {
        let profile = self.profile.as_ref()?;
        let mut slots = MatchProfileSlots::default();
        let mut found = false;
        for binding in &profile.sites {
            if binding.site.clause_start == clause && binding.site.part == Some(part) {
                slots.slots[binding.site.role as usize] = Some(binding.physical_slot);
                found = true;
            }
        }
        found.then_some(slots)
    }

    pub(crate) fn match_residual_slot(&self, clause: usize) -> Option<usize> {
        self.profile
            .as_ref()?
            .sites
            .iter()
            .find(|binding| {
                binding.site.clause_start == clause
                    && binding.site.part.is_none()
                    && binding.site.role == ProfileRole::EarlyFilter
            })
            .map(|binding| binding.physical_slot)
    }

    pub(crate) fn node_point_slot(&self, clause: usize, part: usize) -> Option<usize> {
        let slot = self
            .match_profile_slots(clause, part)?
            .slot(ProfileRole::NodeAccess)?;
        self.profile
            .as_ref()?
            .operators
            .get(slot)
            .and_then(|operator| (operator.operator == "NodeIdSeek").then_some(slot))
    }

    pub(crate) fn activate_profile_slot(&mut self, slot: Option<usize>) -> Option<Option<usize>> {
        let profile = self.profile.as_mut()?;
        let previous = profile.active_operator;
        profile.active_operator = slot;
        Some(previous)
    }

    pub(crate) fn record_profile_rows(&mut self, slot: Option<usize>, count: u64) {
        if let Some(operator) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
        {
            operator.rows = operator.rows.saturating_add(count);
        }
    }

    pub(crate) fn factorized_slot(&self, clause: usize, name: &str) -> Option<usize> {
        let role = match name {
            "FactorizedAccumulate" => ProfileRole::FactorizedAccumulate,
            "ASPJoin" => ProfileRole::ASPJoin,
            "Flatten" => ProfileRole::Flatten,
            _ => return None,
        };
        self.profile
            .as_ref()?
            .sites
            .iter()
            .find(|binding| {
                binding.site.clause_start == clause
                    && binding.site.part.is_none()
                    && binding.site.role == role
            })
            .map(|binding| binding.physical_slot)
    }

    pub(crate) fn set_factorized_profile(
        &mut self,
        slot: Option<usize>,
        mut stats: FactorizedRuntimeMetrics,
    ) {
        if let Some(operator) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
            && matches!(
                operator.operator.as_str(),
                "FactorizedAccumulate" | "ASPJoin" | "Flatten"
            )
        {
            if let Some(previous) = operator.factorized {
                stats.predicate_evaluations = previous.predicate_evaluations;
                stats.expression_evaluations = previous.expression_evaluations;
            }
            operator.factorized = Some(stats);
        }
    }

    pub(crate) fn record_factorized_flatten(&mut self, clause: usize, rows: u64) {
        let slot = self.factorized_slot(clause, "Flatten");
        self.record_profile_rows(slot, rows);
        if let Some(operator) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
        {
            let stats = operator
                .factorized
                .get_or_insert_with(FactorizedRuntimeMetrics::default);
            stats.flattened_rows = stats.flattened_rows.saturating_add(rows);
        }
    }

    pub(crate) fn record_factorized_storage(
        &mut self,
        clause: usize,
        name: &str,
        delta: FactorizedRuntimeMetrics,
    ) {
        let slot = self.factorized_slot(clause, name);
        if let Some(operator) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
        {
            let stats = operator
                .factorized
                .get_or_insert_with(FactorizedRuntimeMetrics::default);
            stats.spill_encoded_bytes = stats
                .spill_encoded_bytes
                .saturating_add(delta.spill_encoded_bytes);
            stats.spill_batches = stats.spill_batches.saturating_add(delta.spill_batches);
            stats.spill_vm_steps = stats.spill_vm_steps.saturating_add(delta.spill_vm_steps);
            stats.memory_peak_bytes = stats.memory_peak_bytes.max(delta.memory_peak_bytes);
            stats.temp_allocated_bytes = stats.temp_allocated_bytes.max(delta.temp_allocated_bytes);
        }
    }

    pub(crate) fn record_factorized_evaluations(
        &mut self,
        slot: Option<usize>,
        predicates: u64,
        expressions: u64,
    ) {
        if let Some(stats) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
            .filter(|operator| {
                matches!(
                    operator.operator.as_str(),
                    "FactorizedAccumulate" | "ASPJoin" | "Flatten"
                )
            })
            .map(|operator| {
                operator
                    .factorized
                    .get_or_insert_with(FactorizedRuntimeMetrics::default)
            })
        {
            stats.predicate_evaluations = stats.predicate_evaluations.saturating_add(predicates);
            stats.expression_evaluations = stats.expression_evaluations.saturating_add(expressions);
        }
    }

    pub fn try_clone(&self) -> QueryResult<Self> {
        self.try_clone_with_extra_bytes(0)
    }

    pub(crate) fn try_clone_with_extra_bytes(&self, extra: u64) -> QueryResult<Self> {
        let bytes = self.profile.as_ref().map_or(Ok(extra), |profile| {
            checked_add(cloned_profile_bytes(profile)?, extra)
        })?;
        let memory = self.resources.reserve(bytes)?;
        let profile = self.profile.as_ref().map(clone_profile);
        Ok(Self {
            rows: self.rows,
            db_hits: self.db_hits,
            elapsed_micros: self.elapsed_micros,
            profile,
            resources: self.resources.clone(),
            _memory: Some(memory),
        })
    }

    pub fn operator_profile(&self) -> Option<&[OperatorRuntimeMetrics]> {
        self.profile
            .as_ref()
            .map(|profile| profile.operators.as_slice())
    }

    pub(crate) fn record_semi_mask(&mut self, keys: u64, seeks: u64) {
        if let Some(operator) = self.profile.as_mut().and_then(|profile| {
            profile
                .active_operator
                .and_then(|slot| profile.operators.get_mut(slot))
        }) {
            let stats = operator
                .semi_mask
                .get_or_insert_with(SemiMaskRuntimeMetrics::default);
            stats.keys = stats.keys.saturating_add(keys);
            stats.seeks = stats.seeks.saturating_add(seeks);
        }
    }

    /// Replace the cumulative snapshot at the exact selected Hash Join site.
    /// A missing or non-Hash site remains unchanged, including fallback plans.
    pub(crate) fn set_hash_profile(
        &mut self,
        slot: Option<usize>,
        mut stats: HashJoinRuntimeMetrics,
    ) {
        if let Some(operator) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
            .filter(|operator| operator.hash.is_some())
        {
            if stats.dynamic_filter.is_none() {
                stats.dynamic_filter = operator.hash.and_then(|previous| previous.dynamic_filter);
            }
            operator.hash = Some(stats);
        }
    }

    pub(crate) fn set_dynamic_filter_profile(
        &mut self,
        slot: Option<usize>,
        stats: super::hash_metrics::JoinDynamicFilterRuntimeMetrics,
    ) {
        if let Some(hash) = self
            .profile
            .as_mut()
            .and_then(|profile| slot.and_then(|slot| profile.operators.get_mut(slot)))
            .and_then(|operator| operator.hash.as_mut())
        {
            hash.dynamic_filter = Some(stats);
        }
    }

    /// Execution-wide reservation high-water mark, frozen at terminal-ready.
    pub fn execution_memory_peak_bytes(&self) -> Option<u64> {
        self.profile.as_ref()?.execution_memory_peak_bytes
    }

    pub(crate) fn record_db_hits(&mut self, count: u64) {
        self.db_hits = self.db_hits.saturating_add(count);
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        if let Some(operator) = profile
            .active_operator
            .and_then(|id| profile.operators.get_mut(id))
        {
            operator.db_hits = operator.db_hits.saturating_add(count);
        } else {
            profile.unattributed_db_hits = profile.unattributed_db_hits.saturating_add(count);
        }
    }

    /// Temporarily attribute physical access to this Pattern Part's variable.
    /// Return a token only if a matching access operator is present; a caller
    /// must restore it even if the cursor returns an error.
    pub(crate) fn activate_access_for_operator(
        &mut self,
        variable: Option<&str>,
        operator_name: &str,
    ) -> Option<Option<usize>> {
        let variable = variable?;
        let profile = self.profile.as_mut()?;
        let access_index = profile.access_operators.iter().enumerate().find_map(
            |(access_index, operator_id)| {
                if profile
                    .sites
                    .iter()
                    .any(|binding| binding.physical_slot == *operator_id)
                {
                    return None;
                }
                let variable_matches = profile
                    .access_variables
                    .get(access_index)
                    .and_then(Option::as_deref)
                    == Some(variable);
                let operator_matches = profile
                    .operators
                    .get(*operator_id)
                    .is_some_and(|operator| operator.operator == operator_name);
                (variable_matches && operator_matches).then_some(access_index)
            },
        )?;
        let access_operator = *profile.access_operators.get(access_index)?;
        let previous = profile.active_operator;
        profile.active_operator = Some(access_operator);
        Some(previous)
    }

    pub(crate) fn restore_access_operator(&mut self, token: Option<Option<usize>>) {
        if let Some(previous) = token {
            self.restore_active_operator(previous);
        }
    }

    pub(crate) fn activate_match_clause_access(
        &mut self,
        clause: usize,
        count: usize,
    ) -> Option<usize> {
        if let Some(slot) = self.clause_access_slot(clause) {
            return self.activate_profile_slot(Some(slot)).flatten();
        }
        self.activate_access_operator_group(count)
    }

    fn clause_access_slot(&self, clause: usize) -> Option<usize> {
        let profile = self.profile.as_ref()?;
        // Point producers take their own temporary scope. Keep other work on
        // its existing access boundary instead of charging it to a point.
        profile
            .sites
            .iter()
            .filter(|binding| {
                binding.site.clause_start == clause
                    && matches!(
                        binding.site.role,
                        ProfileRole::NodeAccess | ProfileRole::RelationshipAccess
                    )
            })
            .map(|binding| binding.physical_slot)
            .min_by_key(|slot| (profile.operators[*slot].operator == "NodeIdSeek", *slot))
    }

    pub(crate) fn record_eager_match_rows(&mut self, clause: usize, count: u64) {
        let point = self.profile.as_ref().is_some_and(|profile| {
            profile.active_operator.is_some_and(|slot| {
                profile.operators[slot].operator == "NodeIdSeek"
                    && profile.sites.iter().any(|binding| {
                        binding.physical_slot == slot && binding.site.clause_start == clause
                    })
            })
        });
        if !point {
            self.record_active_rows(count);
        }
    }

    pub(crate) fn activate_access_operator_group(&mut self, count: usize) -> Option<usize> {
        let profile = self.profile.as_mut()?;
        let previous = profile.active_operator;
        while profile.next_access_operator < profile.access_operators.len()
            && profile.sites.iter().any(|binding| {
                binding.physical_slot == profile.access_operators[profile.next_access_operator]
            })
        {
            profile.next_access_operator += 1;
        }
        if let Some(operator) = profile
            .access_operators
            .get(profile.next_access_operator)
            .copied()
        {
            profile.active_operator = Some(operator);
            profile.next_access_operator = profile
                .next_access_operator
                .saturating_add(count.max(1))
                .min(profile.access_operators.len());
        }
        previous
    }

    pub(crate) fn restore_active_operator(&mut self, previous: Option<usize>) {
        if let Some(profile) = self.profile.as_mut() {
            profile.active_operator = previous;
        }
    }

    pub(crate) fn has_explicit_profile_sites(&self) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|profile| !profile.sites.is_empty())
    }

    pub(crate) fn record_pipeline_rows(&mut self, count: u64) {
        // MATCH frames with exact sites already counted their producer rows.
        // Preserve the old outer accounting when no such sites were installed.
        if !self.has_explicit_profile_sites() {
            self.record_active_rows(count);
        }
    }

    pub(crate) fn record_active_rows(&mut self, count: u64) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        if let Some(operator) = profile
            .active_operator
            .and_then(|id| profile.operators.get_mut(id))
        {
            operator.rows = operator.rows.saturating_add(count);
        }
    }

    pub(super) fn finish_profile(&mut self) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        if profile.execution_memory_peak_bytes.is_none()
            && profile
                .operators
                .iter()
                .any(|operator| operator.hash.is_some() || operator.factorized.is_some())
        {
            profile.execution_memory_peak_bytes = Some(self.resources.peak());
        }
        if profile.unattributed_db_hits > 0 && profile.sites.is_empty() {
            let fallback = profile
                .access_operators
                .first()
                .copied()
                .or((!profile.operators.is_empty()).then_some(0));
            if let Some(operator) = fallback.and_then(|id| profile.operators.get_mut(id)) {
                operator.db_hits = operator
                    .db_hits
                    .saturating_add(profile.unattributed_db_hits);
            }
            profile.unattributed_db_hits = 0;
        }
        if let Some(operator) = profile.operators.last_mut() {
            operator.rows = self.rows;
        }
    }
}

impl PartialEq for QueryMetrics {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
            && self.db_hits == other.db_hits
            && self.elapsed_micros == other.elapsed_micros
            && self.profile == other.profile
    }
}

impl Eq for QueryMetrics {}

fn profile_vectors_bytes(operators: usize, access: usize) -> QueryResult<u64> {
    checked_add(
        allocation_bytes::<OperatorRuntimeMetrics>(operators)?,
        checked_add(
            allocation_bytes::<usize>(access)?,
            allocation_bytes::<Option<String>>(access)?,
        )?,
    )
}

fn access_variable(operator: &PhysicalOperator) -> Option<&str> {
    match operator {
        PhysicalOperator::NodeScan { variable }
        | PhysicalOperator::LabelIndexScan { variable, .. }
        | PhysicalOperator::IndexSeek { variable, .. }
        | PhysicalOperator::NodeIdSeek { variable }
        | PhysicalOperator::RelationshipIdSeek { variable }
        | PhysicalOperator::VectorSearch { variable, .. } => Some(variable),
        PhysicalOperator::RelationshipScan { variable } => variable.as_deref(),
        PhysicalOperator::AdjacencySeek { from, .. } => Some(from),
        _ => None,
    }
}

fn initial_profile_bytes(physical: &PhysicalPlan, access_count: usize) -> QueryResult<u64> {
    physical.operators.iter().try_fold(
        profile_vectors_bytes(physical.operators.len(), access_count)?,
        |bytes, operator| {
            let bytes = checked_add(bytes, operator.profile_name().len() as u64)?;
            checked_add(
                bytes,
                access_variable(operator).map_or(0, |variable| variable.len() as u64),
            )
        },
    )
}

fn cloned_profile_bytes(profile: &QueryProfileState) -> QueryResult<u64> {
    let bytes = profile.operators.iter().try_fold(
        checked_add(
            profile_vectors_bytes(profile.operators.len(), profile.access_operators.len())?,
            allocation_bytes::<ProfileBinding>(profile.sites.len())?,
        )?,
        |bytes, operator| checked_add(bytes, operator.operator.len() as u64),
    )?;
    profile
        .access_variables
        .iter()
        .try_fold(bytes, |bytes, name| {
            checked_add(bytes, name.as_ref().map_or(0, |name| name.len() as u64))
        })
}

fn clone_profile(profile: &QueryProfileState) -> QueryProfileState {
    let mut operators = Vec::with_capacity(profile.operators.len());
    for operator in &profile.operators {
        operators.push(OperatorRuntimeMetrics {
            id: operator.id,
            operator: operator.operator.to_owned(),
            rows: operator.rows,
            db_hits: operator.db_hits,
            hash: operator.hash,
            semi_mask: operator.semi_mask,
            factorized: operator.factorized,
        });
    }
    let mut access_operators = Vec::with_capacity(profile.access_operators.len());
    access_operators.extend_from_slice(&profile.access_operators);
    QueryProfileState {
        operators,
        access_operators,
        access_variables: profile.access_variables.clone(),
        active_operator: profile.active_operator,
        next_access_operator: profile.next_access_operator,
        unattributed_db_hits: profile.unattributed_db_hits,
        sites: profile.sites.clone(),
        execution_memory_peak_bytes: profile.execution_memory_peak_bytes,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MatchProfileSlots {
    slots: [Option<usize>; 7],
}

impl MatchProfileSlots {
    pub(crate) fn slot(self, role: ProfileRole) -> Option<usize> {
        self.slots[role as usize]
    }
}

#[cfg(test)]
mod tests;
