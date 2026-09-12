use super::ast::{
    AstKind, AstNode, ClauseKind, ExpressionKind, QueryConnector, ShowTargetKind, SubqueryKind,
};
use super::error::{FrontendError, FrontendErrorKind};
use super::parser::parse_expression_fragment_at;
use super::semantic::{
    Analyzer, BindingKind, Scope, direct_local_binding, is_pattern_comprehension,
    unescape_identifier,
};
use super::semantic_expression::validate_local_binding_type_use;
use super::semantic_interpolation::interpolation_fragments;
use super::semantic_projection::{
    expression_binding_kind, find_descendant, has_star_projection, projection_items,
    simple_projection_variable, yield_output_name,
};
use super::semantic_rule_helpers::contains_aggregate;
use super::types::{CypherType, infer_expression};

impl Analyzer<'_> {
    pub(super) fn analyze_clause(
        &mut self,
        kind: ClauseKind,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        match kind {
            ClauseKind::Match | ClauseKind::OptionalMatch => self.analyze_match(clause, input),
            ClauseKind::Create | ClauseKind::Insert | ClauseKind::Merge => {
                self.analyze_pattern_write(kind, clause, input)
            }
            ClauseKind::Filter => self.analyze_filter(clause, input),
            ClauseKind::Return => self.analyze_projection(clause, input, false),
            ClauseKind::With => self.analyze_projection(clause, input, true),
            ClauseKind::Let => self.analyze_let(clause, input),
            ClauseKind::Unwind | ClauseKind::For => self.analyze_binding_clause(clause, input),
            ClauseKind::Foreach => self.analyze_foreach(clause, input),
            ClauseKind::Call => self.analyze_call(clause, input),
            ClauseKind::Set | ClauseKind::Remove => self.analyze_reference_only(clause, input),
            ClauseKind::Delete | ClauseKind::DetachDelete => self.analyze_delete(clause, input),
            ClauseKind::LoadCsv => self.analyze_load_csv(clause, input),
            ClauseKind::Show => self.analyze_show(clause, input),
            ClauseKind::CreateIndex
            | ClauseKind::DropIndex
            | ClauseKind::CreateConstraint
            | ClauseKind::DropConstraint
            | ClauseKind::GraphType => Ok(input.clone()),
            ClauseKind::Finish => Ok(Scope::new()),
        }
    }

    fn analyze_match(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        let mut scope = input.clone();
        self.bind_match_pattern(clause, &mut scope)?;
        self.validate_expression_references(clause, &scope)?;
        self.validate_types(clause)?;
        self.validate_match_semantics(clause, &scope)?;
        for search in clause
            .descendants()
            .filter(|node| node.kind == AstKind::Search)
        {
            self.validate_search(search, &mut scope)?;
        }
        Ok(scope)
    }

    fn analyze_pattern_write(
        &mut self,
        kind: ClauseKind,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        self.validate_write_pattern_semantics(kind, clause, input)?;
        let mut scope = input.clone();
        self.bind_pattern(clause, &mut scope)?;
        self.validate_expression_references(clause, &scope)?;
        self.validate_types(clause)?;
        Ok(scope)
    }

    fn analyze_filter(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        self.validate_expression_references(clause, input)?;
        self.validate_types(clause)?;
        self.validate_non_aggregate_expression_context(
            clause,
            input,
            "FILTER/WHERE cannot contain aggregation",
        )?;
        Ok(input.clone())
    }

    fn analyze_reference_only(
        &mut self,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        self.validate_expression_references(clause, input)?;
        self.validate_types(clause)?;
        self.validate_non_aggregate_expression_context(
            clause,
            input,
            "mutation clauses cannot contain aggregation",
        )?;
        Ok(input.clone())
    }

    fn analyze_delete(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        self.validate_expression_references(clause, input)?;
        self.validate_types(clause)?;
        self.validate_delete_semantics(clause, input)?;
        Ok(input.clone())
    }

    fn bind_match_pattern(&self, node: &AstNode, scope: &mut Scope) -> Result<(), FrontendError> {
        for relationship in node
            .descendants()
            .filter(|child| child.kind == AstKind::RelationshipPattern)
        {
            if !relationship
                .descendants()
                .any(|child| child.kind == AstKind::VariableLength)
            {
                continue;
            }
            let variable = relationship
                .descendants()
                .find(|child| child.kind == AstKind::RelationshipVariable)
                .and_then(|child| child.text.as_deref())
                .map(unescape_identifier);
            if let Some(variable) = variable
                && scope.get(&variable) == Some(&BindingKind::List)
            {
                scope.insert(variable, BindingKind::Unknown);
            }
        }
        self.bind_pattern_node(node, scope, None, true)
    }

    fn bind_pattern(&self, node: &AstNode, scope: &mut Scope) -> Result<(), FrontendError> {
        self.bind_pattern_node(node, scope, None, false)
    }

    fn bind_pattern_node(
        &self,
        node: &AstNode,
        scope: &mut Scope,
        override_kind: Option<BindingKind>,
        skip_expressions: bool,
    ) -> Result<(), FrontendError> {
        if skip_expressions && matches!(node.kind, AstKind::Expression(_)) {
            return Ok(());
        }
        let kind = match node.kind {
            AstKind::PathAssignment => Some(BindingKind::Path),
            AstKind::PatternVariable => Some(override_kind.unwrap_or(BindingKind::Node)),
            AstKind::RelationshipVariable => Some(BindingKind::Relationship),
            _ => None,
        };
        if let Some(kind) = kind
            && let Some(name) = node.text.as_deref()
        {
            self.bind(scope, name, kind, node.span)?;
        }
        let child_override = if node.kind == AstKind::PathAssignment {
            Some(BindingKind::Path)
        } else {
            override_kind
        };
        for child in &node.children {
            self.bind_pattern_node(child, scope, child_override, skip_expressions)?;
        }
        Ok(())
    }

    fn analyze_projection(
        &mut self,
        clause: &AstNode,
        input: &Scope,
        replace_scope: bool,
    ) -> Result<Scope, FrontendError> {
        let body = find_descendant(clause, AstKind::ProjectionBody).unwrap_or(clause);
        let mut output = if has_star_projection(body) {
            input.clone()
        } else {
            Scope::new()
        };
        for item in projection_items(body) {
            let (name, kind) = self.projection_binding(item, input);
            if let Some(name) = name
                && output.insert(name.clone(), kind).is_some()
            {
                return Err(self.semantic_error(
                    item.span,
                    format!("projection defines duplicate output name {name:?}"),
                ));
            }
        }
        self.validate_projection_semantics(clause, input, &output, replace_scope)?;
        if replace_scope || !output.is_empty() {
            Ok(output)
        } else {
            Ok(input.clone())
        }
    }

    fn projection_binding(&self, item: &AstNode, input: &Scope) -> (Option<String>, BindingKind) {
        let alias = find_descendant(item, AstKind::ProjectionAlias)
            .and_then(|node| node.text.as_deref())
            .map(unescape_identifier);
        if let Some(variable) = simple_projection_variable(item) {
            let variable = unescape_identifier(variable);
            let kind = input
                .get(&variable)
                .copied()
                .unwrap_or(BindingKind::Unknown);
            return (alias.or(Some(variable)), kind);
        }
        let expression = item
            .children
            .iter()
            .find(|child| matches!(child.kind, AstKind::Expression(_)));
        let kind = expression
            .map(|expression| expression_binding_kind(expression, self.source))
            .unwrap_or(BindingKind::Unknown);
        let name = alias.or_else(|| {
            expression
                .map(|expression| self.slice(expression.span).trim().to_owned())
                .filter(|name| !name.is_empty())
        });
        (name, kind)
    }

    fn analyze_let(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        let mut scope = input.clone();
        for binding in clause
            .descendants()
            .filter(|node| node.kind == AstKind::LetBinding)
        {
            let name = find_descendant(binding, AstKind::BindingVariable)
                .and_then(|node| node.text.as_deref())
                .ok_or_else(|| {
                    self.semantic_error(binding.span, "LET binding is missing a variable")
                })?;
            self.validate_expression_references(binding, input)?;
            self.validate_types(binding)?;
            self.validate_non_aggregate_expression_context(
                binding,
                input,
                "LET cannot contain aggregation",
            )?;
            let name = unescape_identifier(name);
            if scope.contains_key(&name) {
                return Err(self.semantic_error(
                    binding.span,
                    format!("LET variable {name:?} is already defined"),
                ));
            }
            let kind = binding
                .children
                .iter()
                .find(|child| matches!(child.kind, AstKind::Expression(_)))
                .map(|expression| expression_binding_kind(expression, self.source))
                .unwrap_or(BindingKind::Unknown);
            scope.insert(name, kind);
        }
        Ok(scope)
    }

    fn analyze_binding_clause(
        &mut self,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        self.validate_expression_references(clause, input)?;
        self.validate_types(clause)?;
        self.validate_non_aggregate_expression_context(
            clause,
            input,
            "UNWIND/FOR cannot contain aggregation",
        )?;
        let binding = find_descendant(clause, AstKind::BindingVariable)
            .and_then(|node| node.text.as_deref())
            .ok_or_else(|| {
                self.semantic_error(clause.span, "binding clause is missing a variable")
            })?;
        let mut scope = input.clone();
        let binding = unescape_identifier(binding);
        if scope.contains_key(&binding) {
            return Err(self.semantic_error(
                clause.span,
                format!("binding variable {binding:?} is already defined"),
            ));
        }
        scope.insert(binding, BindingKind::Unknown);
        Ok(scope)
    }

    fn analyze_show(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        let show_fields = show_all_scope(clause);
        let default = show_default_scope(clause);
        let Some(yield_node) = show_yield_node(clause) else {
            if !input.is_empty() {
                return Err(self.semantic_error(
                    clause.span,
                    "composable SHOW requires an explicit YIELD column list and a concluding clause",
                ));
            }
            if clause
                .children
                .iter()
                .any(|node| node.kind == AstKind::Clause(ClauseKind::Return))
            {
                return Err(self
                    .semantic_error(clause.span, "SHOW RETURN requires an explicit YIELD clause"));
            }
            self.validate_show_where(clause, &show_fields)?;
            return Ok(default);
        };
        if !input.is_empty()
            && yield_node
                .descendants()
                .any(|node| node.kind == AstKind::YieldAll)
        {
            return Err(
                self.semantic_error(yield_node.span, "composable SHOW does not allow YIELD *")
            );
        }
        for item in projection_items(yield_node) {
            let Some(source) = simple_projection_variable(item) else {
                return Err(self.semantic_error(
                    item.span,
                    "SHOW YIELD accepts only output field names with optional aliases",
                ));
            };
            if !show_fields.contains_key(&unescape_identifier(source)) {
                return Err(self.semantic_error(
                    item.span,
                    "SHOW YIELD can reference only fields produced by the SHOW command",
                ));
            }
        }
        let mut all = input.clone();
        all.extend(show_fields);
        let projection = show_projection_clause(yield_node);
        let yielded = self.analyze_projection(&projection, &all, true)?;
        if let Some(name) = yielded.keys().find(|name| input.contains_key(*name)) {
            return Err(self.semantic_error(
                yield_node.span,
                format!("SHOW YIELD variable {name} is already declared"),
            ));
        }
        let mut visible = input.clone();
        visible.extend(yielded);
        self.validate_show_where(clause, &visible)?;
        if let Some(return_clause) = clause
            .children
            .iter()
            .find(|node| node.kind == AstKind::Clause(ClauseKind::Return))
        {
            self.analyze_projection(return_clause, &visible, false)
        } else {
            Ok(visible)
        }
    }

    fn validate_show_where(
        &mut self,
        clause: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        if let Some(where_clause) = clause
            .children
            .iter()
            .find(|node| node.kind == AstKind::Where)
        {
            self.analyze_filter(where_clause, scope)?;
        }
        Ok(())
    }

    fn analyze_foreach(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        let binding = find_descendant(clause, AstKind::BindingVariable)
            .and_then(|node| node.text.as_deref())
            .ok_or_else(|| self.semantic_error(clause.span, "FOREACH is missing its variable"))?;
        let collection = clause
            .children
            .iter()
            .find(|child| matches!(child.kind, AstKind::Expression(_)))
            .ok_or_else(|| {
                self.semantic_error(clause.span, "FOREACH is missing its list expression")
            })?;
        self.validate_expression_references(collection, input)?;
        self.validate_types(collection)?;
        self.validate_non_aggregate_expression_context(
            collection,
            input,
            "FOREACH collection cannot contain aggregation",
        )?;
        let mut inner = input.clone();
        inner.insert(unescape_identifier(binding), BindingKind::Value);
        for nested in &clause.children {
            if let AstKind::Clause(kind) = nested.kind {
                inner = self.analyze_clause(kind, nested, &inner)?;
            }
        }
        Ok(input.clone())
    }

    fn analyze_call(&mut self, clause: &AstNode, input: &Scope) -> Result<Scope, FrontendError> {
        if let Some(subquery) = clause
            .descendants()
            .find(|node| matches!(node.kind, AstKind::Subquery(SubqueryKind::Call)))
        {
            let output = self.analyze_subquery(subquery, input, false)?;
            let mut scope = input.clone();
            for (name, kind) in output {
                if scope.insert(name.clone(), kind).is_some() {
                    return Err(self.semantic_error(
                        subquery.span,
                        format!("subquery output shadows existing variable {name:?}"),
                    ));
                }
            }
            return Ok(scope);
        }
        self.validate_procedure_arguments(clause, input)?;
        let standalone = self.standalone_procedure_call == Some(clause.span);
        let yield_all = clause
            .descendants()
            .any(|node| node.kind == AstKind::YieldAll);
        let yield_items = clause
            .descendants()
            .filter(|node| node.kind == AstKind::YieldItem)
            .collect::<Vec<_>>();
        let procedure = clause
            .descendants()
            .find(|node| node.kind == AstKind::FunctionName)
            .and_then(|node| node.text.as_deref())
            .and_then(crate::query::registry::procedure);
        self.validate_procedure_yield_shape(
            clause,
            standalone,
            yield_all,
            &yield_items,
            procedure,
        )?;
        let mut scope = input.clone();
        self.bind_procedure_yields(&mut scope, standalone, yield_all, &yield_items, procedure)?;
        if let Some(where_clause) = clause
            .descendants()
            .find(|node| node.kind == AstKind::Where)
        {
            self.analyze_filter(where_clause, &scope)?;
        }
        Ok(scope)
    }

    fn validate_procedure_yield_shape(
        &self,
        clause: &AstNode,
        standalone: bool,
        yield_all: bool,
        yield_items: &[&AstNode],
        procedure: Option<crate::query::registry::ProcedureDefinition>,
    ) -> Result<(), FrontendError> {
        if !standalone && yield_all {
            return Err(self.semantic_error(
                clause.span,
                "YIELD * is only valid for a standalone procedure call",
            ));
        }
        if !standalone
            && yield_items.is_empty()
            && procedure.is_some_and(|definition| !definition.outputs.is_empty())
        {
            return Err(self.semantic_error(
                clause.span,
                "a procedure call inside a larger query requires an explicit YIELD column list",
            ));
        }
        Ok(())
    }

    fn bind_procedure_yields(
        &self,
        scope: &mut Scope,
        standalone: bool,
        yield_all: bool,
        yield_items: &[&AstNode],
        procedure: Option<crate::query::registry::ProcedureDefinition>,
    ) -> Result<(), FrontendError> {
        if (yield_all || (standalone && yield_items.is_empty()))
            && let Some(definition) = procedure
        {
            for name in definition.outputs {
                scope.insert((*name).to_owned(), BindingKind::Value);
            }
        }
        for item in yield_items {
            self.bind_procedure_yield_item(scope, item, procedure)?;
        }
        Ok(())
    }

    fn bind_procedure_yield_item(
        &self,
        scope: &mut Scope,
        item: &AstNode,
        procedure: Option<crate::query::registry::ProcedureDefinition>,
    ) -> Result<(), FrontendError> {
        if let Some(definition) = procedure {
            let source = find_descendant(item, AstKind::YieldName)
                .and_then(|node| node.text.as_deref())
                .map(unescape_identifier)
                .unwrap_or_default();
            if !definition.outputs.contains(&source.as_str()) {
                return Err(self.semantic_error(
                    item.span,
                    format!("procedure does not yield output field {source:?}"),
                ));
            }
        }
        let name = yield_output_name(item);
        if !name.is_empty() && scope.insert(name.clone(), BindingKind::Value).is_some() {
            return Err(self.semantic_error(
                item.span,
                format!("YIELD output shadows existing variable {name:?}"),
            ));
        }
        Ok(())
    }

    fn validate_procedure_arguments(
        &mut self,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<(), FrontendError> {
        for arguments in clause
            .descendants()
            .filter(|node| node.kind == AstKind::ArgumentList)
        {
            self.validate_expression_references(arguments, input)?;
            self.validate_types(arguments)?;
            self.validate_procedure_argument_semantics(arguments, input)?;
        }
        Ok(())
    }

    pub(super) fn analyze_subquery(
        &mut self,
        node: &AstNode,
        outer: &Scope,
        correlated: bool,
    ) -> Result<Scope, FrontendError> {
        self.validate_expression_subquery_is_read_only(node, correlated)?;
        let scope_node = node
            .children
            .iter()
            .find(|child| child.kind == AstKind::SubqueryScope);
        let imported = match scope_node {
            Some(scope_node) => self.import_scope(scope_node, outer)?,
            None if correlated => outer.clone(),
            None => Scope::new(),
        };
        let status_name = self.validate_transaction_subclause(node, outer, &imported)?;
        if let Some(body) = node
            .descendants()
            .find(|child| child.kind == AstKind::QueryBody)
        {
            return self.analyze_subquery_body(
                node,
                body,
                outer,
                &imported,
                scope_node.is_some(),
                status_name,
            );
        }
        let mut local = imported;
        self.bind_pattern(node, &mut local)?;
        for child in &node.children {
            self.validate_references_inner(child, &local, true)?;
        }
        Ok(Scope::new())
    }

    fn analyze_subquery_body(
        &mut self,
        node: &AstNode,
        body: &AstNode,
        outer: &Scope,
        imported: &Scope,
        has_scope_node: bool,
        status_name: Option<String>,
    ) -> Result<Scope, FrontendError> {
        let legacy_call = node.kind == AstKind::Subquery(SubqueryKind::Call) && !has_scope_node;
        let globals = if legacy_call {
            Scope::new()
        } else {
            imported.clone()
        };
        let previous_globals = std::mem::replace(&mut self.global_scope, globals);
        let previous_nonconcluding_len = self.nonconcluding_queries.len();
        if matches!(
            node.kind,
            AstKind::Subquery(SubqueryKind::Exists | SubqueryKind::Count)
        ) {
            super::semantic::collect_terminal_single_query_spans(
                body,
                &mut self.nonconcluding_queries,
            );
        }
        let analyzed = if legacy_call {
            self.analyze_call_body_with_importing_with(body, outer)
        } else {
            self.analyze_node(body, imported)
        };
        self.global_scope = previous_globals;
        self.nonconcluding_queries
            .truncate(previous_nonconcluding_len);
        let mut output = analyzed?;
        if !super::semantic_transaction::query_body_returns_columns(body) {
            output.clear();
        }
        self.bind_transaction_status(node, &mut output, status_name)?;
        Ok(output)
    }

    fn bind_transaction_status(
        &self,
        node: &AstNode,
        output: &mut Scope,
        status_name: Option<String>,
    ) -> Result<(), FrontendError> {
        let Some(name) = status_name else {
            return Ok(());
        };
        if output.insert(name.clone(), BindingKind::Value).is_some() {
            return Err(self.semantic_error(
                node.span,
                format!("transaction status shadows subquery output {name:?}"),
            ));
        }
        Ok(())
    }

    fn import_scope(&self, node: &AstNode, outer: &Scope) -> Result<Scope, FrontendError> {
        if node
            .descendants()
            .any(|child| child.kind == AstKind::SubqueryScopeAll)
        {
            return Ok(outer.clone());
        }
        let mut scope = Scope::new();
        for import in node
            .descendants()
            .filter(|child| child.kind == AstKind::SubqueryImport)
        {
            let name = import.text.as_deref().ok_or_else(|| {
                self.semantic_error(import.span, "subquery import is missing a variable")
            })?;
            let key = unescape_identifier(name);
            let kind = outer.get(&key).copied().ok_or_else(|| {
                self.semantic_error(
                    import.span,
                    format!("subquery imports undefined variable {key:?}"),
                )
            })?;
            scope.insert(key, kind);
        }
        Ok(scope)
    }

    fn analyze_call_body_with_importing_with(
        &mut self,
        body: &AstNode,
        outer: &Scope,
    ) -> Result<Scope, FrontendError> {
        let query = body
            .children
            .iter()
            .find(|child| {
                matches!(
                    child.kind,
                    AstKind::ComposedQuery | AstKind::ConditionalQuery | AstKind::SingleQuery
                )
            })
            .ok_or_else(|| self.semantic_error(body.span, "CALL subquery has no query body"))?;
        self.analyze_call_query_with_importing_with(query, outer)
            .map(|(_, output)| output)
    }

    fn analyze_call_query_with_importing_with(
        &mut self,
        query: &AstNode,
        outer: &Scope,
    ) -> Result<(bool, Scope), FrontendError> {
        match query.kind {
            AstKind::SingleQuery => {
                let imported = self.importing_with_scope(query, outer)?;
                let used_importing_with = imported.is_some();
                let output =
                    self.analyze_node(query, imported.as_ref().unwrap_or(&Scope::new()))?;
                Ok((used_importing_with, output))
            }
            AstKind::Subquery(SubqueryKind::Braced) => {
                let body = query
                    .children
                    .iter()
                    .find(|child| child.kind == AstKind::QueryBody)
                    .ok_or_else(|| {
                        self.semantic_error(query.span, "braced query is missing its body")
                    })?;
                let nested = body
                    .children
                    .iter()
                    .find(|child| {
                        matches!(
                            child.kind,
                            AstKind::ComposedQuery
                                | AstKind::ConditionalQuery
                                | AstKind::SingleQuery
                        )
                    })
                    .ok_or_else(|| {
                        self.semantic_error(body.span, "braced query has no executable query")
                    })?;
                self.analyze_call_query_with_importing_with(nested, outer)
            }
            AstKind::ComposedQuery => self.analyze_call_composed(query, outer),
            AstKind::ConditionalQuery => self
                .analyze_node(query, &Scope::new())
                .map(|output| (false, output)),
            _ => Err(self.semantic_error(query.span, "invalid CALL subquery body")),
        }
    }

    fn analyze_call_composed(
        &mut self,
        query: &AstNode,
        outer: &Scope,
    ) -> Result<(bool, Scope), FrontendError> {
        self.validate_union_connectors(query)?;
        let mut previous = Scope::new();
        let mut segment_input = Scope::new();
        let mut connector = None;
        let mut union_output: Option<Scope> = None;
        let mut used_importing_with = false;
        let mut segment_operands = Vec::new();
        let mut uses_segment_input = false;
        for child in &query.children {
            match child.kind {
                AstKind::Connector(value) => {
                    if self.advance_composed_connector(
                        value,
                        &mut previous,
                        &mut segment_input,
                        &mut segment_operands,
                        &mut union_output,
                    )? {
                        uses_segment_input = true;
                    }
                    connector = Some(value);
                }
                AstKind::SingleQuery | AstKind::Subquery(SubqueryKind::Braced) => {
                    let (used, output) = if uses_segment_input {
                        (false, self.analyze_node(child, &segment_input)?)
                    } else {
                        self.analyze_call_query_with_importing_with(child, outer)?
                    };
                    used_importing_with |= used;
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
        if used_importing_with
            && query
                .children
                .iter()
                .any(|child| child.kind == AstKind::Connector(QueryConnector::Next))
        {
            return Err(self.semantic_error(
                query.span,
                "NEXT cannot be used with the deprecated importing WITH syntax",
            ));
        }
        Ok((used_importing_with, previous))
    }

    fn importing_with_scope(
        &self,
        single: &AstNode,
        outer: &Scope,
    ) -> Result<Option<Scope>, FrontendError> {
        let Some(first) = single
            .children
            .iter()
            .find(|child| matches!(child.kind, AstKind::Clause(_)))
        else {
            return Ok(None);
        };
        if first.kind != AstKind::Clause(ClauseKind::With) {
            return Ok(None);
        }
        let body = find_descendant(first, AstKind::ProjectionBody).unwrap_or(first);
        let has_star = has_star_projection(body);
        let references_outer = body.descendants().any(|node| {
            node.kind == AstKind::Variable
                && node
                    .text
                    .as_deref()
                    .map(unescape_identifier)
                    .is_some_and(|name| outer.contains_key(&name))
        });
        if !has_star && !references_outer {
            return Ok(None);
        }
        if first
            .children
            .iter()
            .any(|child| child.kind == AstKind::Where)
            || body.descendants().any(|node| {
                matches!(
                    node.kind,
                    AstKind::SetQuantifier(_)
                        | AstKind::GroupBy
                        | AstKind::OrderBy
                        | AstKind::Skip
                        | AstKind::Limit
                )
            })
        {
            return Err(self.semantic_error(
                first.span,
                "importing WITH accepts only direct outer-variable references",
            ));
        }
        if has_star {
            if projection_items(body).len() != 1 {
                return Err(self.semantic_error(
                    first.span,
                    "importing WITH * cannot include additional projection items",
                ));
            }
            return Ok(Some(outer.clone()));
        }
        let mut imported = Scope::new();
        for item in projection_items(body) {
            if find_descendant(item, AstKind::ProjectionAlias).is_some() {
                return Err(
                    self.semantic_error(item.span, "importing WITH cannot alias an outer variable")
                );
            }
            let name = simple_projection_variable(item)
                .map(unescape_identifier)
                .ok_or_else(|| {
                    self.semantic_error(
                        item.span,
                        "importing WITH accepts only direct outer-variable references",
                    )
                })?;
            let binding = outer.get(&name).copied().ok_or_else(|| {
                self.semantic_error(
                    item.span,
                    format!("importing WITH references undefined outer variable {name:?}"),
                )
            })?;
            imported.insert(name, binding);
        }
        Ok(Some(imported))
    }

    pub(super) fn validate_global_redeclaration(
        &self,
        kind: ClauseKind,
        clause: &AstNode,
    ) -> Result<(), FrontendError> {
        if self.global_scope.is_empty() {
            return Ok(());
        }
        if matches!(kind, ClauseKind::With | ClauseKind::Return) {
            let body = find_descendant(clause, AstKind::ProjectionBody).unwrap_or(clause);
            for item in projection_items(body) {
                let (name, _) = self.projection_binding(item, &self.global_scope);
                let Some(name) = name.filter(|name| self.global_scope.contains_key(name)) else {
                    continue;
                };
                let direct = find_descendant(item, AstKind::ProjectionAlias).is_none()
                    && simple_projection_variable(item)
                        .map(unescape_identifier)
                        .is_some_and(|variable| variable == name);
                if !direct {
                    return Err(self.semantic_error(
                        item.span,
                        format!("subquery cannot re-declare imported variable {name:?}"),
                    ));
                }
            }
        }
        let bindings = match kind {
            ClauseKind::Let => clause
                .descendants()
                .filter(|node| node.kind == AstKind::LetBinding)
                .filter_map(|node| find_descendant(node, AstKind::BindingVariable))
                .collect::<Vec<_>>(),
            ClauseKind::Unwind | ClauseKind::For | ClauseKind::Foreach => {
                find_descendant(clause, AstKind::BindingVariable)
                    .into_iter()
                    .collect()
            }
            ClauseKind::LoadCsv => find_descendant(clause, AstKind::LoadCsvBinding)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
        for binding in bindings {
            if let Some(name) = binding.text.as_deref().map(unescape_identifier)
                && self.global_scope.contains_key(&name)
            {
                return Err(self.semantic_error(
                    binding.span,
                    format!("subquery cannot re-declare imported variable {name:?}"),
                ));
            }
        }
        Ok(())
    }

    fn analyze_load_csv(
        &mut self,
        clause: &AstNode,
        input: &Scope,
    ) -> Result<Scope, FrontendError> {
        self.validate_expression_references(clause, input)?;
        self.validate_non_aggregate_expression_context(
            clause,
            input,
            "LOAD CSV input cannot contain aggregation",
        )?;
        let binding = clause
            .descendants()
            .find(|node| node.kind == AstKind::LoadCsvBinding)
            .ok_or_else(|| self.semantic_error(clause.span, "LOAD CSV is missing AS binding"))?;
        let name = binding
            .text
            .as_deref()
            .ok_or_else(|| self.semantic_error(binding.span, "LOAD CSV is missing AS binding"))?;
        let mut scope = input.clone();
        scope.insert(unescape_identifier(name), BindingKind::Value);
        Ok(scope)
    }

    fn validate_search(&self, search: &AstNode, scope: &mut Scope) -> Result<(), FrontendError> {
        let binding = search
            .descendants()
            .find(|node| node.kind == AstKind::Variable)
            .and_then(|node| node.text.as_deref())
            .ok_or_else(|| {
                self.semantic_error(search.span, "SEARCH is missing its binding variable")
            })?;
        let binding = unescape_identifier(binding);
        match scope.get(&binding) {
            Some(BindingKind::Node | BindingKind::Relationship) => {}
            Some(_) => {
                return Err(self.semantic_error(
                    search.span,
                    "SEARCH binding must be a node or relationship variable",
                ));
            }
            None => {
                return Err(self.semantic_error(
                    search.span,
                    format!("SEARCH uses undefined variable {binding:?}"),
                ));
            }
        }
        if let Some(alias) =
            find_descendant(search, AstKind::ProjectionAlias).and_then(|node| node.text.as_deref())
        {
            scope.insert(unescape_identifier(alias), BindingKind::Value);
        }
        Ok(())
    }

    pub(super) fn validate_expression_references(
        &mut self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        self.validate_references_inner(node, scope, false)
    }

    fn validate_references_inner(
        &mut self,
        node: &AstNode,
        scope: &Scope,
        nested_subquery: bool,
    ) -> Result<(), FrontendError> {
        if matches!(node.kind, AstKind::Subquery(_)) {
            if nested_subquery {
                return Ok(());
            }
            let _ = self.analyze_subquery(node, scope, true)?;
            return Ok(());
        }
        if let Some(binding) = direct_local_binding(node) {
            return self.validate_local_binding_expression(node, binding, scope);
        }
        if is_pattern_comprehension(node) {
            let mut local = scope.clone();
            self.bind_pattern(node, &mut local)?;
            for child in &node.children {
                self.validate_references_inner(child, &local, nested_subquery)?;
            }
            return Ok(());
        }
        if self.validate_reference_node(node, scope)? {
            return Ok(());
        }
        for child in &node.children {
            self.validate_references_inner(child, scope, nested_subquery)?;
        }
        Ok(())
    }

    fn validate_reference_node(
        &mut self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<bool, FrontendError> {
        if node.kind == AstKind::Variable {
            let name = unescape_identifier(node.text.as_deref().unwrap_or_default());
            if !scope.contains_key(&name) {
                return Err(self.semantic_error(node.span, format!("undefined variable {name:?}")));
            }
        }
        if matches!(
            node.kind,
            AstKind::Expression(ExpressionKind::InterpolatedString)
        ) {
            self.validate_interpolation(node, scope)?;
        }
        let is_reduction = node
            .children
            .iter()
            .any(|child| child.kind == AstKind::ReductionAccumulator);
        if is_reduction {
            self.validate_reduction_expression(node, scope)?;
        }
        Ok(is_reduction)
    }

    fn validate_reduction_expression(
        &mut self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        let accumulator = node
            .children
            .iter()
            .find(|child| child.kind == AstKind::ReductionAccumulator)
            .and_then(|child| child.text.as_deref())
            .ok_or_else(|| {
                self.semantic_error(node.span, "reduction function has no accumulator variable")
            })?;
        let variable = node
            .children
            .iter()
            .find(|child| child.kind == AstKind::ReductionVariable)
            .and_then(|child| child.text.as_deref())
            .ok_or_else(|| {
                self.semantic_error(node.span, "reduction function has no step variable")
            })?;
        let accumulator = unescape_identifier(accumulator);
        let variable = unescape_identifier(variable);
        if accumulator == variable {
            return Err(self.semantic_error(
                node.span,
                "reduction accumulator and step variable must be different",
            ));
        }
        let expressions = node
            .children
            .iter()
            .filter(|child| matches!(child.kind, AstKind::Expression(_)))
            .collect::<Vec<_>>();
        if !(3..=4).contains(&expressions.len()) {
            return Err(self.semantic_error(node.span, "invalid reduction expression shape"));
        }
        self.validate_references_inner(expressions[0], scope, false)?;
        self.validate_references_inner(expressions[1], scope, false)?;
        match infer_expression(expressions[1], self.source)? {
            CypherType::List(_) | CypherType::Any | CypherType::Null => {}
            _ => {
                return Err(self.semantic_error(
                    expressions[1].span,
                    "reduction IN expression must produce a List",
                ));
            }
        }
        let mut local = scope.clone();
        local.insert(accumulator, BindingKind::Unknown);
        local.insert(variable, BindingKind::Unknown);
        for expression in expressions.iter().skip(2) {
            self.validate_references_inner(expression, &local, false)?;
            if contains_aggregate(expression) {
                return Err(self.semantic_error(
                    expression.span,
                    "reduction expressions cannot contain aggregation",
                ));
            }
        }
        Ok(())
    }

    fn validate_local_binding_expression(
        &mut self,
        node: &AstNode,
        binding: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        let name = binding.text.as_deref().ok_or_else(|| {
            self.semantic_error(
                binding.span,
                "local expression binding is missing a variable",
            )
        })?;
        let expressions = node
            .children
            .iter()
            .filter(|child| matches!(child.kind, AstKind::Expression(_)))
            .collect::<Vec<_>>();
        let element_type = if let Some(collection) = expressions.first() {
            self.validate_references_inner(collection, scope, false)?;
            match infer_expression(collection, self.source)? {
                CypherType::List(element) => Some(*element),
                CypherType::Any | CypherType::Null => None,
                _ => {
                    return Err(self.semantic_error(
                        collection.span,
                        "quantifier/comprehension IN expression must produce a List",
                    ));
                }
            }
        } else {
            None
        };
        let binding_name = unescape_identifier(name);
        let mut local = scope.clone();
        local.insert(binding_name.clone(), BindingKind::Unknown);
        for expression in expressions.iter().skip(1) {
            self.validate_references_inner(expression, &local, false)?;
            if contains_aggregate(expression) {
                return Err(self.semantic_error(
                    expression.span,
                    "comprehension predicate/projection cannot contain aggregation",
                ));
            }
            if let Some(element_type) = &element_type {
                validate_local_binding_type_use(
                    expression,
                    &binding_name,
                    element_type,
                    self.source,
                )?;
            }
        }
        Ok(())
    }

    fn validate_interpolation(
        &mut self,
        node: &AstNode,
        scope: &Scope,
    ) -> Result<(), FrontendError> {
        let text = node.text.as_deref().unwrap_or_default();
        for fragment in interpolation_fragments(text)
            .map_err(|message| self.semantic_error(node.span, message))?
        {
            let expression = parse_expression_fragment_at(
                fragment.text,
                self.source,
                node.span.start.saturating_add(fragment.offset),
            )
            .map_err(|mut error| {
                error.kind = FrontendErrorKind::Semantic;
                error.message =
                    format!("invalid string interpolation expression: {}", error.message);
                error
            })?;
            self.validate_expression_references(&expression, scope)?;
            self.validate_types(&expression)?;
            let value_type = infer_expression(&expression, self.source)?;
            let variable_kind = simple_projection_variable(&expression)
                .map(unescape_identifier)
                .and_then(|name| scope.get(&name).copied());
            if matches!(
                value_type,
                CypherType::List(_)
                    | CypherType::Map
                    | CypherType::Node
                    | CypherType::Relationship
                    | CypherType::Path
            ) || matches!(
                variable_kind,
                Some(
                    BindingKind::List
                        | BindingKind::Map
                        | BindingKind::Node
                        | BindingKind::Relationship
                        | BindingKind::Path
                )
            ) {
                return Err(FrontendError::new(
                    FrontendErrorKind::Type,
                    "string interpolation only accepts values supported by toString()",
                    expression.span,
                    self.source,
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn show_yield_node(clause: &AstNode) -> Option<&AstNode> {
    clause.children.iter().find(|node| {
        !matches!(node.kind, AstKind::Clause(_))
            && node
                .descendants()
                .any(|child| matches!(child.kind, AstKind::YieldAll | AstKind::ProjectionItem))
    })
}

pub(crate) fn show_projection_clause(yield_node: &AstNode) -> AstNode {
    fn projection_child(node: &AstNode) -> AstNode {
        let mut node = node.clone();
        if node.kind == AstKind::YieldAll {
            node.kind = AstKind::StarProjection;
        }
        node.children = node.children.iter().map(projection_child).collect();
        node
    }

    AstNode {
        kind: AstKind::Clause(ClauseKind::Return),
        span: yield_node.span,
        text: None,
        children: vec![AstNode {
            kind: AstKind::ProjectionBody,
            span: yield_node.span,
            text: None,
            children: yield_node.children.iter().map(projection_child).collect(),
        }],
    }
}

fn show_all_scope(clause: &AstNode) -> Scope {
    let fields: &[&str] = if show_target(clause) == Some(ShowTargetKind::Procedures) {
        &[
            "name",
            "description",
            "mode",
            "worksOnSystem",
            "signature",
            "argumentDescription",
            "returnDescription",
            "admin",
            "rolesExecution",
            "rolesBoostedExecution",
            "isDeprecated",
            "deprecatedBy",
            "option",
        ]
    } else {
        &[
            "name",
            "category",
            "description",
            "signature",
            "isBuiltIn",
            "argumentDescription",
            "returnDescription",
            "aggregating",
            "rolesExecution",
            "rolesBoostedExecution",
            "isDeprecated",
            "deprecatedBy",
        ]
    };
    fields
        .iter()
        .map(|name| ((*name).to_owned(), BindingKind::Value))
        .collect()
}

fn show_default_scope(clause: &AstNode) -> Scope {
    let mut scope = show_all_scope(clause);
    if show_target(clause) == Some(ShowTargetKind::Procedures) {
        scope.retain(|name, _| {
            matches!(
                name.as_str(),
                "name" | "description" | "mode" | "worksOnSystem"
            )
        });
    } else {
        scope.retain(|name, _| matches!(name.as_str(), "name" | "category" | "description"));
    }
    scope
}

fn show_target(clause: &AstNode) -> Option<ShowTargetKind> {
    clause.descendants().find_map(|node| match node.kind {
        AstKind::ShowTarget(target) => Some(target),
        _ => None,
    })
}
