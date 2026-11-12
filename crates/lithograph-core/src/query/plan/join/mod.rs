//! A bounded physical order for the existing MATCH cursor. Original pattern
//! parts and the complete WHERE remain the semantic source of truth.

use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::expression::Expr;
use crate::query::graph::ResolvedGraphView;
use crate::query::resources::ExecutionControl;
use crate::storage::{HashId, SchemaState, Snapshot};

mod analysis;
mod cost;
mod enumerate;
mod estimates;
mod parts;
pub(crate) use analysis::analyze_join;
pub(crate) use enumerate::choose_join_order;
pub(crate) use estimates::collect_join_estimates;
pub(crate) use parts::{execution_part, expansion_metadata};

pub(crate) type PartId = usize;
pub(crate) type TemplateId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PartSet(pub(crate) u128);

pub(crate) struct JoinClauseScope<'a> {
    pub(crate) clause_start: usize,
    pub(crate) snapshot_identity: HashId,
    pub(crate) incoming: &'a [String],
    pub(crate) declared_columns: &'a [String],
}

pub(crate) struct JoinPlanningContext<'q, 'db, 'ctl> {
    pub(crate) snapshot: &'q Snapshot<'db>,
    pub(crate) schema: &'q SchemaState,
    pub(crate) view: &'q ResolvedGraphView,
    pub(crate) params: &'q BTreeMap<String, Value>,
    pub(crate) control: &'q ExecutionControl<'ctl>,
}

#[derive(Debug, Clone)]
pub(crate) struct JoinPredicate {
    pub(crate) expression: Expr,
    /// All groups must be ready; one completed provider in each group suffices.
    pub(crate) guards: Vec<PartSet>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Hyperedge {
    pub(crate) left: PartSet,
    pub(crate) right: PartSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeTarget {
    StartNode,
    EndNode,
    Relationship,
}

#[derive(Debug, Clone)]
pub(crate) enum ProbeAccess {
    Identity {
        source_variable: String,
    },
    IndexEquality {
        source_variable: String,
        source_property: String,
        index_name: String,
        kind: crate::storage::StandardIndexKind,
        definition_hash: HashId,
    },
    Adjacency {
        source_variable: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ParameterTemplate {
    pub(crate) part: PartId,
    pub(crate) target: ProbeTarget,
    /// One any-provider group containing source-side sufficient proofs only.
    pub(crate) source_guard: PartSet,
    pub(crate) access: ProbeAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashFamily {
    Boolean,
    Number,
    String,
    NodeIdentity,
    RelationshipIdentity,
}

#[derive(Debug, Clone)]
pub(crate) struct HashInputKey {
    pub(crate) variable: String,
    pub(crate) property: Option<String>,
    /// Each provider independently binds this value and proves its family.
    pub(crate) providers: PartSet,
}

#[derive(Debug, Clone)]
pub(crate) struct HashEquality {
    pub(crate) predicate: usize,
    pub(crate) left: HashInputKey,
    pub(crate) right: HashInputKey,
    pub(crate) family: HashFamily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashOperandSide {
    Left,
    Right,
}

impl HashEquality {
    pub(crate) fn keys(&self, inner: HashOperandSide) -> (&HashInputKey, &HashInputKey) {
        match inner {
            HashOperandSide::Left => (&self.left, &self.right),
            HashOperandSide::Right => (&self.right, &self.left),
        }
    }
}

#[derive(Debug)]
pub(crate) struct JoinAnalysis {
    pub(crate) predicates: Vec<JoinPredicate>,
    pub(crate) edges: Vec<Hyperedge>,
    pub(crate) templates: Vec<ParameterTemplate>,
    pub(crate) hashes: Vec<HashEquality>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartOrientation {
    Written,
    Reversed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageAccess {
    Existing,
    Probe(TemplateId),
    Hash {
        equality: usize,
        inner_side: HashOperandSide,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct JoinStage {
    pub(crate) part: PartId,
    pub(crate) orientation: PartOrientation,
    pub(crate) access: StageAccess,
    pub(crate) cartesian: bool,
    pub(crate) predicates_after: Vec<usize>,
}

#[derive(Debug)]
pub(crate) struct JoinPlan {
    pub(crate) clause_start: usize,
    pub(crate) snapshot_identity: HashId,
    pub(crate) declared_columns: Vec<String>,
    pub(crate) predicates: Vec<JoinPredicate>,
    pub(crate) templates: Vec<ParameterTemplate>,
    pub(crate) hashes: Vec<HashEquality>,
    pub(crate) stages: Vec<JoinStage>,
}
