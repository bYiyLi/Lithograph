//! Optional optimizer work has its own deterministic limit. AST/plan storage is
//! deliberately outside the execution memory quota; exhausting this limit
//! disables optimization, never execution or its cancellation/error contract.

use super::*;
use crate::query::expression::BinaryOp;

mod allowance;

#[cfg(test)]
pub(super) mod trace;

pub(super) const MAX_STATES: usize = 4_096;
pub(super) const MAX_CANDIDATES: usize = 1_024;
pub(super) const MAX_BYTES: usize = 256 * 1_024;
pub(crate) const MAX_DEPTH: usize = 64;
pub(super) const MAX_PARTS: usize = 128;

#[derive(Debug, Default)]
pub(crate) struct PlanningBudget {
    pub(super) states: usize,
    pub(super) candidates: usize,
    pub(super) bytes: usize,
    pub(super) depth: usize,
    pub(super) exhausted: bool,
    clause: Option<allowance::ClauseAllowance>,
    // This budget belongs to one prepare with fixed Graph View / PropertyRules.
    // Never reuse its sampled advice across queries or access-policy scopes.
    join_distributions: BTreeMap<(HashId, HashId), Option<crate::query::schema::IndexDistribution>>,
}

impl PlanningBudget {
    #[cfg(test)]
    pub(crate) fn observe(&self, clause_start: usize, stage: &'static str) {
        trace::record(self, clause_start, stage);
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.exhausted || self.clause.as_ref().is_some_and(|clause| clause.stopped)
    }

    pub(crate) fn stop(&mut self) {
        self.exhausted = true;
    }

    pub(super) fn cached_join_distribution(
        &self,
        snapshot: HashId,
        definition: HashId,
    ) -> Option<Option<crate::query::schema::IndexDistribution>> {
        self.join_distributions
            .get(&(snapshot, definition))
            .copied()
    }

    pub(super) fn remember_join_distribution(
        &mut self,
        snapshot: HashId,
        definition: HashId,
        sample: Option<crate::query::schema::IndexDistribution>,
    ) -> bool {
        // A first BTreeMap insert allocates a complete node, not one entry.
        // Prepay a conservative node per retained key, including split headroom.
        let bytes = std::mem::size_of::<(
            (HashId, HashId),
            Option<crate::query::schema::IndexDistribution>,
        )>()
        .saturating_mul(16)
        .saturating_add(256);
        if self.join_distributions.len() >= MAX_PARTS
            || self.remaining_states() == 0
            || bytes > self.remaining_advice_bytes()
        {
            return false;
        }
        if !self.work(1, bytes) {
            return false;
        }
        self.join_distributions
            .insert((snapshot, definition), sample);
        true
    }

    /// Optional folding inspects borrowed parameters only within this budget.
    /// Reject a collection's known capacity before walking its children; the
    /// executor's separate, complete parameter accounting remains unchanged.
    pub(super) fn parameter_bytes(
        &mut self,
        params: &BTreeMap<String, Value>,
        control: &super::super::ExecutionControl<'_>,
    ) -> QueryResult<Option<usize>> {
        self.parameter_map(params, 0, control)
    }

    fn parameter_map(
        &mut self,
        params: &BTreeMap<String, Value>,
        depth: usize,
        control: &super::super::ExecutionControl<'_>,
    ) -> QueryResult<Option<usize>> {
        let before = self.bytes;
        // The input map is borrowed, not cloned as an executor B-tree. Bound
        // its minimum inspection work up front; charge visited names/values
        // below without multiplying an execution allocator allowance again.
        if !self.accepts_work(params.len().saturating_mul(2).saturating_add(1), 0) {
            return Ok(None);
        }
        if !self.work(1, 0) {
            return Ok(None);
        }
        for (name, value) in params {
            control.work(1)?;
            if !self.work(1, name.capacity()) || !self.parameter_value(value, depth + 1, control)? {
                return Ok(None);
            }
        }
        Ok(Some(self.bytes - before))
    }

