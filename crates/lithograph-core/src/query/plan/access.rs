//! Choose existing leaf access and a proved, bounded left-deep order for the
//! same MatchStep cursor. Statistics remain advisory, never membership proof.

use super::*;
use crate::query::resources::{ExecutionControl, MemoryReservation};
use crate::query::schema::{StandardIndexPredicate, StandardIndexSeek};

use super::budget::PlanningBudget;
mod sampling;

pub(crate) fn reserve_match_seeks(
    step: &MatchStep,
    control: &ExecutionControl<'_>,
) -> QueryResult<MemoryReservation> {
    let mut bytes = 0;
    for part in &step.parts {
        bytes = crate::query::resources::checked_add(bytes, part_seek_bytes(part, control)?)?;
    }
    control.reserve(bytes)
}

pub(crate) fn part_seek_bytes(
    part: &PatternPart,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    [
        part.start.index_seek.as_ref(),
        part.relationship
            .as_ref()
            .and_then(|rel| rel.index_seek.as_ref()),
    ]
    .into_iter()
    .flatten()
    .try_fold(0, |bytes, seek| {
        crate::query::resources::checked_add(
            bytes,
            crate::query::schema::seek_bytes(seek, control)?,
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessPath {
    Scan,
    Label(LabelId),
    Index,
    Identity,
}

#[derive(Debug, Clone, Copy)]
struct AccessCandidate {
    path: AccessPath,
    // A heuristic expectation is never an exact row count or membership proof.
    // Missing statistics stay None; even a zero estimate still executes.
    rows: Option<(u64, u64)>,
    io: u64,
    cpu: u64,
    working_bytes: u64,
    indexed_owners: Option<u64>,
}

impl AccessCandidate {
    fn scan(path: AccessPath, rows: Option<u64>) -> Self {
        Self {
            path,
            rows: rows.map(|rows| (rows, rows)),
            io: rows.unwrap_or(u64::MAX / 8),
            cpu: rows.unwrap_or(u64::MAX / 8).saturating_mul(2),
            working_bytes: 256 * 8,
            indexed_owners: None,
        }
    }

    fn score(self) -> u64 {
        self.io
            .saturating_add(self.cpu)
            .saturating_add(self.working_bytes / 4096)
    }

    fn beats(self, old: Self) -> bool {
        // Unknown estimates keep the established access priority. An exact
        // label bound can compete only against a cost with a known domain.
        if old.path == AccessPath::Index
            && (old.rows.is_some_and(|(lower, upper)| lower != upper)
                || old.indexed_owners.is_none_or(|owners| self.io >= owners))
            && self.rows.is_some_and(|(_, upper)| upper >= old.cpu / 3)
        {
            // Without value-frequency statistics, startup alone is not proof
            // that discarding an established index path will reduce work.
            return false;
        }
        self.rows.is_some() && old.rows.is_some() && self.score() < old.score()
    }
}

pub(crate) fn plan_access(
    snapshot: &storage::Snapshot<'_>,
    view: &ResolvedGraphView,
    matches: &mut [MatchStep],
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
    scope: Option<&join::JoinClauseScope<'_>>,
) -> QueryResult<(PlannerStatistics, MemoryReservation)> {
    control.check()?;
    let owned_schema;
    let schema = if let Some(schema) = snapshot.query_schema_state() {
        schema
    } else {
        owned_schema = snapshot.schema_state()?;
        &owned_schema
    };
    let mut memory = control.reserve(0)?;
    let mut statistics = PlannerStatistics::default();
    let context = join::JoinPlanningContext {
        snapshot,
        schema,
        view,
        params,
        control,
    };
    for step in matches {
        let (selected_stats, selected_memory) = plan_step_access(step, &context, budget, scope)?;
        statistics = selected_stats;
        memory.merge(selected_memory)?;
    }
    control.check()?;
    Ok((statistics, memory))
}

fn plan_step_access(
    step: &mut MatchStep,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    scope: Option<&join::JoinClauseScope<'_>>,
) -> QueryResult<(PlannerStatistics, MemoryReservation)> {
    let join::JoinPlanningContext {
        snapshot,
        schema,
        view,
        params,
        control,
    } = *context;
    control.check()?;
    if !eligible_step(step, schema, params, control, budget)? {
        return Ok((PlannerStatistics::default(), control.reserve(0)?));
    }
    select_node_id_seeks(step, params);
    select_relationship_id_seeks(step, params);
    let memory = super::super::schema::select_standard_index_seeks(
        schema,
        std::slice::from_mut(step),
        params,
        &view.query_access,
        snapshot,
        control,
        budget,
    )?;
    let analysis = analyze_step_join(step, scope, context, budget)?;
    if let (Some(scope), Some(analysis)) = (scope, analysis.as_ref()) {
        join::estimates::protect_completion_metadata(step, analysis, scope, context, budget)?;
    }
    let statistics = step_statistics(snapshot, step, view, budget, control, analysis.is_some())?;
    choose_parts(step, view, snapshot, schema, &statistics, budget, control)?;
    if let (Some(scope), Some(analysis)) = (scope, analysis) {
        let estimates =
            join::collect_join_estimates(step, &analysis, context, &statistics, budget, scope)?;
        step.join = join::choose_join_order(
            step,
            scope,
            analysis,
            &estimates,
            context,
            &statistics,
            budget,
        )?;
        install_legacy_factor(step, scope, context, &statistics, &estimates, budget)?;
    }
    Ok((statistics, memory))
}

fn analyze_step_join(
    step: &MatchStep,
    scope: Option<&join::JoinClauseScope<'_>>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<join::JoinAnalysis>> {
    let Some(scope) = scope else {
        return Ok(None);
    };
    let bytes = step
        .parts
        .len()
        .saturating_mul(4)
        .saturating_mul(std::mem::size_of::<ProfileBinding>());
    if !budget.work(0, bytes) {
        return Ok(None);
    }
    join::analyze_join(step, scope, context, budget)
}

fn install_legacy_factor(
    step: &mut MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<()> {
    if budget.clause_scoped() {
        return Ok(());
    }
    super::factor::install(step, scope, context, statistics, estimates, budget)
}

fn step_statistics(
    snapshot: &storage::Snapshot<'_>,
    step: &MatchStep,
    view: &ResolvedGraphView,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
    joining: bool,
) -> QueryResult<PlannerStatistics> {
    let need_relationships = step
        .parts
        .iter()
        .any(|part| collects_relationships(part) || (joining && part.relationship.is_some()));
    if !needs_step_statistics(step, view, joining, need_relationships) {
        return Ok(PlannerStatistics::default());
    }
    let label_slots = statistics_label_slots(step, view, joining);
    if label_slots > 128 {
        return Ok(PlannerStatistics::default());
    }
    let work = snapshot.bounded_statistics_work(label_slots, step.parts.len(), need_relationships);
    if work > budget.remaining_states() || 64 * 1024 > budget.remaining_bytes() {
        return Ok(PlannerStatistics::default());
    }
    let Some(_probe) = sampling::claim(1, 0, budget, control)? else {
        return Ok(PlannerStatistics::default());
    };
    let Some(decode_work) = snapshot.bounded_statistics_decode_work()? else {
        return Ok(PlannerStatistics::default());
    };
    let work = work.saturating_add(decode_work);
    let _memory = match sampling::claim(work, 64 * 1024, budget, control)? {
        Some(memory) => memory,
        None => return Ok(PlannerStatistics::default()),
    };
    let (labels, types) = requested_statistics(step, view, joining);
    if (labels.is_empty() && types.is_empty() && !need_relationships) || view.is_empty() {
        return Ok(PlannerStatistics::default());
    }
    PlannerStatistics::bounded(snapshot, &labels, &types, need_relationships, work)
}

fn requested_statistics(
    step: &MatchStep,
    view: &ResolvedGraphView,
    joining: bool,
) -> (BTreeSet<LabelId>, BTreeSet<RelationshipTypeId>) {
    let mut labels = candidate_labels(step, view);
    if joining {
        labels.extend(
            step.parts
                .iter()
                .flat_map(|part| part.start.labels.iter())
                .copied(),
        );
        labels.extend(view.required_scan_labels().iter().copied());
        for end in step.parts.iter().filter_map(|part| part.end.as_ref()) {
            labels.extend(end.labels.iter().copied());
        }
    }
    let types = step
        .parts
        .iter()
        .filter(|part| joining || collects_relationships(part))
        .filter_map(|part| part.relationship.as_ref().and_then(|rel| rel.type_id))
        .collect::<BTreeSet<_>>();
    (labels, types)
}

fn needs_step_statistics(
    step: &MatchStep,
    view: &ResolvedGraphView,
    joining: bool,
    relationships: bool,
) -> bool {
    !view.is_empty()
        && (joining
            || relationships
            || step
                .parts
                .iter()
                .any(|part| collects_node_labels(part, view)))
}

fn statistics_label_slots(step: &MatchStep, view: &ResolvedGraphView, joining: bool) -> usize {
    step.parts
        .iter()
        .try_fold(view.required_scan_labels().len(), |count, part| {
            let end = if joining {
                part.end.as_ref().map_or(0, |end| end.labels.len())
            } else {
                0
            };
            count
                .checked_add(part.start.labels.len())
                .and_then(|count| count.checked_add(end))
        })
        .unwrap_or(usize::MAX)
}

fn choose_parts(
    step: &mut MatchStep,
    view: &ResolvedGraphView,
    snapshot: &storage::Snapshot<'_>,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for part in &mut step.parts {
        control.work(1)?;
        if part
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.id_seek.is_some())
        {
            continue;
        }
        choose_node_access(
            &mut part.start,
            view,
            snapshot,
            schema,
            statistics,
            budget,
            control,
        )?;
        if sampling::prefer_adjacency(part, view, snapshot, schema, statistics, budget, control)?
            && let Some(rel) = part.relationship.as_mut()
        {
            rel.index_seek = None;
        }
        // Direct start identity plus adjacency takes priority over a global
        // relationship index on this existing match cursor.
        if part.start.id_seek.is_some()
            && let Some(relationship) = &mut part.relationship
        {
            relationship.index_seek = None;
        }
    }
    Ok(())
}

fn eligible_step(
    step: &MatchStep,
    schema: &storage::SchemaState,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let before = budget.bytes;
    let before_states = budget.states;
    if !budget.step(step, schema) {
        // Unsupported expressions also require the established graph/program
        // executor; never cache an unoptimized step in the scalar matcher.
        if !budget.exhausted() {
            budget.stop();
        }
        return Ok(false);
    }
    Ok(step.predicate.is_none()
        || folding_fits(
            params,
            schema,
            step.parts.len(),
            budget.bytes - before,
            budget.states - before_states,
            budget,
            control,
        )?)
}

fn folding_fits(
    params: &BTreeMap<String, Value>,
    schema: &storage::SchemaState,
    parts: usize,
    inspected_bytes: usize,
    inspected_states: usize,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if schema.indexes.is_empty() {
        return Ok(true);
    }
    let before_parameters = budget.states;
    let Some(bytes) = budget.parameter_bytes(params, control)? else {
        return Ok(false);
    };
    let properties = schema
        .indexes
        .values()
        .map(|index| match &index.target {
            storage::IndexTarget::NodeProperties { properties, .. }
            | storage::IndexTarget::RelationshipProperties { properties, .. } => properties.len(),
            _ => 1,
        })
        .sum::<usize>();
    // Bound the existing eligibility/proof and temporary cloned seek workspace
    // before invoking it. Repeated clauses pay only their own inspection;
    // earlier sampling/search states are not folded again for every Index.
    // The retained execution values keep their old lease.
    Ok(budget.work(
        properties.saturating_mul(
            inspected_states
                .saturating_add(budget.states - before_parameters)
                .max(1),
        ),
        bytes
            .saturating_add(inspected_bytes)
            .saturating_add(128)
            .saturating_mul(properties + 1)
            .saturating_mul(4)
            .saturating_mul(parts),
    ))
}

fn candidate_labels(step: &MatchStep, view: &ResolvedGraphView) -> BTreeSet<LabelId> {
    let mut labels = BTreeSet::new();
    for part in &step.parts {
        let node = &part.start;
        if collects_node_labels(part, view) {
            labels.extend(node.labels.iter().copied());
            labels.extend(view.required_scan_labels().iter().copied());
        }
    }
    labels
}

fn collects_node_labels(part: &PatternPart, view: &ResolvedGraphView) -> bool {
    let node = &part.start;
    if node.id_seek.is_some()
        || node.impossible
        || part
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.id_seek.is_some())
    {
        return false;
    }
    let alternatives = node
        .labels
        .len()
        .saturating_add(view.required_scan_labels().len());
    super::node_needs_scan_statistics(node)
        || alternatives > 1
        || (node.index_seek.is_some() && alternatives > 0)
}

fn collects_relationships(part: &PatternPart) -> bool {
    part.start.id_seek.is_none()
        && part
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.id_seek.is_none() && rel.index_seek.is_some())
}

fn choose_node_access(
    node: &mut NodeSpec,
    view: &ResolvedGraphView,
    snapshot: &storage::Snapshot<'_>,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if node.impossible || !budget.candidate() {
        return Ok(());
    }
    let mut best = initial_candidate(node, view, schema, statistics);
    if best.path == AccessPath::Identity {
        return Ok(());
    }
    let combination = sampling::combination(node, view, snapshot, statistics, budget, control)?;
    if let (AccessPath::Label(_), Some(sample)) = (best.path, combination) {
        best.cpu = best.io.saturating_add(sample.expected);
        best.rows = Some((sample.lower, sample.upper));
    }
    if let Some(estimate) =
        observed_index(node, view, snapshot, schema, statistics, budget, control)?
    {
        best.rows = Some((estimate.rows.lower, estimate.rows.upper));
        best.io = estimate.rows.expected.saturating_add(16);
        best.cpu = estimate.rows.expected.saturating_mul(3);
        best.indexed_owners = Some(estimate.indexed_owners);
    }
    for &label in node.labels.iter().chain(view.required_scan_labels()) {
        if !budget.candidate() {
            break;
        }
        let mut candidate =
            AccessCandidate::scan(AccessPath::Label(label), statistics.label_count(label));
        if let Some(sample) = combination {
            candidate.cpu = candidate.io.saturating_add(sample.expected);
            candidate.rows = Some((sample.lower, sample.upper));
        }
        if candidate.beats(best) {
            best = candidate;
        }
    }
    if let AccessPath::Label(label) = best.path {
        // Node Lookup is defined to use the canonical label B-tree. Retain
        // that versioned definition when choosing its physical label prefix.
        if node
            .index_seek
            .as_ref()
            .is_none_or(|seek| seek.kind != StandardIndexKind::Lookup)
        {
            node.index_seek = None;
        }
        node.scan_label = Some(label);
        node.scan_label_name = storage::label_name(snapshot.connection_for_query(), label)?;
    }
    Ok(())
}

