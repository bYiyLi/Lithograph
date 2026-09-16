use super::*;

mod delete;
mod delta;
mod matcher;
mod program;
mod value;

pub(crate) use program::{
    TransactionBatchOutcome, TransactionMutationContext, execute_program,
    execute_program_suffix_transaction, execute_transaction_batch,
};
pub(crate) use value::property_from_value;

use delete::apply_delete;
use delta::{property_states_equal, staged_snapshot};
use matcher::match_pattern;
use value::{
    binding_node, binding_owner, evaluate_map, project_rows, require_visible_owner,
    set_property_map, validate_clause_view,
};

struct MutationContext<'connection, 'query> {
    connection: &'connection Connection,
    base: Snapshot<'connection>,
    staged: Snapshot<'connection>,
    base_commit: HashId,
    params: &'query BTreeMap<String, Value>,
    graph_view_selector: &'query GraphViewSelector,
    delta: DeltaBuilder,
    global_bindings: BindingRow,
}

impl<'connection, 'query> MutationContext<'connection, 'query> {
    fn new(
        connection: &'connection Connection,
        base_commit: HashId,
        params: &'query BTreeMap<String, Value>,
        graph_view_selector: &'query GraphViewSelector,
    ) -> QueryResult<Self> {
        let base = Snapshot::resolve(connection, base_commit)?;
        Ok(Self {
            connection,
            staged: base.clone(),
            base,
            base_commit,
            params,
            graph_view_selector,
            delta: DeltaBuilder::default(),
            global_bindings: BindingRow::default(),
        })
    }

    fn staged_snapshot(&self) -> QueryResult<Snapshot<'connection>> {
        Ok(self.staged.clone())
    }

    fn set_node(&mut self, id: i64, current: bool) -> QueryResult<()> {
        self.delta.set_node(&self.base, id, current)?;
        let mut layer = LayerBuilder::default();
        if current {
            layer.add_node(id)?;
        } else {
            layer.remove_node(id)?;
        }
        self.staged.apply_layer(&layer)?;
        Ok(())
    }

    fn set_label(&mut self, node_id: i64, label_id: i64, current: bool) -> QueryResult<()> {
        self.delta
            .set_label(&self.base, node_id, label_id, current)?;
        let mut layer = LayerBuilder::default();
        if current {
            layer.add_label(node_id, label_id)?;
        } else {
            layer.remove_label(node_id, label_id)?;
        }
        self.staged.apply_layer(&layer)?;
        Ok(())
    }

    fn set_relationship(
        &mut self,
        id: i64,
        current: Option<RelationshipRecord>,
    ) -> QueryResult<()> {
        let previous = self.staged.relationship(id)?;
        self.delta.set_relationship(&self.base, id, current)?;
        let mut layer = LayerBuilder::default();
        match current {
            Some(record) => layer.add_relationship(record)?,
            None => {
                if let Some(record) = previous {
                    layer.remove_relationship(record)?;
                }
            }
        }
        self.staged.apply_layer(&layer)?;
        Ok(())
    }

    fn set_property(
        &mut self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: i64,
        current: Option<PropertyValue>,
    ) -> QueryResult<()> {
        self.delta
            .set_property(&self.base, owner_kind, owner_id, key_id, current.clone())?;
        let mut layer = LayerBuilder::default();
        match current {
            Some(value) => layer.set_property(owner_kind, owner_id, key_id, value)?,
            None => layer.remove_property(owner_kind, owner_id, key_id)?,
        }
        self.staged.apply_layer(&layer)?;
        Ok(())
    }

    fn graph_view(&self) -> QueryResult<ResolvedGraphView> {
        ResolvedGraphView::resolve(self.connection, self.graph_view_selector)
    }

    fn validate_view(&self, touched: &TouchedElements) -> QueryResult<()> {
        validate_clause_view(
            self.connection,
            self.base_commit,
            &self.delta,
            self.graph_view_selector,
            touched,
        )
    }
}

