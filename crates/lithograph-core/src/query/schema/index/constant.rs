use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::expression::{self, BindingRow, Expr, UnaryOp};
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
    ) -> QueryResult<Self> {
        Ok(Self {
            snapshot,
            params,
            access,
            control,
            folded: RefCell::new(control.reserve(0)?),
            retained: control.reserve(0)?,
            error: RefCell::new(None),
        })
    }

    pub(super) fn value(&self, expression: &Expr) -> Option<Value> {
        if self.error.borrow().is_some() || !is_constant(expression) {
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

    /// Proof failure may select a scan, but cancellation must survive that
    /// optional-proof API and be returned by `check`/`finish`.
    pub(super) fn check_work(&self) -> bool {
        if self.error.borrow().is_some() {
            return false;
        }
        match self.control.work(1) {
            Ok(()) => true,
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

fn is_constant(expression: &Expr) -> bool {
    match expression {
        Expr::Literal(_) | Expr::Parameter(_) => true,
        Expr::Unary(UnaryOp::Positive | UnaryOp::Negative, value) => is_constant(value),
        Expr::List(values) => values.iter().all(is_constant),
        Expr::Map(values) => values.values().all(is_constant),
        Expr::Function {
            args,
            distinct: false,
            star: false,
            ..
        } => args.iter().all(is_constant),
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
