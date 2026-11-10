use crate::cypher::ExecutionMode;

use super::super::QueryResult;
use super::super::plan::{PhysicalOperator, PhysicalPlan};
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
}

#[derive(Debug, PartialEq, Eq)]
struct QueryProfileState {
    operators: Vec<OperatorRuntimeMetrics>,
    access_operators: Vec<usize>,
    access_variables: Vec<Option<String>>,
    active_operator: Option<usize>,
    next_access_operator: usize,
    unattributed_db_hits: u64,
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
        });
        metrics._memory = Some(memory);
        Ok(metrics)
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

    pub(crate) fn activate_access_operator_group(&mut self, count: usize) -> Option<usize> {
        let profile = self.profile.as_mut()?;
        let previous = profile.active_operator;
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
        if profile.unattributed_db_hits > 0 {
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
        profile_vectors_bytes(profile.operators.len(), profile.access_operators.len())?,
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
    }
}

#[cfg(test)]
mod tests;
