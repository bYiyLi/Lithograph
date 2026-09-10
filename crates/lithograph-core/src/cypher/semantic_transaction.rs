use super::ast::{AstKind, AstNode, TransactionDisjointKind};
use super::error::FrontendError;
use super::semantic::{Analyzer, Scope, unescape_identifier};

impl Analyzer<'_> {
    pub(super) fn validate_transaction_subclause(
        &mut self,
        subquery: &AstNode,
        outer: &Scope,
        imported: &Scope,
    ) -> Result<Option<String>, FrontendError> {
        let concurrent =
            descendants_of_kind(subquery, |kind| kind == &AstKind::TransactionConcurrent);
        let batches = descendants_of_kind(subquery, |kind| kind == &AstKind::TransactionBatch);
        let disjoint = descendants_of_kind(subquery, |kind| {
            matches!(kind, AstKind::TransactionDisjoint(_))
        });
        let errors = descendants_of_kind(subquery, |kind| {
            matches!(kind, AstKind::TransactionError(_))
        });
        let statuses = descendants_of_kind(subquery, |kind| kind == &AstKind::TransactionStatus);

        validate_transaction_duplicates(
            self,
            &concurrent,
            &batches,
            &disjoint,
            &errors,
            &statuses,
        )?;
        validate_disjoint_concurrency(self, &concurrent, &disjoint)?;
        self.validate_outer_transaction_expressions(&concurrent, &batches, &errors, outer)?;
        self.validate_disjoint_expression(&disjoint, imported)?;
        Ok(transaction_status_name(&statuses))
    }

    fn validate_outer_transaction_expressions(
        &mut self,
        concurrent: &[&AstNode],
        batches: &[&AstNode],
        errors: &[&AstNode],
        outer: &Scope,
    ) -> Result<(), FrontendError> {
        for node in concurrent.iter().chain(batches).chain(errors) {
            self.validate_expression_references(node, outer)?;
            self.validate_types(node)?;
        }
        Ok(())
    }

    fn validate_disjoint_expression(
        &mut self,
        disjoint: &[&AstNode],
        imported: &Scope,
    ) -> Result<(), FrontendError> {
        let Some(node) = disjoint.first() else {
            return Ok(());
        };
        if !matches!(
            node.kind,
            AstKind::TransactionDisjoint(TransactionDisjointKind::Explicit)
        ) {
            return Ok(());
        }
        self.validate_expression_references(node, imported)?;
        self.validate_types(node)?;
        self.validate_non_aggregate_expression_context(
            node,
            imported,
            "DISJOINT BY expressions cannot contain aggregation",
        )
    }
}

fn validate_transaction_duplicates(
    analyzer: &Analyzer<'_>,
    concurrent: &[&AstNode],
    batches: &[&AstNode],
    disjoint: &[&AstNode],
    errors: &[&AstNode],
    statuses: &[&AstNode],
) -> Result<(), FrontendError> {
    reject_duplicate(concurrent, "CONCURRENT", analyzer)?;
    reject_duplicate(batches, "OF ... ROWS", analyzer)?;
    reject_duplicate(disjoint, "DISJOINT BY", analyzer)?;
    reject_duplicate(errors, "ON ERROR", analyzer)?;
    reject_duplicate(statuses, "REPORT STATUS", analyzer)
}

fn validate_disjoint_concurrency(
    analyzer: &Analyzer<'_>,
    concurrent: &[&AstNode],
    disjoint: &[&AstNode],
) -> Result<(), FrontendError> {
    if disjoint.is_empty() || !concurrent.is_empty() {
        return Ok(());
    }
    Err(analyzer.semantic_error(
        disjoint[0].span,
        "DISJOINT BY is only valid with CONCURRENT TRANSACTIONS",
    ))
}

fn transaction_status_name(statuses: &[&AstNode]) -> Option<String> {
    statuses.first().and_then(|status| {
        status
            .descendants()
            .find(|node| node.kind == AstKind::TransactionStatusBinding)
            .and_then(|node| node.text.as_deref())
            .map(unescape_identifier)
    })
}

pub(super) fn query_body_returns_columns(node: &AstNode) -> bool {
    match node.kind {
        AstKind::QueryBody | AstKind::ComposedQuery | AstKind::ConditionalQuery => node
            .children
            .iter()
            .filter(|child| !matches!(child.kind, AstKind::Subquery(_)))
            .any(query_body_returns_columns),
        AstKind::SingleQuery => {
            node.children
                .iter()
                .rev()
                .find_map(|child| match child.kind {
                    AstKind::Clause(kind) => Some(kind),
                    _ => None,
                })
                == Some(super::ast::ClauseKind::Return)
        }
        _ => false,
    }
}

fn descendants_of_kind(node: &AstNode, predicate: impl Fn(&AstKind) -> bool) -> Vec<&AstNode> {
    node.descendants()
        .filter(|child| predicate(&child.kind))
        .collect()
}

fn reject_duplicate(
    nodes: &[&AstNode],
    name: &str,
    analyzer: &Analyzer<'_>,
) -> Result<(), FrontendError> {
    if nodes.len() <= 1 {
        return Ok(());
    }
    Err(analyzer.semantic_error(
        nodes[1].span,
        format!("transaction subquery cannot repeat {name}"),
    ))
}