fn observed_index(
    node: &NodeSpec,
    view: &ResolvedGraphView,
    snapshot: &storage::Snapshot<'_>,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<crate::query::stats::IndexEstimate>> {
    let Some(seek) = node.index_seek.as_ref() else {
        return Ok(None);
    };
    if !matches!(
        seek.predicates.as_slice(),
        [(0, StandardIndexPredicate::Equal(_))]
    ) {
        return Ok(None);
    }
    let Some(index) = schema.indexes.get(&seek.index_name) else {
        return Ok(None);
    };
    if !matches!(&index.target, storage::IndexTarget::NodeProperties { properties, .. } if properties.len() == 1)
    {
        return Ok(None);
    }
    let Some(domain) = index_domain(node, index, statistics) else {
        return Ok(None);
    };
    // This access site keeps a producer covering the entire indexed domain.
    // Collect a frequency only when a genuinely smaller Label can compete.
    if !node
        .labels
        .iter()
        .chain(view.required_scan_labels())
        .any(|id| {
            statistics
                .label_count(*id)
                .is_some_and(|rows| rows < domain)
        })
    {
        return Ok(None);
    }
    let Some(_memory) = sampling::claim(1024, 16 * 1024, budget, control)? else {
        return Ok(None);
    };
    super::super::schema::estimate_index_access(snapshot, index, seek, &view.query_access, control)
}