pub(crate) fn execute_write(
    connection: &Connection,
    prepared: &PreparedWrite,
    base_commit: HashId,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<WriteOutcome> {
    let mut context = MutationContext::new(connection, base_commit, params, &prepared.graph_view)?;
    let mut rows = vec![BindingRow::default()];
    for clause in &prepared.clauses {
        check_interrupted(is_interrupted)?;
        rows = execute_clause(&mut context, clause, rows, metrics, is_interrupted)?;
        check_interrupted(is_interrupted)?;
    }
    finish_write(context, prepared, rows, is_interrupted)
}

fn finish_write(
    context: MutationContext<'_, '_>,
    prepared: &PreparedWrite,
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<WriteOutcome> {
    check_interrupted(is_interrupted)?;
    let final_layer = context.delta.layer()?;
    let counters = context.delta.counters()?;
    let final_snapshot =
        Snapshot::resolve_with_layer(context.connection, context.base_commit, &final_layer)?;
    crate::query::schema::validate_layer_against_commit_schema(
        context.connection,
        context.base_commit,
        &final_snapshot,
        &final_layer,
    )?;
    let output = match &prepared.projection {
        Some(projection) => project_rows(&final_snapshot, context.params, rows, projection)?,
        None => Vec::new(),
    };
    check_interrupted(is_interrupted)?;
    let metadata = CommitMetadata {
        author: prepared.author.clone(),
        message: prepared.message.clone(),
        committed_at: now_micros()?,
    };
    let commit = storage::commit_layer(
        context.connection,
        &prepared.branch,
        context.base_commit,
        None,
        &final_layer,
        &metadata,
    )?;
    Ok(WriteOutcome {
        rows: output,
        commit,
        counters,
    })
}

fn execute_clause(
    context: &mut MutationContext<'_, '_>,
    clause: &WriteClause,
    rows: Vec<BindingRow>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    match clause {
        WriteClause::Match { clause, optional } => {
            execute_match_clause(context, clause, *optional, rows, metrics, is_interrupted)
        }
        WriteClause::Create(pattern) => {
            execute_create_clause(context, pattern, rows, is_interrupted)
        }
        WriteClause::Set(items) => execute_set_clause(context, items, rows, is_interrupted),
        WriteClause::Remove(items) => execute_remove_clause(context, items, rows, is_interrupted),
        WriteClause::Delete {
            expressions,
            detach,
        } => execute_delete_clause(context, expressions, *detach, rows, is_interrupted),
        WriteClause::Merge(merge) => execute_merge_clause(context, merge, rows, is_interrupted),
    }
}

fn execute_match_clause(
    context: &MutationContext<'_, '_>,
    clause: &AstNode,
    optional: bool,
    rows: Vec<BindingRow>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let snapshot = context.staged_snapshot()?;
    let graph_view = context.graph_view()?;
    let step = lower_match(context.connection, clause, optional)?;
    materialize_match_step(
        &snapshot,
        &graph_view,
        context.params,
        rows,
        &step,
        metrics,
        is_interrupted,
    )
}

fn execute_create_clause(
    context: &mut MutationContext<'_, '_>,
    pattern: &[WritePatternPart],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut state = begin_mutation_clause(context)?;
    let rows = execute_create_clause_batch(context, pattern, rows, &mut state, is_interrupted)?;
    finish_mutation_clause(context, &state)?;
    Ok(rows)
}

struct MutationClauseState<'connection> {
    clause_input: Snapshot<'connection>,
    graph_view: ResolvedGraphView,
    touched: TouchedElements,
}

fn begin_mutation_clause<'connection>(
    context: &MutationContext<'connection, '_>,
) -> QueryResult<MutationClauseState<'connection>> {
    Ok(MutationClauseState {
        clause_input: context.staged_snapshot()?,
        graph_view: context.graph_view()?,
        touched: TouchedElements::default(),
    })
}

fn finish_mutation_clause(
    context: &MutationContext<'_, '_>,
    state: &MutationClauseState<'_>,
) -> QueryResult<()> {
    context.validate_view(&state.touched)
}

