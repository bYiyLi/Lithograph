use std::collections::BTreeMap;

use super::ast::{
    AstKind, AstNode, ClauseKind, ConditionalBranchKind, ExpressionKind, QueryAst, QueryConnector,
    SubqueryKind,
};
use super::error::{FrontendError, FrontendErrorKind, Span};
use super::parser::parse;
use super::semantic_projection::{
    find_descendant, has_star_projection, projection_items, simple_projection_variable,
};
use super::types::infer_expression;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Unknown,
    Value,
    List,
    Map,
    Node,
    Relationship,
    Path,
}

pub(super) type Scope = BTreeMap<String, BindingKind>;

/// Parse and validate a Cypher query without reading graph rows or executing it.
pub fn validate(source: &str) -> Result<QueryAst, FrontendError> {
    let ast = parse(source)?;
    analyze(&ast, source)?;
    Ok(ast)
}

/// Run scope, structural semantic, type, and schema-surface validation.
pub fn analyze(ast: &QueryAst, source: &str) -> Result<(), FrontendError> {
    let mut analyzer = Analyzer {
        source,
        global_scope: Scope::new(),
        standalone_procedure_call: standalone_procedure_call(&ast.root).map(|clause| clause.span),
        standalone_show: standalone_clause(&ast.root, ClauseKind::Show).map(|clause| clause.span),
        nonconcluding_queries: Vec::new(),
    };
    analyzer.validate_schema_surface(&ast.root)?;
    analyzer.analyze_node(&ast.root, &Scope::new())?;
    Ok(())
}

pub(super) struct Analyzer<'a> {
    pub(super) source: &'a str,
    pub(super) global_scope: Scope,
    pub(super) standalone_procedure_call: Option<Span>,
    pub(super) standalone_show: Option<Span>,
    pub(super) nonconcluding_queries: Vec<Span>,
}

fn standalone_clause(node: &AstNode, expected: ClauseKind) -> Option<&AstNode> {
    match node.kind {
        AstKind::QueryBody => node.children.iter().find_map(|child| {
            matches!(
                child.kind,
                AstKind::ComposedQuery | AstKind::ConditionalQuery | AstKind::SingleQuery
            )
            .then(|| standalone_clause(child, expected))
            .flatten()
        }),
        AstKind::ComposedQuery => {
            let mut operands = node
                .children
                .iter()
                .filter(|child| matches!(child.kind, AstKind::SingleQuery | AstKind::Subquery(_)));
            let only = operands.next()?;
            if operands.next().is_some()
                || node
                    .children
                    .iter()
                    .any(|child| matches!(child.kind, AstKind::Connector(_)))
            {
                return None;
            }
            standalone_clause(only, expected)
        }
        AstKind::SingleQuery => {
            let mut clauses = node
                .children
                .iter()
                .filter(|child| matches!(child.kind, AstKind::Clause(_)));
            let clause = clauses.next()?;
            (clauses.next().is_none() && clause.kind == AstKind::Clause(expected)).then_some(clause)
        }
        _ => None,
    }
}

fn standalone_procedure_call(node: &AstNode) -> Option<&AstNode> {
    let clause = standalone_clause(node, ClauseKind::Call)?;
    (!clause
        .children
        .iter()
        .any(|child| matches!(child.kind, AstKind::Subquery(SubqueryKind::Call))))
    .then_some(clause)
}

fn collect_terminal_return_clauses<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    visit_terminal_single_queries(node, true, &mut |single| {
        let Some(clause) = single
            .children
            .iter()
            .rev()
            .find(|child| matches!(child.kind, AstKind::Clause(_)))
        else {
            return;
        };
        if clause.kind == AstKind::Clause(ClauseKind::Return) {
            output.push(clause);
        } else if clause.kind == AstKind::Clause(ClauseKind::Show)
            && let Some(return_clause) = clause
                .children
                .iter()
                .find(|child| child.kind == AstKind::Clause(ClauseKind::Return))
        {
            output.push(return_clause);
        }
    });
}

pub(super) fn collect_terminal_single_query_spans(node: &AstNode, output: &mut Vec<Span>) {
    visit_terminal_single_queries(node, false, &mut |single| output.push(single.span));
}