    fn parameter_value(
        &mut self,
        value: &Value,
        depth: usize,
        control: &super::super::ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        control.work(1)?;
        self.depth = self.depth.max(depth);
        if depth > MAX_DEPTH {
            self.stop();
            return Ok(false);
        }
        if !self.work(1, 0) {
            return Ok(false);
        }
        let inline = std::mem::size_of::<Value>();
        match value {
            Value::List(values) => {
                if !self.work(
                    0,
                    inline.saturating_mul(values.capacity().saturating_add(1)),
                ) {
                    return Ok(false);
                }
                for value in values {
                    if !self.parameter_value(value, depth + 1, control)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Value::Map(values) => {
                // Unlike the borrowed top-level parameter map, a Map value
                // is cloned when optional constant evaluation reads it.
                let entry = super::super::resources::map_entry_bytes::<Value>()? as usize;
                if !self.work(0, values.len().saturating_mul(entry)) {
                    return Ok(false);
                }
                self.parameter_map(values, depth, control)
                    .map(|size| size.is_some())
            }
            Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
                self.stop();
                Ok(false)
            }
            // Every other variant has an O(1) size calculation (including
            // vector coordinate capacity), without recursive child traversal.
            value => {
                let bytes = super::super::resources::value_bytes(value)?;
                Ok(self.work(0, usize::try_from(bytes).unwrap_or(usize::MAX)))
            }
        }
    }

    pub(crate) fn work(&mut self, states: usize, bytes: usize) -> bool {
        if !self.accepts_work(states, bytes) {
            return false;
        }
        self.states += states;
        self.bytes += bytes;
        true
    }

    pub(super) fn candidate(&mut self) -> bool {
        if self.candidates >= MAX_CANDIDATES {
            self.exhausted = true;
            return false;
        }
        if !self.work(1, 128) {
            return false;
        }
        self.candidates += 1;
        true
    }

    pub(super) fn step(&mut self, step: &MatchStep, schema: &storage::SchemaState) -> bool {
        if step.parts.len() > MAX_PARTS {
            self.stop();
            return false;
        }
        if !self.work(step.parts.len(), step.parts.len().saturating_mul(256))
            || !self.work(schema.indexes.len() + schema.constraints.len(), 0)
        {
            return false;
        }
        for part in &step.parts {
            if !self.work(
                part.start.labels.len(),
                part.start.label_names.iter().map(String::len).sum(),
            ) {
                return false;
            }
        }
        step.predicate
            .as_ref()
            .is_none_or(|predicate| self.expression(predicate, 1))
    }

    fn expression(&mut self, expression: &Expr, depth: usize) -> bool {
        self.depth = self.depth.max(depth);
        if depth > MAX_DEPTH {
            self.stop();
            return false;
        }
        if !self.work(1, std::mem::size_of::<Expr>()) {
            return false;
        }
        match expression {
            Expr::Binary(_, left, right) => {
                self.expression(left, depth + 1) && self.expression(right, depth + 1)
            }
            Expr::Property(value, name) => {
                self.work(0, name.len()) && self.expression(value, depth + 1)
            }
            Expr::Unary(_, value) | Expr::IsNull { value, .. } => self.expression(value, depth + 1),
            Expr::Function { args, .. } | Expr::List(args) => {
                args.iter().all(|arg| self.expression(arg, depth + 1))
            }
            Expr::Map(entries) => entries
                .iter()
                .all(|(key, value)| self.work(0, key.len()) && self.expression(value, depth + 1)),
            Expr::Literal(value) => crate::query::resources::value_bytes(value)
                .ok()
                .is_some_and(|bytes| self.work(0, usize::try_from(bytes).unwrap_or(usize::MAX))),
            Expr::Variable(name) | Expr::Parameter(name) => self.work(0, name.len()),
            // Advanced expressions keep their existing executor. No optimizer
            // traverses an unbounded nested query/comprehension on this path.
            _ => false,
        }
    }
}

impl Drop for PlanningBudget {
    fn drop(&mut self) {
        #[cfg(feature = "test-support")]
        crate::performance::record_planning(
            self.states,
            self.candidates,
            self.bytes,
            self.depth,
            self.exhausted,
        );
    }
}

/// Bound Phase 21's recursive conjunct analysis as well as the cost search.
/// This is a small stack of borrowed expressions, never a cloned predicate.
pub(super) fn conjuncts(expression: &Expr) -> Option<Vec<&Expr>> {
    let mut pending = vec![(expression, 1)];
    let mut output = Vec::new();
    while let Some((expression, depth)) = pending.pop() {
        if depth > MAX_DEPTH || pending.len() + output.len() >= MAX_PARTS {
            return None;
        }
        if let Expr::Binary(BinaryOp::And, left, right) = expression {
            pending.push((right, depth + 1));
            pending.push((left, depth + 1));
        } else {
            output.push(expression);
        }
    }
    Some(output)
}

#[cfg(test)]
mod join_advice_tests {
    use super::*;

    #[test]
    fn cached_unknown_advice_is_owned_by_one_query_state_and_definition() {
        let snapshot = HashId::from_bytes([1; 32]);
        let definition = HashId::from_bytes([2; 32]);
        let changed = HashId::from_bytes([3; 32]);
        let mut budget = PlanningBudget::default();
        assert_eq!(budget.cached_join_distribution(snapshot, definition), None);
        assert!(budget.remember_join_distribution(snapshot, definition, None));
        assert_eq!(
            budget.cached_join_distribution(snapshot, definition),
            Some(None)
        );
        assert_eq!(budget.cached_join_distribution(changed, definition), None);
        assert_eq!(budget.cached_join_distribution(snapshot, changed), None);
        assert_eq!(
            PlanningBudget::default().cached_join_distribution(snapshot, definition),
            None
        );
    }

    #[test]
    fn optional_advice_cache_refusal_keeps_the_existing_plan_budget_usable() {
        let mut budget = PlanningBudget::default();
        budget.bytes = MAX_BYTES;
        let id = HashId::from_bytes([1; 32]);
        assert!(!budget.remember_join_distribution(id, id, None));
        assert!(!budget.exhausted());
        assert!(budget.work(1, 0));
        assert_eq!(budget.cached_join_distribution(id, id), None);
    }
}