fn execute_create_clause_batch(
    context: &mut MutationContext<'_, '_>,
    pattern: &[WritePatternPart],
    mut rows: Vec<BindingRow>,
    state: &mut MutationClauseState<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    for row in &mut rows {
        check_interrupted(is_interrupted)?;
        create_pattern(
            context,
            row,
            pattern,
            &state.clause_input,
            &state.graph_view,
            &mut state.touched,
            is_interrupted,
        )?;
    }
    Ok(rows)
}

fn execute_set_clause(
    context: &mut MutationContext<'_, '_>,
    items: &[SetItem],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut state = begin_mutation_clause(context)?;
    let rows = execute_set_clause_batch(context, items, rows, &mut state, is_interrupted)?;
    finish_mutation_clause(context, &state)?;
    Ok(rows)
}

fn execute_remove_clause(
    context: &mut MutationContext<'_, '_>,
    items: &[RemoveItem],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut state = begin_mutation_clause(context)?;
    let rows = execute_remove_clause_batch(context, items, rows, &mut state, is_interrupted)?;
    finish_mutation_clause(context, &state)?;
    Ok(rows)
}

enum RowMutation<'a> {
    Set(&'a [SetItem]),
    Remove(&'a [RemoveItem]),
}

fn execute_row_mutation_batch(
    context: &mut MutationContext<'_, '_>,
    mutation: RowMutation<'_>,
    rows: Vec<BindingRow>,
    state: &mut MutationClauseState<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    for row in &rows {
        check_interrupted(is_interrupted)?;
        match mutation {
            RowMutation::Set(items) => apply_set_items(
                context,
                row,
                items,
                &state.clause_input,
                &state.graph_view,
                &mut state.touched,
            )?,
            RowMutation::Remove(items) => apply_remove_items(
                context,
                row,
                items,
                &state.clause_input,
                &state.graph_view,
                &mut state.touched,
            )?,
        }
    }
    Ok(rows)
}

fn execute_set_clause_batch(
    context: &mut MutationContext<'_, '_>,
    items: &[SetItem],
    rows: Vec<BindingRow>,
    state: &mut MutationClauseState<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    execute_row_mutation_batch(
        context,
        RowMutation::Set(items),
        rows,
        state,
        is_interrupted,
    )
}

fn execute_remove_clause_batch(
    context: &mut MutationContext<'_, '_>,
    items: &[RemoveItem],
    rows: Vec<BindingRow>,
    state: &mut MutationClauseState<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    execute_row_mutation_batch(
        context,
        RowMutation::Remove(items),
        rows,
        state,
        is_interrupted,
    )
}

fn execute_delete_clause(
    context: &mut MutationContext<'_, '_>,
    expressions: &[Expr],
    detach: bool,
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let state = begin_mutation_clause(context)?;
    let (variables, targets) = evaluate_delete_targets(context, expressions, &rows, &state)?;
    apply_delete(
        context,
        &targets,
        &variables,
        detach,
        &state.clause_input,
        &state.graph_view,
        is_interrupted,
    )?;
    Ok(rows)
}

fn delete_target_variables(expressions: &[Expr]) -> Vec<String> {
    (0..expressions.len())
        .map(|index| format!("__lithograph_delete_{index}"))
        .collect()
}

fn evaluate_delete_targets(
    context: &MutationContext<'_, '_>,
    expressions: &[Expr],
    rows: &[BindingRow],
    state: &MutationClauseState<'_>,
) -> QueryResult<(Vec<String>, Vec<BindingRow>)> {
    let variables = delete_target_variables(expressions);
    let targets = rows
        .iter()
        .map(|row| {
            let mut targets = BindingRow::default();
            for (variable, expression) in variables.iter().zip(expressions) {
                let value =
                    expression::evaluate(expression, &state.clause_input, row, context.params)?;
                targets.insert(
                    variable.clone(),
                    expression::binding_from_value(&state.clause_input, value)?,
                );
            }
            Ok(targets)
        })
        .collect::<QueryResult<Vec<_>>>()?;
    Ok((variables, targets))
}

fn execute_merge_clause(
    context: &mut MutationContext<'_, '_>,
    merge: &MergePlan,
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut state = begin_mutation_clause(context)?;
    let output = execute_merge_clause_batch(context, merge, rows, &mut state, is_interrupted)?;
    finish_mutation_clause(context, &state)?;
    Ok(output)
}

fn execute_merge_clause_batch(
    context: &mut MutationContext<'_, '_>,
    merge: &MergePlan,
    rows: Vec<BindingRow>,
    state: &mut MutationClauseState<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        check_interrupted(is_interrupted)?;
        output.extend(merge_one(
            context,
            row,
            merge,
            &state.clause_input,
            &state.graph_view,
            &mut state.touched,
            is_interrupted,
        )?);
    }
    Ok(output)
}

