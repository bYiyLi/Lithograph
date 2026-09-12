use std::collections::BTreeSet;

use super::ast::{
    AstKind, AstNode, ClauseKind, ExpressionKind, MatchModeKind, PathModeKind, PathSelectorKind,
    QuantifierKind, QueryConnector, SubqueryKind,
};
use super::error::{FrontendError, Span};
use super::semantic::{Analyzer, BindingKind, Scope, unescape_identifier};
use super::semantic_expression::function_name;
use super::semantic_projection::{
    find_descendant, has_star_projection, projection_items, simple_projection_variable,
};
use super::semantic_rule_helpers::{
    contains_aggregate, is_aggregate_call, reference_key, simple_expression_variable,
    statically_negative_integer, top_level_argument_expressions,
};
use super::types::{CypherType, infer_expression};

impl Analyzer<'_> {
    pub(super) fn validate_union_connectors(&self, node: &AstNode) -> Result<(), FrontendError> {
        let mut flavor = None;
        for connector in node.children.iter().filter_map(|child| match child.kind {
            AstKind::Connector(value) => Some(value),
            _ => None,
        }) {
            let current = match connector {
                QueryConnector::UnionAll => true,
                QueryConnector::Union | QueryConnector::UnionDistinct => false,
                QueryConnector::Next => {
                    flavor = None;
                    continue;
                }
            };
            if let Some(expected) = flavor
                && expected != current
            {
                return Err(self.semantic_error(
                    node.span,
                    "UNION and UNION ALL cannot be mixed in the same composed query",
                ));
            }
            flavor = Some(current);
        }
        Ok(())
    }

    pub(super) fn validate_match_semantics(
        &mut self,
        clause: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        self.reject_aggregates(clause, "MATCH/FILTER predicates cannot contain aggregation")?;
        self.validate_path_modes_and_selectors(clause)?;
        self.validate_variable_length_bounds(clause)?;
        self.validate_pattern_predicates(clause, scope)?;
        self.validate_predicate_types(clause, scope)?;
        self.validate_expression_categories(clause, scope)
    }

    pub(super) fn validate_write_pattern_semantics(
        &mut self,
        kind: ClauseKind,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<(), FrontendError> {
        self.reject_aggregates(clause, "write patterns cannot contain aggregation")?;
        self.validate_create_insert_label_syntax(kind, clause)?;
        self.validate_write_relationships(kind, clause, input)?;
        self.validate_write_nodes(clause, input)?;
        self.validate_write_property_references(clause, input)?;
        self.validate_expression_categories(clause, input)
    }

    fn validate_create_insert_label_syntax(
        &self,
        kind: ClauseKind,
        clause: &AstNode,
    ) -> Result<(), FrontendError> {
        let mut colon_separated = false;
        let mut ampersand_separated = false;
        for labels in clause
            .descendants()
            .filter(|node| node.kind == AstKind::LabelExpression)
        {
            let direct_groups = labels
                .children
                .iter()
                .filter(|node| {
                    node.kind
                        == AstKind::NameExpression(super::ast::NameExpressionKind::Disjunction)
                })
                .count();
            colon_separated |= direct_groups > 1;
            ampersand_separated |= labels.descendants().any(|node| {
                node.kind == AstKind::NameExpression(super::ast::NameExpressionKind::Conjunction)
                    && node
                        .children
                        .iter()
                        .filter(|child| {
                            matches!(
                                child.kind,
                                AstKind::NameExpression(super::ast::NameExpressionKind::Negation(
                                    _
                                ))
                            )
                        })
                        .count()
                        > 1
            });
            if kind == ClauseKind::Insert
                && labels.descendants().any(|node| {
                    node.kind == AstKind::NameExpression(super::ast::NameExpressionKind::Dynamic)
                })
            {
                return Err(
                    self.semantic_error(labels.span, "INSERT does not support dynamic node labels")
                );
            }
        }
        if kind == ClauseKind::Insert
            && clause.descendants().any(|node| {
                node.kind == AstKind::RelationshipPattern
                    && node.descendants().any(|child| {
                        child.kind
                            == AstKind::NameExpression(super::ast::NameExpressionKind::Dynamic)
                    })
            })
        {
            return Err(self.semantic_error(
                clause.span,
                "INSERT does not support dynamic relationship types",
            ));
        }
        if kind == ClauseKind::Insert && colon_separated {
            return Err(self.semantic_error(
                clause.span,
                "INSERT requires ampersands between multiple node labels",
            ));
        }
        if kind == ClauseKind::Create && colon_separated && ampersand_separated {
            return Err(self.semantic_error(
                clause.span,
                "CREATE cannot mix colon and ampersand label separators in one clause",
            ));
        }
        Ok(())
    }

    fn validate_write_property_references(
        &mut self,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<(), FrontendError> {
        for element in clause.descendants().filter(|node| {
            matches!(
                node.kind,
                AstKind::NodePattern | AstKind::RelationshipPattern
            )
        }) {
            let mut properties = Vec::new();
            collect_write_property_maps(element, &mut properties);
            for property_map in properties {
                self.validate_expression_references(property_map, input)?;
            }
        }
        Ok(())
    }

    pub(super) fn validate_delete_semantics(
        &mut self,
        clause: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        self.reject_aggregates(clause, "DELETE cannot contain aggregation")?;
        let expressions = top_level_argument_expressions(clause);
        if expressions.is_empty() {
            return Err(self.semantic_error(clause.span, "DELETE requires an expression"));
        }
        for expression in expressions {
            if expression.descendants().any(|child| {
                matches!(
                    child.kind,
                    AstKind::LabelName | AstKind::RelationshipTypeName
                )
            }) {
                return Err(self.semantic_error(
                    expression.span,
                    "DELETE cannot target a label or relationship-type predicate",
                ));
            }
            if let Some(variable) = simple_expression_variable(expression) {
                let variable = unescape_identifier(variable);
                if matches!(scope.get(&variable), Some(BindingKind::Value)) {
                    return Err(self.semantic_error(
                        expression.span,
                        "DELETE expression is statically non-graph",
                    ));
                }
                continue;
            }
            let value_type = infer_expression(expression, self.source)?;
            if !matches!(
                value_type,
                CypherType::Any
                    | CypherType::Null
                    | CypherType::Node
                    | CypherType::Relationship
                    | CypherType::Path
            ) {
                return Err(self.semantic_error(
                    expression.span,
                    "DELETE expression must resolve to a graph element or path",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_non_aggregate_expression_context(
        &mut self,
        node: &AstNode,
        scope: &Scope,
        message: &str,
    ) -> Result<(), FrontendError> {
        self.reject_aggregates(node, message)?;
        self.reject_pattern_expressions(node)?;
        self.validate_expression_categories(node, scope)
    }

    pub(super) fn validate_projection_semantics(
        &mut self,
        clause: &AstNode,
        input: &Scope,
        output: &Scope,
        replace_scope: bool,
    ) -> Result<(), FrontendError> {
        let body = find_descendant(clause, AstKind::ProjectionBody).unwrap_or(clause);
        self.validate_projection_star(body, input, replace_scope)?;

        let items = projection_items(body);
        let group_by = body
            .children
            .iter()
            .find(|node| node.kind == AstKind::GroupBy);
        let grouping_expressions = self.grouping_expressions(group_by, &items);
        let grouping_keys = self.grouping_reference_keys(group_by, &items, &grouping_expressions);
        if let Some(group_by) = group_by {
            self.validate_group_by(group_by, input, output, &grouping_expressions)?;
        }
        let aggregating = group_by.is_some() || items.iter().any(|item| contains_aggregate(item));
        self.validate_projection_items(
            &items,
            input,
            replace_scope,
            &grouping_keys,
            &grouping_expressions,
            group_by,
        )?;
        self.validate_order_visibility(body, input, output)?;
        if aggregating {
            self.validate_aggregate_order(body, output, &grouping_keys, &grouping_expressions)?;
        } else {
            self.reject_order_aggregation(body)?;
        }
        self.validate_skip_limit(body)?;
        self.validate_projection_suffix(clause, input, output, replace_scope)?;
        Ok(())
    }

    fn validate_projection_star(
        &self,
        body: &AstNode,
        input: &Scope,
        replace_scope: bool,
    ) -> Result<(), FrontendError> {
        if has_star_projection(body) && input.is_empty() && !replace_scope {
            return Err(self.semantic_error(
                body.span,
                "star projection requires at least one variable in scope",
            ));
        }
        Ok(())
    }

    fn validate_projection_items(
        &mut self,
        items: &[&AstNode],
        input: &Scope,
        replace_scope: bool,
        grouping_keys: &BTreeSet<String>,
        grouping_expressions: &[&AstNode],
        group_by: Option<&AstNode>,
    ) -> Result<(), FrontendError> {
        for item in items {
            self.validate_projection_item(
                item,
                input,
                replace_scope,
                grouping_keys,
                grouping_expressions,
            )?;
            if let Some(group_by) = group_by {
                self.validate_explicit_group_projection(item, group_by, grouping_expressions)?;
            }
        }
        Ok(())
    }

    fn validate_projection_suffix(
        &mut self,
        clause: &AstNode,
        input: &Scope,
        output: &Scope,
        replace_scope: bool,
    ) -> Result<(), FrontendError> {
        if !replace_scope {
            return Ok(());
        }
        let mut evaluation_scope = input.clone();
        evaluation_scope.extend(output.clone());
        for child in &clause.children {
            if child.kind == AstKind::ProjectionBody {
                continue;
            }
            self.validate_expression_references(child, &evaluation_scope)?;
            self.validate_expression_categories(child, &evaluation_scope)?;
            self.validate_predicate_types(child, &evaluation_scope)?;
        }
        Ok(())
    }

    pub(super) fn validate_procedure_argument_semantics(
        &mut self,
        arguments: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        self.reject_aggregates(
            arguments,
            "procedure arguments cannot contain aggregating expressions",
        )?;
        self.validate_expression_categories(arguments, scope)
    }

    pub(super) fn validate_expression_subquery_is_read_only(
        &self,
        node: &AstNode,
        correlated: bool,
    ) -> Result<(), FrontendError> {
        if !correlated {
            return Ok(());
        }
        if !matches!(
            node.kind,
            AstKind::Subquery(SubqueryKind::Exists | SubqueryKind::Count | SubqueryKind::Collect)
        ) {
            return Ok(());
        }
        if node.descendants().any(|child| {
            matches!(
                child.kind,
                AstKind::Clause(
                    ClauseKind::Create
                        | ClauseKind::Insert
                        | ClauseKind::Merge
                        | ClauseKind::Set
                        | ClauseKind::Remove
                        | ClauseKind::Delete
                        | ClauseKind::DetachDelete
                        | ClauseKind::Foreach
                        | ClauseKind::GraphType
                        | ClauseKind::CreateIndex
                        | ClauseKind::DropIndex
                        | ClauseKind::CreateConstraint
                        | ClauseKind::DropConstraint
                )
            )
        }) {
            return Err(self.semantic_error(node.span, "subquery expressions must be read-only"));
        }
        Ok(())
    }

    fn validate_projection_item(
        &mut self,
        item: &AstNode,
        input: &Scope,
        require_alias: bool,
        grouping_keys: &BTreeSet<String>,
        grouping_expressions: &[&AstNode],
    ) -> Result<(), FrontendError> {
        self.validate_expression_references(item, input)?;
        self.reject_pattern_expressions(item)?;
        self.validate_expression_categories(item, input)?;
        self.validate_types(item)?;
        self.validate_aggregation_expression(item, grouping_keys, grouping_expressions)?;
        if require_alias
            && find_descendant(item, AstKind::ProjectionAlias).is_none()
            && simple_projection_variable(item).is_none()
            && !has_star_projection(item)
        {
            return Err(self.semantic_error(
                item.span,
                "WITH expressions that are not simple variables must use AS",
            ));
        }
        Ok(())
    }

    fn validate_order_visibility(
        &mut self,
        body: &AstNode,
        input: &Scope,
        output: &Scope,
    ) -> Result<(), FrontendError> {
        let distinct = body.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::SetQuantifier(super::ast::SetQuantifierKind::Distinct)
            )
        });
        let mut visible = input.clone();
        visible.extend(output.clone());
        let projected = projection_items(body)
            .into_iter()
            .filter_map(projection_expression)
            .collect::<Vec<_>>();
        for order in body
            .descendants()
            .filter(|node| node.kind == AstKind::OrderBy)
        {
            if distinct {
                let mut expressions = Vec::new();
                collect_surface_expressions(order, &mut expressions);
                for expression in expressions {
                    let expression_visible = if projected
                        .iter()
                        .any(|projection| same_syntax(projection, expression))
                    {
                        &visible
                    } else {
                        output
                    };
                    self.validate_expression_references(expression, expression_visible)?;
                    self.validate_expression_categories(expression, expression_visible)?;
                }
            } else {
                self.validate_expression_references(order, &visible)?;
                self.validate_expression_categories(order, &visible)?;
            }
            self.validate_types(order)?;
        }
        Ok(())
    }

    fn validate_aggregate_order(
        &self,
        body: &AstNode,
        output: &Scope,
        grouping_keys: &BTreeSet<String>,
        grouping_expressions: &[&AstNode],
    ) -> Result<(), FrontendError> {
        let projected_aggregates = projection_items(body)
            .into_iter()
            .flat_map(AstNode::descendants)
            .filter(|node| is_aggregate_call(node))
            .collect::<Vec<_>>();
        let output_references = output
            .keys()
            .map(|name| format!("{}:{name};", name.len()))
            .collect::<BTreeSet<_>>();
        for order in body
            .descendants()
            .filter(|node| node.kind == AstKind::OrderBy)
        {
            for aggregate in order.descendants().filter(|node| is_aggregate_call(node)) {
                if aggregate.children.iter().any(contains_aggregate) {
                    return Err(self
                        .semantic_error(aggregate.span, "aggregating functions cannot be nested"));
                }
                if !projected_aggregates
                    .iter()
                    .any(|projected| same_syntax(projected, aggregate))
                {
                    return Err(self.semantic_error(
                        aggregate.span,
                        "ORDER BY aggregation must also be projected",
                    ));
                }
            }
            let mut references = BTreeSet::new();
            self.collect_grouping_references(order, &mut references, grouping_expressions);
            if let Some(reference) = references.iter().find(|reference| {
                !grouping_keys.contains(*reference) && !output_references.contains(*reference)
            }) {
                return Err(self.semantic_error(
                    order.span,
                    format!("aggregating ORDER BY uses ungrouped reference {reference:?}"),
                ));
            }
        }
        Ok(())
    }

    fn reject_order_aggregation(&self, body: &AstNode) -> Result<(), FrontendError> {
        if let Some(aggregate) = body
            .descendants()
            .find(|node| node.kind == AstKind::OrderBy)
            .and_then(|order| order.descendants().find(|node| is_aggregate_call(node)))
        {
            Err(self.semantic_error(
                aggregate.span,
                "ORDER BY cannot introduce aggregation after a non-aggregating projection",
            ))
        } else {
            Ok(())
        }
    }

    fn grouping_expressions<'a>(
        &self,
        group_by: Option<&'a AstNode>,
        items: &[&'a AstNode],
    ) -> Vec<&'a AstNode> {
        let Some(group_by) = group_by else {
            return Vec::new();
        };
        if group_by
            .descendants()
            .any(|node| node.kind == AstKind::GroupByAll)
        {
            return items
                .iter()
                .filter(|item| !contains_aggregate(item))
                .filter_map(|item| projection_expression(item))
                .collect();
        }
        if group_by
            .descendants()
            .any(|node| node.kind == AstKind::GroupByEmpty)
        {
            return Vec::new();
        }
        let mut expressions = Vec::new();
        collect_surface_expressions(group_by, &mut expressions);
        expressions
    }

    fn grouping_reference_keys(
        &self,
        group_by: Option<&AstNode>,
        items: &[&AstNode],
        grouping_expressions: &[&AstNode],
    ) -> BTreeSet<String> {
        if group_by.is_none() {
            return items
                .iter()
                .filter_map(|item| self.simple_grouping_key(item))
                .collect();
        }
        let mut keys = grouping_expressions
            .iter()
            .filter_map(|expression| self.simple_expression_grouping_key(expression))
            .collect::<BTreeSet<_>>();
        for item in items.iter().filter(|item| !contains_aggregate(item)) {
            let alias = find_descendant(item, AstKind::ProjectionAlias)
                .and_then(|node| node.text.as_deref())
                .map(unescape_identifier);
            if alias.is_some_and(|alias| {
                grouping_expressions.iter().any(|expression| {
                    simple_expression_variable(expression)
                        .map(unescape_identifier)
                        .as_deref()
                        == Some(alias.as_str())
                })
            }) && let Some(key) = self.simple_grouping_key(item)
            {
                keys.insert(key);
            }
        }
        keys
    }

    fn validate_group_by(
        &mut self,
        group_by: &AstNode,
        input: &Scope,
        output: &Scope,
        grouping_expressions: &[&AstNode],
    ) -> Result<(), FrontendError> {
        let mut visible = input.clone();
        visible.extend(output.clone());
        for expression in grouping_expressions {
            self.reject_aggregates(expression, "GROUP BY cannot contain aggregation")?;
            self.reject_pattern_expressions(expression)?;
            self.validate_expression_references(expression, &visible)?;
            self.validate_expression_categories(expression, &visible)?;
            self.validate_types(expression)?;
        }
        if group_by
            .descendants()
            .any(|node| node.kind == AstKind::GroupByEmpty)
            && !grouping_expressions.is_empty()
        {
            return Err(self.semantic_error(group_by.span, "GROUP BY () cannot contain keys"));
        }
        Ok(())
    }

    fn validate_explicit_group_projection(
        &self,
        item: &AstNode,
        group_by: &AstNode,
        grouping_expressions: &[&AstNode],
    ) -> Result<(), FrontendError> {
        if contains_aggregate(item)
            || group_by
                .descendants()
                .any(|node| node.kind == AstKind::GroupByAll)
        {
            return Ok(());
        }
        let Some(expression) = projection_expression(item) else {
            return Ok(());
        };
        let exact_key = grouping_expressions
            .iter()
            .any(|key| same_expression_syntax(key, expression));
        let alias_key = find_descendant(item, AstKind::ProjectionAlias)
            .and_then(|node| node.text.as_deref())
            .map(unescape_identifier)
            .is_some_and(|alias| {
                grouping_expressions.iter().any(|key| {
                    simple_expression_variable(key)
                        .map(unescape_identifier)
                        .as_deref()
                        == Some(alias.as_str())
                })
            });
        if exact_key || alias_key {
            return Ok(());
        }
        let mut references = BTreeSet::new();
        self.collect_grouping_references(expression, &mut references, &[]);
        let volatile = expression
            .descendants()
            .any(|node| function_name(node).as_deref() == Some("rand"));
        if references.is_empty() && !volatile {
            return Ok(());
        }
        Err(self.semantic_error(
            item.span,
            "non-aggregating projection expression must be a GROUP BY key",
        ))
    }

    fn validate_skip_limit(&self, body: &AstNode) -> Result<(), FrontendError> {
        for node in body
            .descendants()
            .filter(|node| matches!(node.kind, AstKind::Skip | AstKind::Limit))
        {
            let expression = node
                .descendants()
                .find(|child| matches!(child.kind, AstKind::Expression(ExpressionKind::Expression)))
                .ok_or_else(|| {
                    self.semantic_error(node.span, "SKIP/LIMIT requires an expression")
                })?;
            if expression
                .descendants()
                .any(|child| child.kind == AstKind::Variable)
            {
                return Err(self
                    .semantic_error(expression.span, "SKIP/LIMIT cannot depend on row variables"));
            }
            if contains_aggregate(expression) {
                return Err(
                    self.semantic_error(expression.span, "SKIP/LIMIT cannot contain aggregation")
                );
            }
            let value_type = infer_expression(expression, self.source)?;
            if !matches!(value_type, CypherType::Integer | CypherType::Any) {
                return Err(self.semantic_error(
                    expression.span,
                    "SKIP/LIMIT requires a non-negative Integer expression",
                ));
            }
            if statically_negative_integer(expression) {
                return Err(self.semantic_error(
                    expression.span,
                    "SKIP/LIMIT requires a non-negative Integer expression",
                ));
            }
        }
        Ok(())
    }

    fn validate_aggregation_expression(
        &self,
        item: &AstNode,
        grouping_keys: &BTreeSet<String>,
        grouping_expressions: &[&AstNode],
    ) -> Result<(), FrontendError> {
        for aggregate in item.descendants().filter(|node| is_aggregate_call(node)) {
            if aggregate.children.iter().any(contains_aggregate) {
                return Err(
                    self.semantic_error(aggregate.span, "aggregating functions cannot be nested")
                );
            }
            if aggregate
                .descendants()
                .any(|child| function_name(child).as_deref() == Some("rand"))
            {
                return Err(self.semantic_error(
                    aggregate.span,
                    "non-deterministic rand() cannot be nested inside aggregation",
                ));
            }
        }
        if contains_aggregate(item) {
            let mut references = BTreeSet::new();
            self.collect_grouping_references(item, &mut references, grouping_expressions);
            if let Some(reference) = references
                .iter()
                .find(|reference| !grouping_keys.contains(*reference))
            {
                return Err(self.semantic_error(
                    item.span,
                    format!("aggregating expression uses ungrouped reference {reference:?}"),
                ));
            }
        }
        Ok(())
    }

    fn simple_grouping_key(&self, item: &AstNode) -> Option<String> {
        if contains_aggregate(item) {
            return None;
        }
        let expression = item
            .children
            .iter()
            .find(|child| matches!(child.kind, AstKind::Expression(_)))?;
        self.simple_expression_grouping_key(expression)
    }

    fn simple_expression_grouping_key(&self, expression: &AstNode) -> Option<String> {
        if expression.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::FunctionName
                    | AstKind::Operator
                    | AstKind::Literal(_)
                    | AstKind::Pattern
                    | AstKind::Subquery(_)
                    | AstKind::Expression(ExpressionKind::List)
                    | AstKind::Expression(ExpressionKind::Map)
                    | AstKind::Expression(ExpressionKind::Case)
                    | AstKind::Expression(ExpressionKind::InterpolatedString)
            )
        }) {
            return None;
        }
        reference_key(expression)
    }

    fn collect_grouping_references(
        &self,
        node: &AstNode,
        output: &mut BTreeSet<String>,
        grouping_expressions: &[&AstNode],
    ) {
        if grouping_reference_barrier(node, grouping_expressions) {
            return;
        }
        if let Some(reference) = postfix_grouping_reference(node) {
            output.insert(reference);
            return;
        }
        if node.kind == AstKind::Variable {
            if let Some(reference) = reference_key(node) {
                output.insert(reference);
            }
            return;
        }
        for child in &node.children {
            self.collect_grouping_references(child, output, grouping_expressions);
        }
    }

    fn reject_aggregates(&self, node: &AstNode, message: &str) -> Result<(), FrontendError> {
        if contains_aggregate(node) {
            Err(self.semantic_error(node.span, message))
        } else {
            Ok(())
        }
    }

    fn validate_path_modes_and_selectors(&self, clause: &AstNode) -> Result<(), FrontendError> {
        let match_mode = clause
            .children
            .iter()
            .find_map(|node| match node.kind {
                AstKind::MatchMode(mode) => Some(mode),
                _ => None,
            })
            .unwrap_or(MatchModeKind::DifferentRelationships);
        let Some(pattern) = clause
            .children
            .iter()
            .find(|node| node.kind == AstKind::Pattern)
        else {
            return Ok(());
        };
        let parts = pattern
            .children
            .iter()
            .filter(|node| node.kind == AstKind::PatternPart)
            .collect::<Vec<_>>();
        let mut effective_mode = None;
        let mut selective_span = None;
        for part in &parts {
            let (mode, part_selective_span) =
                self.validate_path_part(part, match_mode, effective_mode)?;
            effective_mode = Some(mode);
            selective_span = selective_span.or(part_selective_span);
        }
        if match_mode == MatchModeKind::DifferentRelationships
            && parts.len() > 1
            && let Some(span) = selective_span
        {
            return Err(self.semantic_error(
                span,
                "DIFFERENT RELATIONSHIPS allows a selective path selector only with one path pattern",
            ));
        }
        Ok(())
    }

    fn validate_path_part(
        &self,
        part: &AstNode,
        match_mode: MatchModeKind,
        expected_mode: Option<PathModeKind>,
    ) -> Result<(PathModeKind, Option<Span>), FrontendError> {
        let explicit_mode = part.children.iter().find_map(|node| match node.kind {
            AstKind::PathMode(mode) => Some((node, mode)),
            _ => None,
        });
        let mode = explicit_mode.map_or(PathModeKind::Walk, |(_, mode)| mode);
        if expected_mode.is_some_and(|expected| expected != mode) {
            return Err(self.semantic_error(
                part.span,
                "all path patterns in one MATCH must use the same path mode",
            ));
        }
        if match_mode == MatchModeKind::RepeatableElements && mode != PathModeKind::Walk {
            return Err(self.semantic_error(
                part.span,
                "REPEATABLE ELEMENTS can only be combined with the WALK path mode",
            ));
        }
        if let Some((path_mode, _)) = explicit_mode
            && part
                .descendants()
                .any(|node| node.kind == AstKind::VariableLength)
        {
            return Err(self.semantic_error(
                path_mode.span,
                "explicit path modes cannot be combined with legacy variable-length relationships",
            ));
        }
        if match_mode == MatchModeKind::RepeatableElements {
            self.validate_repeatable_bounds(part)?;
        }
        let selective_span = part
            .children
            .iter()
            .find(|node| {
                matches!(
                    node.kind,
                    AstKind::PathSelector(kind) if kind != PathSelectorKind::All
                )
            })
            .map(|node| node.span);
        Ok((mode, selective_span))
    }

    fn validate_repeatable_bounds(&self, part: &AstNode) -> Result<(), FrontendError> {
        for quantifier in part
            .descendants()
            .filter(|node| matches!(node.kind, AstKind::Quantifier(_)))
        {
            let AstKind::Quantifier(kind) = quantifier.kind else {
                unreachable!();
            };
            let bounded = kind == QuantifierKind::Fixed
                || (kind == QuantifierKind::Range
                    && quantifier
                        .descendants()
                        .any(|node| node.kind == AstKind::QuantifierUpperBound));
            if !bounded {
                return Err(self.semantic_error(
                    quantifier.span,
                    "REPEATABLE ELEMENTS requires an upper bound on every quantified path",
                ));
            }
        }
        for length in part
            .descendants()
            .filter(|node| node.kind == AstKind::VariableLength)
        {
            let text = length.text.as_deref().unwrap_or_default();
            let bounded = text
                .split_once("..")
                .is_none_or(|(_, upper)| !upper.trim().is_empty())
                && text.trim() != "*";
            if !bounded {
                return Err(self.semantic_error(
                    length.span,
                    "REPEATABLE ELEMENTS requires an upper bound on every variable-length relationship",
                ));
            }
        }
        Ok(())
    }

    fn validate_variable_length_bounds(&self, clause: &AstNode) -> Result<(), FrontendError> {
        for length in clause
            .descendants()
            .filter(|node| node.kind == AstKind::VariableLength)
        {
            if length.descendants().any(|child| {
                matches!(
                    child.kind,
                    AstKind::Literal(super::ast::LiteralKind::Integer)
                ) && child
                    .text
                    .as_deref()
                    .is_some_and(|text| text.starts_with('-'))
            }) {
                return Err(self.semantic_error(
                    length.span,
                    "variable-length relationship bounds cannot be negative",
                ));
            }
        }
        Ok(())
    }

    fn validate_write_relationships(
        &self,
        kind: ClauseKind,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<(), FrontendError> {
        for relationship in clause
            .descendants()
            .filter(|node| node.kind == AstKind::RelationshipPattern)
        {
            let arrow_heads = relationship
                .descendants()
                .filter(|node| {
                    matches!(
                        node.kind,
                        AstKind::RelationshipLeftArrow | AstKind::RelationshipRightArrow
                    )
                })
                .count();
            let direction_valid = match kind {
                ClauseKind::Merge => arrow_heads <= 1,
                ClauseKind::Create | ClauseKind::Insert => arrow_heads == 1,
                _ => true,
            };
            if !direction_valid {
                return Err(self.semantic_error(
                    relationship.span,
                    format!("{kind:?} relationship direction is not legal"),
                ));
            }
            let types = relationship
                .descendants()
                .filter(|node| node.kind == AstKind::RelationshipTypeName)
                .count();
            if types != 1 {
                return Err(self.semantic_error(
                    relationship.span,
                    format!("{kind:?} relationships must have exactly one relationship type"),
                ));
            }
            if relationship
                .descendants()
                .any(|node| node.kind == AstKind::VariableLength)
            {
                return Err(self.semantic_error(
                    relationship.span,
                    format!("{kind:?} does not allow variable-length relationships"),
                ));
            }
            if let Some(variable) = relationship
                .descendants()
                .find(|node| node.kind == AstKind::RelationshipVariable)
                .and_then(|node| node.text.as_deref())
                && input.contains_key(&unescape_identifier(variable))
            {
                return Err(self.semantic_error(
                    relationship.span,
                    format!("{kind:?} cannot create an already-bound relationship"),
                ));
            }
        }
        Ok(())
    }

    fn validate_write_nodes(&self, clause: &AstNode, input: &Scope) -> Result<(), FrontendError> {
        let mut introduced = BTreeSet::new();
        for part in clause
            .descendants()
            .filter(|node| node.kind == AstKind::PatternPart)
        {
            let has_relationship = part
                .descendants()
                .any(|node| node.kind == AstKind::RelationshipPattern);
            for node in part
                .descendants()
                .filter(|node| node.kind == AstKind::NodePattern)
            {
                let Some(variable) = node
                    .descendants()
                    .find(|child| child.kind == AstKind::PatternVariable)
                    .and_then(|child| child.text.as_deref())
                else {
                    continue;
                };
                let variable = unescape_identifier(variable);
                let decorated = node
                    .children
                    .iter()
                    .any(|child| !matches!(child.kind, AstKind::PatternVariable));
                if input.contains_key(&variable) {
                    if !has_relationship || decorated {
                        return Err(self.semantic_error(
                            node.span,
                            format!(
                                "write pattern cannot recreate or decorate bound node {variable:?}"
                            ),
                        ));
                    }
                    continue;
                }
                if introduced.contains(&variable) {
                    if !has_relationship || decorated {
                        return Err(self.semantic_error(
                            node.span,
                            format!(
                                "write pattern cannot recreate or decorate bound node {variable:?}"
                            ),
                        ));
                    }
                    continue;
                }
                introduced.insert(variable);
            }
        }
        Ok(())
    }
}