fn initial_candidate(
    node: &NodeSpec,
    view: &ResolvedGraphView,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
) -> AccessCandidate {
    if node.id_seek.is_some() {
        return AccessCandidate {
            path: AccessPath::Identity,
            rows: Some((0, 1)),
            io: 1,
            cpu: 1,
            working_bytes: 8,
            indexed_owners: None,
        };
    }
    if let Some(seek) = &node.index_seek
        && seek.kind != StandardIndexKind::Lookup
    {
        return index_candidate(node, seek, schema, statistics);
    }
    let label = node.scan_label.or_else(|| view.scan_label());
    AccessCandidate::scan(
        label.map_or(AccessPath::Scan, AccessPath::Label),
        label.and_then(|label| statistics.label_count(label)),
    )
}

fn index_candidate(
    node: &NodeSpec,
    seek: &StandardIndexSeek,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
) -> AccessCandidate {
    let domain = schema
        .indexes
        .get(&seek.index_name)
        .and_then(|index| index_domain(node, index, statistics));
    let (divisor, probes) = selectivity(seek);
    let expected = domain.map(|domain| domain.div_ceil(divisor).saturating_mul(probes));
    AccessCandidate {
        path: AccessPath::Index,
        rows: domain.map(|domain| (0, domain)),
        io: expected
            .unwrap_or(u64::MAX / 8)
            .saturating_add(16_u64.saturating_mul(probes)),
        cpu: expected.unwrap_or(u64::MAX / 8).saturating_mul(3),
        working_bytes: 16_384,
        indexed_owners: domain,
    }
}

fn index_domain(
    node: &NodeSpec,
    index: &storage::IndexDefinition,
    statistics: &PlannerStatistics,
) -> Option<u64> {
    let storage::IndexTarget::NodeProperties { label, .. } = &index.target else {
        return None;
    };
    let ordinal = node.label_names.iter().position(|name| name == label)?;
    statistics.label_count(*node.labels.get(ordinal)?)
}

fn selectivity(seek: &StandardIndexSeek) -> (u64, u64) {
    let Some((_, predicate)) = seek.predicates.first() else {
        return (1, 1);
    };
    match predicate {
        StandardIndexPredicate::Equal(_) => (16, 1),
        StandardIndexPredicate::In(values) => (16, (values.len() as u64).max(1)),
        StandardIndexPredicate::StartsWith(prefix) if !prefix.is_empty() => (4, 1),
        StandardIndexPredicate::Less(_, _)
        | StandardIndexPredicate::Greater(_, _)
        | StandardIndexPredicate::Bounds { .. }
        | StandardIndexPredicate::WithinBBox { .. }
        | StandardIndexPredicate::Distance { .. } => (3, 1),
        _ => (1, 1),
    }
}

#[cfg(test)]
mod tests;