#[derive(Debug, Default)]
struct TouchedElements {
    nodes: BTreeSet<i64>,
    relationships: BTreeSet<i64>,
}

fn create_pattern(
    context: &mut MutationContext<'_, '_>,
    row: &mut BindingRow,
    pattern: &[WritePatternPart],
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    for part in pattern {
        check_interrupted(is_interrupted)?;
        create_pattern_part(
            context,
            row,
            part,
            clause_input,
            graph_view,
            touched,
            is_interrupted,
        )?;
    }
    Ok(())
}

fn create_pattern_part(
    context: &mut MutationContext<'_, '_>,
    row: &mut BindingRow,
    part: &WritePatternPart,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let mut node_ids = Vec::with_capacity(part.nodes.len());
    let mut relationships = Vec::with_capacity(part.relationships.len());
    let start = ensure_node(
        context,
        row,
        &part.nodes[0],
        clause_input,
        graph_view,
        touched,
    )?;
    node_ids.push(start);
    let mut current = start;
    for (index, relationship) in part.relationships.iter().enumerate() {
        check_interrupted(is_interrupted)?;
        let next = ensure_node(
            context,
            row,
            &part.nodes[index + 1],
            clause_input,
            graph_view,
            touched,
        )?;
        node_ids.push(next);
        let (source, target) = match relationship.direction {
            Direction::Outgoing => (current, next),
            Direction::Incoming => (next, current),
            Direction::Undirected => (current, next),
        };
        let record = create_relationship(
            context,
            row,
            relationship,
            source,
            target,
            clause_input,
            touched,
        )?;
        relationships.push(record);
        current = next;
    }
    if let Some(variable) = &part.path_variable {
        row.insert(
            variable.clone(),
            BindingValue::Path {
                nodes: node_ids,
                relationships,
            },
        );
    }
    Ok(())
}

fn ensure_node(
    context: &mut MutationContext<'_, '_>,
    row: &mut BindingRow,
    spec: &NodeWriteSpec,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<i64> {
    if let Some(id) = bound_node(context, row, spec, clause_input, graph_view)? {
        return Ok(id);
    }
    create_node(context, row, spec, clause_input, touched)
}

fn bound_node(
    context: &MutationContext<'_, '_>,
    row: &BindingRow,
    spec: &NodeWriteSpec,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
) -> QueryResult<Option<i64>> {
    let Some(variable) = &spec.variable else {
        return Ok(None);
    };
    let Some(binding) = row.values.get(variable) else {
        return Ok(None);
    };
    let BindingValue::Node(id) = binding else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            format!("CREATE variable {variable} is not a Node"),
        ));
    };
    let staged = context.staged_snapshot()?;
    require_visible_owner(graph_view, clause_input, &staged, row, variable)?;
    Ok(Some(*id))
}

fn create_node(
    context: &mut MutationContext<'_, '_>,
    row: &mut BindingRow,
    spec: &NodeWriteSpec,
    clause_input: &Snapshot<'_>,
    touched: &mut TouchedElements,
) -> QueryResult<i64> {
    let id = storage::allocate_node_id(context.connection)?;
    context.set_node(id, true)?;
    touched.nodes.insert(id);
    add_node_labels(context, row, id, &spec.labels, clause_input)?;
    apply_create_properties(
        context,
        row,
        OwnerKind::Node,
        id,
        spec.properties.as_ref(),
        clause_input,
    )?;
    if let Some(variable) = &spec.variable {
        row.insert(variable.clone(), BindingValue::Node(id));
    }
    Ok(id)
}

