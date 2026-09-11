use std::collections::BTreeSet;

use super::ast::{AstKind, AstNode, ClauseKind, ExpressionKind, QueryConnector, SubqueryKind};
use super::error::FrontendError;
use super::semantic::{Analyzer, BindingKind, Scope, unescape_identifier};
use super::semantic_expression::function_name;
use super::semantic_projection::{find_descendant, projection_items, simple_projection_variable};
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
                QueryConnector::UnionAll => Some(true),
                QueryConnector::Union | QueryConnector::UnionDistinct => Some(false),
                QueryConnector::Next => None,
            };
            let Some(current) = current else {
                continue;
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
        self.validate_pattern_relationship_reuse(clause)?;
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
        self.validate_write_relationships(kind, clause, input)?;
        self.validate_write_nodes(clause, input)?;
        self.validate_write_property_references(clause, input)?;
        self.validate_expression_categories(clause, input)
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
        if body
            .descendants()
            .any(|node| node.kind == AstKind::StarProjection)
            && input.is_empty()
            && !replace_scope
        {
            return Err(self.semantic_error(
                body.span,
                "star projection requires at least one variable in scope",
            ));
        }

        let items = projection_items(body);
        let grouping_keys = items
            .iter()
            .filter_map(|item| self.simple_grouping_key(item))
            .collect::<BTreeSet<_>>();
        for item in items {
            self.validate_projection_item(item, input, replace_scope, &grouping_keys)?;
        }
        self.validate_order_visibility(body, input, output, replace_scope)?;
        self.validate_skip_limit(body)?;

        if replace_scope {
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
    ) -> Result<(), FrontendError> {
        self.validate_expression_references(item, input)?;
        self.reject_pattern_expressions(item)?;
        self.validate_expression_categories(item, input)?;
        self.validate_types(item)?;
        self.validate_aggregation_expression(item, grouping_keys)?;
        if require_alias
            && find_descendant(item, AstKind::ProjectionAlias).is_none()
            && simple_projection_variable(item).is_none()
            && !item
                .descendants()
                .any(|node| node.kind == AstKind::StarProjection)
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
        _replace_scope: bool,
    ) -> Result<(), FrontendError> {
        let mut visible = input.clone();
        visible.extend(output.clone());
        for order in body
            .descendants()
            .filter(|node| node.kind == AstKind::OrderBy)
        {
            self.validate_expression_references(order, &visible)?;
            self.validate_expression_categories(order, &visible)?;
            self.validate_types(order)?;
        }
        Ok(())
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
            self.collect_grouping_references(item, &mut references);
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

    fn collect_grouping_references(&self, node: &AstNode, output: &mut BTreeSet<String>) {
        if matches!(node.kind, AstKind::Subquery(_)) || is_aggregate_call(node) {
            return;
        }
        if matches!(node.kind, AstKind::Expression(ExpressionKind::List))
            && node
                .descendants()
                .any(|child| child.kind == AstKind::BindingVariable)
        {
            return;
        }
        if matches!(node.kind, AstKind::Expression(ExpressionKind::FunctionCall))
            && node
                .children
                .iter()
                .any(|child| child.kind == AstKind::PredicateVariable)
        {
            return;
        }
        if matches!(node.kind, AstKind::Expression(ExpressionKind::Postfix))
            && !node.descendants().any(is_aggregate_call)
            && node
                .descendants()
                .any(|child| child.kind == AstKind::PropertyKey)
            && node
                .descendants()
                .any(|child| child.kind == AstKind::Variable)
            && let Some(reference) = reference_key(node)
        {
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
            self.collect_grouping_references(child, output);
        }
    }

    fn reject_aggregates(&self, node: &AstNode, message: &str) -> Result<(), FrontendError> {
        if contains_aggregate(node) {
            Err(self.semantic_error(node.span, message))
        } else {
            Ok(())
        }
    }

    fn validate_pattern_relationship_reuse(&self, clause: &AstNode) -> Result<(), FrontendError> {
        for part in clause
            .descendants()
            .filter(|node| node.kind == AstKind::PatternPart)
        {
            let mut seen = BTreeSet::new();
            for relationship in part
                .descendants()
                .filter(|node| node.kind == AstKind::RelationshipVariable)
            {
                let Some(name) = relationship.text.as_deref() else {
                    continue;
                };
                let name = unescape_identifier(name);
                if !seen.insert(name.clone()) {
                    return Err(self.semantic_error(
                        relationship.span,
                        format!("relationship variable {name:?} cannot be reused in one pattern"),
                    ));
                }
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
