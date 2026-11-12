use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::expression::{self, BindingRow, Expr, UnaryOp};
use crate::query::plan::{MAX_DEPTH, PlanningBudget};
use crate::query::resources::{ExecutionControl, MemoryReservation, checked_add, checked_mul};
use crate::query::{PropertyRules, QueryError, QueryErrorKind, QueryResult};
use crate::storage::{SchemaState, Snapshot};

use super::{StandardIndexPredicate, StandardIndexSeek};

/// Optional folding retains its normal "no candidate" behavior for semantic
/// errors, but execution cancellation and quota rejection must reach the host.
pub(super) struct SeekEvaluation<'a, 'connection> {
    snapshot: &'a Snapshot<'connection>,
    params: &'a BTreeMap<String, Value>,
    access: &'a PropertyRules,
    control: &'a ExecutionControl<'a>,
    budget: RefCell<&'a mut PlanningBudget>,
    folded: RefCell<MemoryReservation>,
    retained: MemoryReservation,
    error: RefCell<Option<QueryError>>,
}

impl<'a, 'connection> SeekEvaluation<'a, 'connection> {
    pub(super) fn new(
        snapshot: &'a Snapshot<'connection>,
        params: &'a BTreeMap<String, Value>,
        access: &'a PropertyRules,
        control: &'a ExecutionControl<'a>,
        budget: &'a mut PlanningBudget,
    ) -> QueryResult<Self> {
        Ok(Self {
            snapshot,
            params,
            access,
            control,
            budget: RefCell::new(budget),
            folded: RefCell::new(control.reserve(0)?),
            retained: control.reserve(0)?,
            error: RefCell::new(None),
        })
    }

    pub(super) fn value(&self, expression: &Expr) -> Option<Value> {
        if !is_constant(expression, self.params, self, 1) {
            return None;
        }
        let result = expression::evaluate_with_access_control(
            expression,
            self.snapshot,
            &BindingRow::default(),
            self.params,
            self.access,
            self.control,
        )
        .and_then(|value| {
            self.folded.borrow_mut().merge(value.reservation)?;
            Ok(value.value)
        });
        match result {
            Ok(value) => Some(value),
            Err(error)
                if matches!(
                    error.kind,
                    QueryErrorKind::Interrupted | QueryErrorKind::Resource
                ) =>
            {
                *self.error.borrow_mut() = Some(error);
                None
            }
            Err(_) => None,
        }
    }

    pub(super) fn check(&self) -> QueryResult<()> {
        if let Some(error) = self.error.borrow_mut().take() {
            return Err(error);
        }
        self.control.check()
    }

    pub(super) fn allows_property(&self, property: &str) -> bool {
        !self.access.denies(property)
    }

    /// Proof failure may select a scan, but cancellation must survive that
    /// optional-proof API and be returned by `check`/`finish`.
    pub(super) fn check_work(&self) -> bool {
        self.check_units(1)
    }

    pub(super) fn exhausted(&self) -> bool {
        self.budget.borrow().exhausted()
    }

    fn check_units(&self, units: usize) -> bool {
        if self.error.borrow().is_some() {
            return false;
        }
        match self.control.work(units as u64) {
            Ok(()) => self.budget.borrow_mut().work(units, 0),
            Err(error) => {
                *self.error.borrow_mut() = Some(error);
                false
            }
        }
    }

    pub(super) fn is_property_value(&self, value: &Value) -> bool {
        if !self.check_work() {
            return false;
        }
        let Value::List(values) = value else {
            return value.is_property_value();
        };
        let Some(first) = values.first() else {
            return true;
        };
        if matches!(first, Value::List(_) | Value::Vector(_)) || !first.is_property_value() {
            return false;
        }
        let family = std::mem::discriminant(first);
        values
            .iter()
            .all(|value| self.check_work() && std::mem::discriminant(value) == family)
    }