fn visit_terminal_single_queries<'a>(
    node: &'a AstNode,
    descend_call_subqueries: bool,
    visitor: &mut impl FnMut(&'a AstNode),
) {
    let query_container = matches!(
        node.kind,
        AstKind::QueryBody | AstKind::Subquery(SubqueryKind::Braced)
    ) || (descend_call_subqueries
        && matches!(node.kind, AstKind::Subquery(SubqueryKind::Call)));
    if query_container {
        if let Some(query) = node.children.iter().find(|child| {
            matches!(
                child.kind,
                AstKind::QueryBody
                    | AstKind::ComposedQuery
                    | AstKind::ConditionalQuery
                    | AstKind::SingleQuery
            )
        }) {
            visit_terminal_single_queries(query, descend_call_subqueries, visitor);
        }
        return;
    }
    match node.kind {
        AstKind::ComposedQuery => {
            for child in node.children.iter().rev() {
                match child.kind {
                    AstKind::Connector(QueryConnector::Next) => break,
                    AstKind::SingleQuery | AstKind::Subquery(SubqueryKind::Braced) => {
                        visit_terminal_single_queries(child, descend_call_subqueries, visitor);
                    }
                    _ => {}
                }
            }
        }
        AstKind::ConditionalQuery => {
            for branch in node
                .children
                .iter()
                .filter(|child| matches!(child.kind, AstKind::ConditionalBranch(_)))
            {
                visit_terminal_single_queries(branch, descend_call_subqueries, visitor);
            }
        }
        AstKind::ConditionalBranch(_) => {
            if let Some(query) = node.children.iter().find(|child| {
                matches!(
                    child.kind,
                    AstKind::QueryBody
                        | AstKind::ComposedQuery
                        | AstKind::ConditionalQuery
                        | AstKind::Subquery(SubqueryKind::Braced)
                )
            }) {
                visit_terminal_single_queries(query, descend_call_subqueries, visitor);
            }
        }
        AstKind::SingleQuery => visitor(node),
        _ => {}
    }
}