fn apply_create_properties(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    owner_kind: OwnerKind,
    owner_id: i64,
    properties: Option<&Expr>,
    clause_input: &Snapshot<'_>,
) -> QueryResult<()> {
    let Some(properties) = properties else {
        return Ok(());
    };
    let map = evaluate_map(properties, clause_input, row, context.params)?;
    set_new_property_map(context, owner_kind, owner_id, &map)
}

fn set_new_property_map(
    context: &mut MutationContext<'_, '_>,
    owner_kind: OwnerKind,
    owner_id: i64,
    map: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    for (key, value) in map {
        let Some(value) = property_from_value(value.clone())? else {
            continue;
        };
        let key_id = storage::intern_property_key(context.connection, key)?;
        context.set_property(owner_kind, owner_id, key_id, Some(value))?;
    }
    Ok(())
}

fn add_node_labels(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    id: i64,
    labels: &[WriteName],
    clause_input: &Snapshot<'_>,
) -> QueryResult<()> {
    let resolved = if labels
        .iter()
        .all(|label| matches!(label, WriteName::Static(_)))
    {
        labels
            .iter()
            .filter_map(|label| match label {
                WriteName::Static(label) => Some(label.clone()),
                WriteName::Dynamic(_) => None,
            })
            .collect()
    } else {
        resolve_write_names(labels, clause_input, row, context.params)?
    };
    for label in resolved {
        let label_id = storage::intern_label(context.connection, &label)?;
        context.set_label(id, label_id, true)?;
    }
    Ok(())
}

fn create_relationship(
    context: &mut MutationContext<'_, '_>,
    row: &mut BindingRow,
    spec: &RelationshipWriteSpec,
    source: i64,
    target: i64,
    clause_input: &Snapshot<'_>,
    touched: &mut TouchedElements,
) -> QueryResult<RelationshipRecord> {
    if let Some(variable) = &spec.variable
        && row.values.contains_key(variable)
    {
        return Err(QueryError::semantic(format!(
            "CREATE cannot reuse bound Relationship variable {variable}"
        )));
    }
    let relationship_types = resolve_write_names(
        std::slice::from_ref(&spec.relationship_type),
        clause_input,
        row,
        context.params,
    )?;
    if relationship_types.len() != 1 {
        return Err(QueryError::semantic(
            "dynamic Relationship Type expression must produce exactly one name",
        ));
    }
    let id = storage::allocate_relationship_id(context.connection)?;
    let type_id = storage::intern_relationship_type(context.connection, &relationship_types[0])?;
    let record = RelationshipRecord {
        id,
        source,
        type_id,
        target,
    };
    context.set_relationship(id, Some(record))?;
    touched.relationships.insert(id);
    apply_create_properties(
        context,
        row,
        OwnerKind::Relationship,
        id,
        spec.properties.as_ref(),
        clause_input,
    )?;
    if let Some(variable) = &spec.variable {
        row.insert(variable.clone(), BindingValue::Relationship(record));
    }
    Ok(record)
}

fn apply_set_items(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    items: &[SetItem],
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    for item in items {
        apply_set_item(context, row, item, clause_input, graph_view, touched)?;
    }
    Ok(())
}