    pub(super) fn copy_workspace(
        &self,
        schema: &SchemaState,
        candidates: &[(String, String, StandardIndexPredicate)],
    ) -> QueryResult<MemoryReservation> {
        if schema.indexes.is_empty() {
            return self.control.reserve(0);
        }
        let bytes = candidates
            .iter()
            .try_fold(0, |bytes, (variable, key, predicate)| {
                self.control.work(1)?;
                checked_add(
                    bytes,
                    checked_add(
                        predicate_bytes(predicate, self.control)?,
                        checked_add(variable.capacity() as u64, key.capacity() as u64)?,
                    )?,
                )
            })?;
        // Each index candidate can hold supported predicates and its selected
        // seek; start and relationship seeks may coexist until the part ends.
        self.control.reserve(checked_mul(
            bytes,
            checked_mul(schema.indexes.len() as u64 + 1, 4)?,
        )?)
    }

    pub(super) fn retain(&mut self, seek: Option<&StandardIndexSeek>) -> QueryResult<()> {
        let Some(seek) = seek else {
            return Ok(());
        };
        let bytes = seek.predicates.iter().try_fold(
            seek.index_name.capacity() as u64,
            |bytes, (_, predicate)| checked_add(bytes, predicate_bytes(predicate, self.control)?),
        )?;
        self.retained.grow(bytes)
    }

    pub(super) fn release_folded(&mut self) -> QueryResult<()> {
        self.folded.get_mut().resize(0)
    }
    pub(super) fn finish(self) -> QueryResult<MemoryReservation> {
        self.check()?;
        Ok(self.retained)
    }
}

fn is_constant(
    expression: &Expr,
    params: &BTreeMap<String, Value>,
    evaluation: &SeekEvaluation<'_, '_>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH || !evaluation.check_work() {
        return false;
    }
    match expression {
        Expr::Literal(value) => constant_value_fits(value, evaluation, depth + 1),
        Expr::Parameter(name) => params
            .get(name)
            .is_none_or(|value| constant_value_fits(value, evaluation, depth + 1)),
        Expr::Unary(UnaryOp::Positive | UnaryOp::Negative, value) => {
            is_constant(value, params, evaluation, depth + 1)
        }
        Expr::List(values) => values
            .iter()
            .all(|value| is_constant(value, params, evaluation, depth + 1)),
        Expr::Map(values) => values
            .values()
            .all(|value| is_constant(value, params, evaluation, depth + 1)),
        Expr::Function {
            name,
            args,
            distinct: false,
            star: false,
        } => {
            bounded_constructor(name, args, params)
                && args
                    .iter()
                    .all(|value| is_constant(value, params, evaluation, depth + 1))
        }
        _ => false,
    }
}

/// Inspect borrowed inputs and prepay their clone work before evaluation can
/// allocate. All attempts draw from the same query's optional planning budget.
fn constant_value_fits(value: &Value, evaluation: &SeekEvaluation<'_, '_>, depth: usize) -> bool {
    if depth > MAX_DEPTH || !evaluation.check_work() {
        return false;
    }
    match value {
        Value::List(values) => {
            evaluation.check_units(values.len())
                && values
                    .iter()
                    .all(|value| constant_value_fits(value, evaluation, depth + 1))
        }
        Value::Map(values) => {
            evaluation.check_units(values.len())
                && values.iter().all(|(key, value)| {
                    evaluation.check_units(key.len().div_ceil(64))
                        && constant_value_fits(value, evaluation, depth + 1)
                })
        }
        Value::Node(_) | Value::Relationship(_) | Value::Path(_) => false,
        value => match crate::query::resources::value_bytes(value) {
            Ok(bytes) => {
                evaluation.check_units(usize::try_from(bytes.div_ceil(64)).unwrap_or(usize::MAX))
            }
            Err(error) => {
                *evaluation.error.borrow_mut() = Some(error);
                false
            }
        },
    }
}