impl Analyzer<'_> {
    pub(super) fn analyze_node(
        &mut self,
        node: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        match node.kind {
            AstKind::QueryBody => self.analyze_query_body(node, input),
            AstKind::ComposedQuery => self.analyze_composed(node, input),
            AstKind::ConditionalQuery => self.analyze_conditional(node, input),
            AstKind::SingleQuery => self.analyze_single(node, input),
            AstKind::Subquery(SubqueryKind::Braced) => self.analyze_braced_query(node, input),
            AstKind::Subquery(_) => self.analyze_subquery(node, input, true),
            _ => Ok(input.clone()),
        }
    }

    fn analyze_query_body(
        &mut self,
        node: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        let query = node.children.iter().find(|child| {
            matches!(
                child.kind,
                AstKind::ComposedQuery | AstKind::ConditionalQuery | AstKind::SingleQuery
            )
        });
        match query {
            Some(query) => self.analyze_node(query, input),
            None => Ok(input.clone()),
        }
    }

    fn analyze_composed(&mut self, node: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        self.validate_union_connectors(node)?;
        let mut previous = input.clone();
        let mut segment_input = input.clone();
        let mut connector = None;
        let mut union_output: Option<Scope> = None;
        let mut segment_operands = Vec::new();
        for child in &node.children {
            match child.kind {
                AstKind::Connector(value) => {
                    self.advance_composed_connector(
                        value,
                        &mut previous,
                        &mut segment_input,
                        &mut segment_operands,
                        &mut union_output,
                    )?;
                    connector = Some(value);
                }
                AstKind::SingleQuery | AstKind::Subquery(SubqueryKind::Braced) => {
                    let branch_input = match connector {
                        Some(QueryConnector::Next) => &previous,
                        _ => &segment_input,
                    };
                    let output = self.analyze_node(child, branch_input)?;
                    self.validate_composed_union_output(
                        child,
                        connector,
                        &previous,
                        &output,
                        &mut union_output,
                    )?;
                    previous = output;
                    segment_operands.push(child);
                    connector = None;
                }
                _ => {}
            }
        }
        Ok(previous)
    }

    pub(super) fn advance_composed_connector(
        &self,
        connector: QueryConnector,
        previous: &mut Scope,
        segment_input: &mut Scope,
        segment_operands: &mut Vec<&AstNode>,
        union_output: &mut Option<Scope>,
    ) -> Result<bool, FrontendError> {
        if connector != QueryConnector::Next {
            return Ok(false);
        }
        self.validate_next_projections(segment_operands)?;
        if !segment_operands
            .last()
            .is_some_and(|operand| super::query_body_returns_columns(operand))
        {
            previous.clear();
        }
        *segment_input = previous.clone();
        segment_operands.clear();
        *union_output = None;
        Ok(true)
    }

    pub(super) fn validate_composed_union_output(
        &self,
        operand: &AstNode,
        connector: Option<QueryConnector>,
        previous: &Scope,
        output: &Scope,
        union_output: &mut Option<Scope>,
    ) -> Result<(), FrontendError> {
        if matches!(
            connector,
            Some(QueryConnector::Union | QueryConnector::UnionAll | QueryConnector::UnionDistinct)
        ) {
            let expected = union_output.get_or_insert_with(|| previous.clone());
            if expected.keys().ne(output.keys()) {
                return Err(self.semantic_error(
                    operand.span,
                    "UNION branches must expose the same column names",
                ));
            }
        }
        Ok(())
    }

    fn analyze_conditional(
        &mut self,
        node: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        let mut outputs = Vec::new();
        for branch in &node.children {
            if !matches!(branch.kind, AstKind::ConditionalBranch(_)) {
                continue;
            }
            self.validate_branch_condition(branch, input)?;
            if let Some(query) = branch.children.iter().find(|nested| {
                matches!(
                    nested.kind,
                    AstKind::ComposedQuery | AstKind::Subquery(SubqueryKind::Braced)
                )
            }) {
                outputs.push(self.analyze_node(query, input)?);
            }
        }
        let Some(first) = outputs.first() else {
            return Ok(input.clone());
        };
        if outputs
            .iter()
            .skip(1)
            .any(|output| first.keys().ne(output.keys()))
        {
            return Err(
                self.semantic_error(node.span, "WHEN branches must expose the same column names")
            );
        }
        Ok(first.clone())
    }

    fn validate_branch_condition(
        &mut self,
        branch: &AstNode,
        input: &Scope,
    ) -> Result<(), FrontendError> {
        if branch.kind != AstKind::ConditionalBranch(ConditionalBranchKind::When) {
            return Ok(());
        }
        let Some(expression) = branch
            .children
            .iter()
            .find(|child| matches!(child.kind, AstKind::Expression(_)))
        else {
            return Ok(());
        };
        self.validate_expression_references(expression, input)?;
        self.validate_types(expression)
    }

    fn analyze_braced_query(
        &mut self,
        node: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        let Some(body) = node
            .children
            .iter()
            .find(|child| child.kind == AstKind::QueryBody)
        else {
            return Ok(input.clone());
        };
        self.analyze_query_body(body, input)
    }

    fn analyze_single(&mut self, node: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        let clauses = node
            .children
            .iter()
            .filter_map(|clause| match clause.kind {
                AstKind::Clause(kind) => Some((kind, clause)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut scope = input.clone();
        for (index, (kind, clause)) in clauses.iter().copied().enumerate() {
            self.validate_single_clause_position(kind, clause, index, clauses.len(), &scope)?;
            self.validate_global_redeclaration(kind, clause)?;
            scope = self.analyze_clause(kind, clause, &scope)?;
            if kind == ClauseKind::With {
                for (name, binding) in &self.global_scope {
                    scope.insert(name.clone(), *binding);
                }
            }
        }
        if !self.nonconcluding_queries.contains(&node.span) {
            self.validate_query_conclusion(&clauses)?;
        }
        Ok(scope)
    }

    fn validate_single_clause_position(
        &self,
        kind: ClauseKind,
        clause: &AstNode,
        index: usize,
        clause_count: usize,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        let has_following_clause = index + 1 < clause_count;
        if has_following_clause && matches!(kind, ClauseKind::Return | ClauseKind::Finish) {
            return Err(self.semantic_error(
                clause.span,
                "RETURN and FINISH must terminate a single query",
            ));
        }
        if kind != ClauseKind::Show {
            return Ok(());
        }
        if has_following_clause {
            let yielded = super::semantic_clause::show_yield_node(clause).ok_or_else(|| {
                self.semantic_error(
                    clause.span,
                    "composable SHOW requires an explicit YIELD column list",
                )
            })?;
            if yielded
                .descendants()
                .any(|node| node.kind == AstKind::YieldAll)
            {
                return Err(
                    self.semantic_error(yielded.span, "composable SHOW does not allow YIELD *")
                );
            }
        }
        let has_nested_return = clause
            .children
            .iter()
            .any(|node| node.kind == AstKind::Clause(ClauseKind::Return));
        if !has_following_clause && !scope.is_empty() && !has_nested_return {
            return Err(self.semantic_error(
                clause.span,
                "a composable SHOW query must end with a concluding clause",
            ));
        }
        Ok(())
    }

    fn validate_query_conclusion(
        &self,
        clauses: &[(ClauseKind, &AstNode)],
    ) -> Result<(), FrontendError> {
        let Some((kind, clause)) = clauses.last().copied() else {
            return Ok(());
        };
        let valid = match kind {
            ClauseKind::Return
            | ClauseKind::Finish
            | ClauseKind::Create
            | ClauseKind::Insert
            | ClauseKind::Merge
            | ClauseKind::Set
            | ClauseKind::Remove
            | ClauseKind::Delete
            | ClauseKind::DetachDelete
            | ClauseKind::Foreach
            | ClauseKind::CreateIndex
            | ClauseKind::DropIndex
            | ClauseKind::CreateConstraint
            | ClauseKind::DropConstraint
            | ClauseKind::GraphType => true,
            ClauseKind::Show => {
                self.standalone_show == Some(clause.span)
                    || clause
                        .descendants()
                        .any(|node| node.kind == AstKind::Clause(ClauseKind::Return))
            }
            ClauseKind::Call => {
                let call_subquery = clause
                    .descendants()
                    .find(|node| node.kind == AstKind::Subquery(SubqueryKind::Call));
                if let Some(subquery) = call_subquery {
                    !super::query_body_returns_columns(subquery)
                } else {
                    clauses.len() == 1
                        || !clause
                            .descendants()
                            .any(|node| matches!(node.kind, AstKind::YieldAll | AstKind::YieldItem))
                }
            }
            ClauseKind::Match
            | ClauseKind::OptionalMatch
            | ClauseKind::Filter
            | ClauseKind::With
            | ClauseKind::Let
            | ClauseKind::Unwind
            | ClauseKind::For
            | ClauseKind::LoadCsv => false,
        };
        if valid {
            Ok(())
        } else {
            Err(self.semantic_error(
                clause.span,
                "incomplete query: conclude with RETURN, FINISH, an update, a unit CALL subquery, or a standalone procedure call",
            ))
        }
    }

    pub(super) fn validate_next_projections(
        &self,
        operands: &[&AstNode],
    ) -> Result<(), FrontendError> {
        let mut returns = Vec::new();
        for operand in operands {
            collect_terminal_return_clauses(operand, &mut returns);
        }
        for return_clause in returns {
            for item in projection_items(return_clause) {
                if has_star_projection(item)
                    || find_descendant(item, AstKind::ProjectionAlias).is_some()
                    || simple_projection_variable(item).is_some()
                {
                    continue;
                }
                return Err(self.semantic_error(
                    item.span,
                    "RETURN before NEXT requires variables or explicitly aliased expressions",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_types(&self, node: &AstNode) -> Result<(), FrontendError> {
        for expression in node.descendants().filter(|node| {
            matches!(
                node.kind,
                AstKind::Expression(ExpressionKind::Expression)
                    | AstKind::Expression(ExpressionKind::FunctionCall)
            )
        }) {
            let _ = infer_expression(expression, self.source)?;
        }
        Ok(())
    }

    fn validate_schema_surface(&self, root: &AstNode) -> Result<(), FrontendError> {
        super::semantic_schema::validate_schema_surface(root, self.source)
    }

    pub(super) fn bind(
        &self,
        scope: &mut Scope,
        name: &str,
        kind: BindingKind,
        span: Span,
    ) -> Result<(), FrontendError> {
        let name = unescape_identifier(name);
        if let Some(existing) = scope.get(&name).copied() {
            if existing == BindingKind::Unknown {
                scope.insert(name, kind);
                return Ok(());
            }
            if existing != kind {
                return Err(self.semantic_error(
                    span,
                    format!("variable {name:?} is used with incompatible graph element categories"),
                ));
            }
            return Ok(());
        }
        scope.insert(name, kind);
        Ok(())
    }

    pub(super) fn semantic_error(&self, span: Span, message: impl Into<String>) -> FrontendError {
        FrontendError::new(FrontendErrorKind::Semantic, message, span, self.source)
    }

    pub(super) fn slice(&self, span: Span) -> &str {
        self.source.get(span.start..span.end).unwrap_or_default()
    }
}

pub(super) fn direct_local_binding(node: &AstNode) -> Option<&AstNode> {
    if !matches!(
        node.kind,
        AstKind::Expression(ExpressionKind::FunctionCall)
            | AstKind::Expression(ExpressionKind::List)
    ) {
        return None;
    }
    node.children.iter().find(|child| {
        matches!(
            child.kind,
            AstKind::PredicateVariable | AstKind::BindingVariable
        )
    })
}

pub(super) fn is_pattern_comprehension(node: &AstNode) -> bool {
    matches!(node.kind, AstKind::Expression(ExpressionKind::List))
        && node
            .descendants()
            .any(|child| child.kind == AstKind::Pattern)
}

pub(crate) fn unescape_identifier(name: &str) -> String {
    let trimmed = name.trim();
    if let Some(inner) = trimmed
        .strip_prefix('`')
        .and_then(|value| value.strip_suffix('`'))
    {
        inner.replace("``", "`")
    } else {
        trimmed.to_owned()
    }
}