fn apply_set_item(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    item: &SetItem,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let target = match item {
        SetItem::Property { variable, .. }
        | SetItem::Properties { variable, .. }
        | SetItem::Labels { variable, .. } => variable,
    };
    if matches!(row.values.get(target), Some(BindingValue::Null)) {
        return Ok(());
    }
    match item {
        SetItem::Property {
            variable,
            key,
            value,
        } => set_single_property(
            context,
            clause_input,
            row,
            variable,
            key,
            value,
            graph_view,
            touched,
        ),
        SetItem::Properties {
            variable,
            operator,
            value,
        } => {
            let staged = context.staged_snapshot()?;
            set_property_map_item(
                context,
                &staged,
                clause_input,
                row,
                variable,
                *operator,
                value,
                graph_view,
                touched,
            )
        }
        SetItem::Labels { variable, labels } => {
            let staged = context.staged_snapshot()?;
            mutate_labels(
                context,
                &staged,
                clause_input,
                row,
                variable,
                labels,
                graph_view,
                touched,
                LabelMutation::Add,
            )
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "SET keeps clause input, staged state, Graph View, and touched elements explicit"
)]
fn set_single_property(
    context: &mut MutationContext<'_, '_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    key: &WritePropertyKey,
    value: &Expr,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let (owner_kind, id) = visible_owner(&context.staged, clause_input, row, variable, graph_view)?;
    let key = resolve_property_key(key, &context.staged, row, context.params)?;
    let value = expression::evaluate(value, &context.staged, row, context.params)?;
    let next = property_from_value(value)?;
    let key_id = match &next {
        Some(_) => storage::intern_property_key(context.connection, &key)?,
        None => match storage::find_property_key(context.connection, &key)? {
            Some(key_id) => key_id,
            None => return Ok(()),
        },
    };
    let previous = context.staged.property(owner_kind, id, key_id)?;
    if !property_states_equal(&previous, &next)? {
        context.set_property(owner_kind, id, key_id, next.clone())?;
    }
    touched.insert(owner_kind, id);
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "SET map keeps clause input, staged state, Graph View, and touched elements explicit"
)]
fn set_property_map_item(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    operator: SetOperatorKind,
    value: &Expr,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let (owner_kind, id) = visible_owner(staged, clause_input, row, variable, graph_view)?;
    let map = evaluate_map(value, staged, row, context.params)?;
    set_property_map(
        context,
        staged,
        owner_kind,
        id,
        &map,
        operator == SetOperatorKind::Assign,
    )?;
    touched.insert(owner_kind, id);
    Ok(())
}

#[derive(Clone, Copy)]
enum LabelMutation {
    Add,
    Remove,
}

#[allow(
    clippy::too_many_arguments,
    reason = "label mutation keeps clause input, staged state, Graph View, and touched elements explicit"
)]
fn mutate_labels(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    labels: &[WriteName],
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    mutation: LabelMutation,
) -> QueryResult<()> {
    let id = binding_node(row, variable)?;
    require_visible_owner(graph_view, clause_input, staged, row, variable)?;
    let existing = staged.labels(id)?;
    for label in resolve_write_names(labels, staged, row, context.params)? {
        let label_id = match mutation {
            LabelMutation::Add => storage::intern_label(context.connection, &label)?,
            LabelMutation::Remove => {
                let Some(label_id) = storage::find_label(context.connection, &label)? else {
                    continue;
                };
                label_id
            }
        };
        let present = existing.binary_search(&label_id).is_ok();
        if matches!(mutation, LabelMutation::Add) && !present {
            context.set_label(id, label_id, true)?;
        } else if matches!(mutation, LabelMutation::Remove) && present {
            context.set_label(id, label_id, false)?;
        }
    }
    touched.nodes.insert(id);
    Ok(())
}

fn apply_remove_items(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    items: &[RemoveItem],
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    for item in items {
        apply_remove_item(context, row, item, clause_input, graph_view, touched)?;
    }
    Ok(())
}

fn apply_remove_item(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    item: &RemoveItem,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let target = match item {
        RemoveItem::Property { variable, .. } | RemoveItem::Labels { variable, .. } => variable,
    };
    if matches!(row.values.get(target), Some(BindingValue::Null)) {
        return Ok(());
    }
    let staged = context.staged_snapshot()?;
    match item {
        RemoveItem::Property { variable, key } => remove_property(
            context,
            &staged,
            clause_input,
            row,
            variable,
            key,
            graph_view,
            touched,
        ),
        RemoveItem::Labels { variable, labels } => mutate_labels(
            context,
            &staged,
            clause_input,
            row,
            variable,
            labels,
            graph_view,
            touched,
            LabelMutation::Remove,
        ),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "REMOVE keeps clause input, staged state, Graph View, and touched elements explicit"
)]
fn remove_property(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    key: &WritePropertyKey,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let (owner_kind, id) = visible_owner(staged, clause_input, row, variable, graph_view)?;
    let key = resolve_property_key(key, staged, row, context.params)?;
    let Some(key_id) = storage::find_property_key(context.connection, &key)? else {
        return Ok(());
    };
    if staged.property(owner_kind, id, key_id)?.is_some() {
        context.set_property(owner_kind, id, key_id, None)?;
    }
    touched.insert(owner_kind, id);
    Ok(())
}

