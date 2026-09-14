use crate::cypher::ExecutionMode;

use super::super::plan::PhysicalPlan;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryMetrics {
    pub rows: u64,
    pub db_hits: u64,
    pub elapsed_micros: u64,
    profile: Option<QueryProfileState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorRuntimeMetrics {
    pub id: u64,
    pub operator: String,
    pub rows: u64,
    pub db_hits: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryProfileState {
    operators: Vec<OperatorRuntimeMetrics>,
    access_operators: Vec<usize>,
    active_operator: Option<usize>,
    next_access_operator: usize,
    unattributed_db_hits: u64,
}

impl QueryMetrics {
    pub(super) fn for_execution(mode: ExecutionMode, physical: &PhysicalPlan) -> Self {
        if mode != ExecutionMode::Profile {
            return Self::default();
        }
        let operators = physical
            .operators
            .iter()
            .enumerate()
            .map(|(id, operator)| OperatorRuntimeMetrics {
                id: u64::try_from(id).unwrap_or(u64::MAX),
                operator: operator.profile_name().to_owned(),
                rows: 0,
                db_hits: 0,
            })
            .collect::<Vec<_>>();
        let access_operators = physical
            .operators
            .iter()
            .enumerate()
            .filter_map(|(id, operator)| operator.is_storage_access().then_some(id))
            .collect::<Vec<_>>();
        let active_operator = access_operators
            .first()
            .copied()
            .or((!operators.is_empty()).then_some(0));
        Self {
            profile: Some(QueryProfileState {
                operators,
                access_operators,
                active_operator,
                next_access_operator: 0,
                unattributed_db_hits: 0,
            }),
            ..Self::default()
        }
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