// Optional access preparation must not evaluate volatile/clock/external
// functions or expanding constructors such as range()/repeat(). These few
// constructors produce a value bounded by already inspected input capacity.
fn bounded_constructor(name: &str, args: &[Expr], params: &BTreeMap<String, Value>) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "point" | "duration" | "uuid" => args.len() == 1,
        "vector" => {
            matches!(args.first(), Some(Expr::List(_)))
                || matches!(args.first(),
            Some(Expr::Parameter(key)) if matches!(params.get(key), Some(Value::List(_))))
        }
        "date" | "time" | "localtime" | "datetime" | "localdatetime" => {
            matches!(args, [Expr::Literal(Value::String(_))])
                || matches!(args,
                [Expr::Parameter(key)] if matches!(params.get(key), Some(Value::String(_))))
        }
        _ => false,
    }
}

pub(super) fn predicate_bytes(
    predicate: &StandardIndexPredicate,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    use StandardIndexPredicate as P;
    let value_bytes = |value| crate::query::resources::value_bytes_controlled(value, control);
    let dynamic = match predicate {
        P::Equal(value) | P::Less(value, _) | P::Greater(value, _) => value_bytes(value)?,
        P::In(values) => crate::query::resources::owned_row_bytes_controlled(values, control)?,
        P::Bounds { lower, upper } => {
            lower.iter().chain(upper).try_fold(0, |bytes, (value, _)| {
                checked_add(bytes, value_bytes(value)?)
            })?
        }
        P::StartsWith(value) | P::EndsWith(value) | P::Contains(value) => value.capacity() as u64,
        P::WithinBBox { lower, upper } => checked_add(value_bytes(lower)?, value_bytes(upper)?)?,
        P::Distance { center, radius, .. } => {
            checked_add(value_bytes(center)?, value_bytes(radius)?)?
        }
        P::Lookup | P::IsNotNull => 0,
    };
    checked_add(
        std::mem::size_of::<StandardIndexPredicate>() as u64,
        dynamic,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn folding_spends_the_existing_budget_before_clone_without_hiding_resource_errors() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
        crate::storage::create_storage_schema(&db).unwrap();
        let root = crate::storage::initialize_root(&db).unwrap().root;
        let snapshot = Snapshot::resolve(&db, root).unwrap();
        let params = BTreeMap::new();
        let access = PropertyRules::default();
        for consumed in [4095, 0] {
            let resources = crate::query::QueryResources::new(Some(256));
            let control = ExecutionControl::new(&resources, &|| false);
            let mut budget = PlanningBudget::default();
            assert!(budget.work(consumed, 0));
            let evaluation =
                SeekEvaluation::new(&snapshot, &params, &access, &control, &mut budget).unwrap();
            assert!(
                evaluation
                    .value(&Expr::Literal(Value::String("x".repeat(2048))))
                    .is_none()
            );
            if consumed == 0 {
                assert_eq!(
                    evaluation.check().unwrap_err().kind,
                    QueryErrorKind::Resource
                );
                assert!(!evaluation.exhausted());
            } else {
                evaluation.check().unwrap();
                assert!(evaluation.exhausted());
                assert_eq!(
                    resources.peak(),
                    0,
                    "no fold allocation before budget fallback"
                );
            }
            drop(evaluation);
            assert_eq!(resources.used(), 0);
        }
        let resources = crate::query::QueryResources::default();
        let armed = Cell::new(false);
        let interrupted = || armed.get();
        let control = ExecutionControl::new(&resources, &interrupted);
        let mut budget = PlanningBudget::default();
        assert!(budget.work(4096, 0));
        let evaluation =
            SeekEvaluation::new(&snapshot, &params, &access, &control, &mut budget).unwrap();
        armed.set(true);
        assert!(
            evaluation
                .value(&Expr::Literal(Value::Integer(1)))
                .is_none()
        );
        assert!(evaluation.exhausted());
        assert_eq!(
            evaluation.check().unwrap_err().kind,
            QueryErrorKind::Interrupted
        );
    }
}
