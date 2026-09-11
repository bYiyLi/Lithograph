use std::collections::BTreeMap;

use super::ast::{
    AstKind, AstNode, ClauseKind, ConditionalBranchKind, ExpressionKind, QueryAst, QueryConnector,
    SubqueryKind,
};
use super::error::{FrontendError, FrontendErrorKind, Span};
use super::parser::parse;
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
    let mut analyzer = Analyzer { source };
    analyzer.validate_schema_surface(&ast.root)?;
    analyzer.analyze_node(&ast.root, &Scope::new())?;
    Ok(())
}

pub(super) struct Analyzer<'a> {
    pub(super) source: &'a str,
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
        let mut connector = None;
        let mut union_output: Option<Scope> = None;
        for child in &node.children {
            match child.kind {
                AstKind::Connector(value) => connector = Some(value),
                AstKind::SingleQuery | AstKind::Subquery(SubqueryKind::Braced) => {
                    let branch_input = match connector {
                        Some(QueryConnector::Next) => &previous,
                        _ => input,
                    };
                    let output = self.analyze_node(child, branch_input)?;
                    if matches!(
                        connector,
                        Some(
                            QueryConnector::Union
                                | QueryConnector::UnionAll
                                | QueryConnector::UnionDistinct
                        )
                    ) {
                        let expected = union_output.get_or_insert_with(|| previous.clone());
                        if expected.keys().ne(output.keys()) {
                            return Err(self.semantic_error(
                                child.span,
                                "UNION branches must expose the same column names",
                            ));
                        }
                    }
                    previous = output;
                    connector = None;
                }
                _ => {}
            }
        }
        Ok(previous)
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
            if index + 1 < clauses.len() && matches!(kind, ClauseKind::Return | ClauseKind::Finish)
            {
                return Err(self.semantic_error(
                    clause.span,
                    "RETURN and FINISH must terminate a single query",
                ));
            }
            scope = self.analyze_clause(kind, clause, &scope)?;
        }
        Ok(scope)
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

pub(super) fn unescape_identifier(name: &str) -> String {
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
