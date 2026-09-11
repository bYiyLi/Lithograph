use super::*;

mod delete;
mod delta;
mod matcher;
mod value;

use delete::apply_delete;
use delta::{property_states_equal, staged_snapshot};
use matcher::match_pattern;
use value::{
    binding_node, binding_owner, evaluate_map, project_rows, property_from_value,
    require_visible_owner, set_property_map, validate_clause_view,
};

struct MutationContext<'connection, 'query> {
    connection: &'connection Connection,
    base: Snapshot<'connection>,
    base_commit: HashId,
    params: &'query BTreeMap<String, Value>,
    graph_view_selector: &'query GraphViewSelector,
    delta: DeltaBuilder,
}

impl<'connection, 'query> MutationContext<'connection, 'query> {
    fn new(
        connection: &'connection Connection,
        base_commit: HashId,
        params: &'query BTreeMap<String, Value>,
        graph_view_selector: &'query GraphViewSelector,
    ) -> QueryResult<Self> {
        Ok(Self {
            connection,
            base: Snapshot::resolve(connection, base_commit)?,
            base_commit,
            params,
            graph_view_selector,
            delta: DeltaBuilder::default(),
        })
    }

    fn staged_snapshot(&self) -> QueryResult<Snapshot<'connection>> {
        let layer = self.delta.layer()?;
        Ok(Snapshot::resolve_with_layer(
            self.connection,
            self.base_commit,
            &layer,
        )?)
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
        WriteClause::Delete { variables, detach } => {
            execute_delete_clause(context, variables, *detach, rows, is_interrupted)
        }
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
    mut rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let clause_input = context.staged_snapshot()?;
    let graph_view = context.graph_view()?;
    let mut touched = TouchedElements::default();
    for row in &mut rows {
        check_interrupted(is_interrupted)?;
        create_pattern(
            context,
            row,
            pattern,
            &clause_input,
            &graph_view,
            &mut touched,
            is_interrupted,
        )?;
    }
    context.validate_view(&touched)?;
    Ok(rows)
}

fn execute_set_clause(
    context: &mut MutationContext<'_, '_>,
    items: &[SetItem],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    execute_row_mutation_clause(context, RowMutation::Set(items), rows, is_interrupted)
}

fn execute_remove_clause(
    context: &mut MutationContext<'_, '_>,
    items: &[RemoveItem],
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    execute_row_mutation_clause(context, RowMutation::Remove(items), rows, is_interrupted)
}