fn same_syntax(left: &AstNode, right: &AstNode) -> bool {
    left.kind == right.kind
        && left.text == right.text
        && left.children.len() == right.children.len()
        && left
            .children
            .iter()
            .zip(&right.children)
            .all(|(left, right)| same_syntax(left, right))
}

fn same_expression_syntax<'a>(left: &'a AstNode, right: &'a AstNode) -> bool {
    fn core(mut node: &AstNode) -> &AstNode {
        while matches!(node.kind, AstKind::Expression(_))
            && node.text.is_none()
            && node.children.len() == 1
        {
            node = &node.children[0];
        }
        node
    }

    same_syntax(core(left), core(right))
}

fn grouping_reference_barrier(node: &AstNode, grouping_expressions: &[&AstNode]) -> bool {
    if grouping_expressions
        .iter()
        .any(|grouping| same_expression_syntax(grouping, node))
        || matches!(node.kind, AstKind::Subquery(_))
        || is_aggregate_call(node)
    {
        return true;
    }
    if matches!(node.kind, AstKind::Expression(ExpressionKind::List))
        && node
            .descendants()
            .any(|child| child.kind == AstKind::BindingVariable)
    {
        return true;
    }
    matches!(node.kind, AstKind::Expression(ExpressionKind::FunctionCall))
        && node.children.iter().any(|child| {
            matches!(
                child.kind,
                AstKind::PredicateVariable | AstKind::ReductionAccumulator
            )
        })
}

fn postfix_grouping_reference(node: &AstNode) -> Option<String> {
    if !matches!(node.kind, AstKind::Expression(ExpressionKind::Postfix))
        || node.descendants().any(is_aggregate_call)
        || !node
            .descendants()
            .any(|child| child.kind == AstKind::PropertyKey)
        || !node
            .descendants()
            .any(|child| child.kind == AstKind::Variable)
    {
        return None;
    }
    reference_key(node)
}

fn projection_expression(item: &AstNode) -> Option<&AstNode> {
    item.children
        .iter()
        .find(|node| matches!(node.kind, AstKind::Expression(_)))
}

fn collect_surface_expressions<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    if matches!(node.kind, AstKind::Expression(ExpressionKind::Expression)) {
        output.push(node);
        return;
    }
    for child in &node.children {
        collect_surface_expressions(child, output);
    }
}

fn collect_write_property_maps<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    for child in &node.children {
        if child.kind == AstKind::Where {
            continue;
        }
        if matches!(child.kind, AstKind::Expression(ExpressionKind::Map)) {
            output.push(child);
            continue;
        }
        collect_write_property_maps(child, output);
    }
}