fn resolve_property_key(
    key: &WritePropertyKey,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
) -> QueryResult<String> {
    match key {
        WritePropertyKey::Static(key) => Ok(key.clone()),
        WritePropertyKey::Dynamic(expression) => {
            match expression::evaluate(expression, snapshot, row, params)? {
                Value::String(key) if !key.is_empty() => Ok(key),
                Value::String(_) => {
                    Err(QueryError::semantic("dynamic property key cannot be empty"))
                }
                _ => Err(QueryError::new(
                    QueryErrorKind::Type,
                    "dynamic property key must evaluate to a non-null String",
                )),
            }
        }
    }
}

fn visible_owner(
    staged: &Snapshot<'_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    graph_view: &ResolvedGraphView,
) -> QueryResult<(OwnerKind, i64)> {
    let owner = binding_owner(row, variable)?;
    require_visible_owner(graph_view, clause_input, staged, row, variable)?;
    Ok(owner)
}

fn merge_one(
    context: &mut MutationContext<'_, '_>,
    row: BindingRow,
    merge: &MergePlan,
    clause_input: &Snapshot<'_>,
    clause_graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let staged = context.staged_snapshot()?;
    validate_merge_pattern_properties(&staged, &row, &merge.pattern, context.params)?;
    let match_graph_view = context.graph_view()?;
    let matches = match_pattern(
        context.connection,
        &staged,
        &match_graph_view,
        &row,
        &merge.pattern,
        context.params,
        is_interrupted,
    )?;
    if !matches.is_empty() {
        return merge_existing(
            context,
            matches,
            merge,
            clause_input,
            clause_graph_view,
            touched,
            is_interrupted,
        );
    }
    merge_create(
        context,
        row,
        merge,
        clause_input,
        clause_graph_view,
        touched,
        is_interrupted,
    )
}

fn validate_merge_pattern_properties(
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    pattern: &WritePatternPart,
    params: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    let expressions = pattern
        .nodes
        .iter()
        .filter_map(|node| node.properties.as_ref())
        .chain(
            pattern
                .relationships
                .iter()
                .filter_map(|relationship| relationship.properties.as_ref()),
        );
    for expression in expressions {
        for (key, value) in evaluate_map(expression, snapshot, row, params)? {
            if matches!(value, Value::Null) {
                return Err(QueryError::semantic(format!(
                    "MERGE property {key:?} cannot be null"
                )));
            }
            property_from_value(value)?;
        }
    }
    Ok(())
}

fn merge_existing(
    context: &mut MutationContext<'_, '_>,
    matches: Vec<BindingRow>,
    merge: &MergePlan,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut output = Vec::with_capacity(matches.len());
    for matched in matches {
        check_interrupted(is_interrupted)?;
        apply_set_items(
            context,
            &matched,
            &merge.on_match,
            clause_input,
            graph_view,
            touched,
        )?;
        output.push(matched);
    }
    Ok(output)
}

fn merge_create(
    context: &mut MutationContext<'_, '_>,
    row: BindingRow,
    merge: &MergePlan,
    clause_input: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut created = row;
    create_pattern_part(
        context,
        &mut created,
        &merge.pattern,
        clause_input,
        graph_view,
        touched,
        is_interrupted,
    )?;
    apply_set_items(
        context,
        &created,
        &merge.on_create,
        clause_input,
        graph_view,
        touched,
    )?;
    Ok(vec![created])
}