enum RowMutation<'a> {
    Set(&'a [SetItem]),
    Remove(&'a [RemoveItem]),
}

fn execute_row_mutation_clause(
    context: &mut MutationContext<'_, '_>,
    mutation: RowMutation<'_>,
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let clause_input = context.staged_snapshot()?;
    let graph_view = context.graph_view()?;
    let mut touched = TouchedElements::default();
    for row in &rows {
        check_interrupted(is_interrupted)?;
        match mutation {
            RowMutation::Set(items) => apply_set_items(
                context,
                row,
                items,
                &clause_input,
                &graph_view,
                &mut touched,
            )?,
            RowMutation::Remove(items) => apply_remove_items(
                context,
                row,
                items,
                &clause_input,
                &graph_view,
                &mut touched,
            )?,
        }
    }
    context.validate_view(&touched)?;
    Ok(rows)
}

fn execute_delete_clause(
    context: &mut MutationContext<'_, '_>,
    variables: &[String],
    detach: bool,
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let clause_input = context.staged_snapshot()?;
    let graph_view = context.graph_view()?;
    apply_delete(
        context,
        &rows,
        variables,
        detach,
        &clause_input,
        &graph_view,
        is_interrupted,
    )?;
    Ok(rows)
}

fn execute_merge_clause(
    context: &mut MutationContext<'_, '_>,
    merge: &MergePlan,
    rows: Vec<BindingRow>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let clause_input = context.staged_snapshot()?;
    let clause_graph_view = context.graph_view()?;
    let mut output = Vec::with_capacity(rows.len());
    let mut touched = TouchedElements::default();
    for row in rows {
        check_interrupted(is_interrupted)?;
        output.extend(merge_one(
            context,
            row,
            merge,
            &clause_input,
            &clause_graph_view,
            &mut touched,
            is_interrupted,
        )?);
    }
    context.validate_view(&touched)?;
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
            Direction::Undirected => unreachable!(),
        };
        let record = create_relationship(context, row, relationship, source, target, touched)?;
        relationships.push(record);
        current = next;
    }
    if let Some(variable) = &part.path_variable {
        row.values.insert(
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
    create_node(context, row, spec, touched)
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
    touched: &mut TouchedElements,
) -> QueryResult<i64> {
    let id = storage::allocate_node_id(context.connection)?;
    context.delta.set_node(&context.base, id, true)?;
    touched.nodes.insert(id);
    add_node_labels(context, id, &spec.labels)?;
    apply_create_properties(context, row, OwnerKind::Node, id, spec.properties.as_ref())?;
    if let Some(variable) = &spec.variable {
        row.values.insert(variable.clone(), BindingValue::Node(id));
    }
    Ok(id)
}

fn apply_create_properties(
    context: &mut MutationContext<'_, '_>,
    row: &BindingRow,
    owner_kind: OwnerKind,
    owner_id: i64,
    properties: Option<&Expr>,
) -> QueryResult<()> {
    let Some(properties) = properties else {
        return Ok(());
    };
    let staged = context.staged_snapshot()?;
    let map = evaluate_map(properties, &staged, row, context.params)?;
    set_property_map(
        context.connection,
        &context.base,
        &staged,
        &mut context.delta,
        owner_kind,
        owner_id,
        &map,
        false,
    )
}

fn add_node_labels(
    context: &mut MutationContext<'_, '_>,
    id: i64,
    labels: &[String],
) -> QueryResult<()> {
    for label in labels.iter().collect::<BTreeSet<_>>() {
        let label_id = storage::intern_label(context.connection, label)?;
        context.delta.set_label(&context.base, id, label_id, true)?;
    }
    Ok(())
}

fn create_relationship(
    context: &mut MutationContext<'_, '_>,
    row: &mut BindingRow,
    spec: &RelationshipWriteSpec,
    source: i64,
    target: i64,
    touched: &mut TouchedElements,
) -> QueryResult<RelationshipRecord> {
    if let Some(variable) = &spec.variable
        && row.values.contains_key(variable)
    {
        return Err(QueryError::semantic(format!(
            "CREATE cannot reuse bound Relationship variable {variable}"
        )));
    }
    let id = storage::allocate_relationship_id(context.connection)?;
    let type_id = storage::intern_relationship_type(context.connection, &spec.relationship_type)?;
    let record = RelationshipRecord {
        id,
        source,
        type_id,
        target,
    };
    context
        .delta
        .set_relationship(&context.base, id, Some(record))?;
    touched.relationships.insert(id);
    apply_create_properties(
        context,
        row,
        OwnerKind::Relationship,
        id,
        spec.properties.as_ref(),
    )?;
    if let Some(variable) = &spec.variable {
        row.values
            .insert(variable.clone(), BindingValue::Relationship(record));
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
    let staged = context.staged_snapshot()?;
    match item {
        SetItem::Property {
            variable,
            key,
            value,
        } => set_single_property(
            context,
            &staged,
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
        } => set_property_map_item(
            context,
            &staged,
            clause_input,
            row,
            variable,
            *operator,
            value,
            graph_view,
            touched,
        ),
        SetItem::Labels { variable, labels } => mutate_labels(
            context,
            &staged,
            clause_input,
            row,
            variable,
            labels,
            graph_view,
            touched,
            LabelMutation::Add,
        ),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "SET keeps clause input, staged state, Graph View, and touched elements explicit"
)]
fn set_single_property(
    context: &mut MutationContext<'_, '_>,
    staged: &Snapshot<'_>,
    clause_input: &Snapshot<'_>,
    row: &BindingRow,
    variable: &str,
    key: &str,
    value: &Expr,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let (owner_kind, id) = visible_owner(staged, clause_input, row, variable, graph_view)?;
    let value = expression::evaluate(value, staged, row, context.params)?;
    let next = property_from_value(value)?;
    let key_id = match &next {
        Some(_) => storage::intern_property_key(context.connection, key)?,
        None => match storage::find_property_key(context.connection, key)? {
            Some(key_id) => key_id,
            None => return Ok(()),
        },
    };
    let previous = staged.property(owner_kind, id, key_id)?;
    if !property_states_equal(&previous, &next)? {
        context
            .delta
            .set_property(&context.base, owner_kind, id, key_id, next.clone())?;
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
        context.connection,
        &context.base,
        staged,
        &mut context.delta,
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
    labels: &[String],
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
    mutation: LabelMutation,
) -> QueryResult<()> {
    let id = binding_node(row, variable)?;
    require_visible_owner(graph_view, clause_input, staged, row, variable)?;
    let existing = staged.labels(id)?;
    for label in labels.iter().collect::<BTreeSet<_>>() {
        let label_id = match mutation {
            LabelMutation::Add => storage::intern_label(context.connection, label)?,
            LabelMutation::Remove => {
                let Some(label_id) = storage::find_label(context.connection, label)? else {
                    continue;
                };
                label_id
            }
        };
        let present = existing.binary_search(&label_id).is_ok();
        if matches!(mutation, LabelMutation::Add) && !present {
            context.delta.set_label(&context.base, id, label_id, true)?;
        } else if matches!(mutation, LabelMutation::Remove) && present {
            context
                .delta
                .set_label(&context.base, id, label_id, false)?;
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
    key: &str,
    graph_view: &ResolvedGraphView,
    touched: &mut TouchedElements,
) -> QueryResult<()> {
    let (owner_kind, id) = visible_owner(staged, clause_input, row, variable, graph_view)?;
    let Some(key_id) = storage::find_property_key(context.connection, key)? else {
        return Ok(());
    };
    if staged.property(owner_kind, id, key_id)?.is_some() {
        context
            .delta
            .set_property(&context.base, owner_kind, id, key_id, None)?;
    }
    touched.insert(owner_kind, id);
    Ok(())
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
