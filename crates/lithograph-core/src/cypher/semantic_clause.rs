use super::ast::{AstKind, AstNode, ClauseKind, ExpressionKind, SubqueryKind};
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
            ClauseKind::CreateIndex
            | ClauseKind::DropIndex
            | ClauseKind::CreateConstraint
            | ClauseKind::DropConstraint
            | ClauseKind::Show
            | ClauseKind::GraphType
            | ClauseKind::Finish => Ok(input.clone()),
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
            self.validate_expression_references(binding, &scope)?;
            self.validate_types(binding)?;
            self.validate_non_aggregate_expression_context(
                binding,
                &scope,
                "LET cannot contain aggregation",
            )?;
            let kind = binding
                .children
                .iter()
                .find(|child| matches!(child.kind, AstKind::Expression(_)))
                .map(|expression| expression_binding_kind(expression, self.source))
                .unwrap_or(BindingKind::Unknown);
            scope.insert(unescape_identifier(name), kind);
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
        scope.insert(unescape_identifier(binding), BindingKind::Unknown);
        Ok(scope)
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
                let _ = self.analyze_clause(kind, nested, &inner)?;
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
        let mut scope = input.clone();
        for item in clause
            .descendants()
            .filter(|node| node.kind == AstKind::YieldItem)
        {
            let name = yield_output_name(item);
            if !name.is_empty() && scope.insert(name.clone(), BindingKind::Value).is_some() {
                return Err(self.semantic_error(
                    item.span,
                    format!("YIELD output shadows existing variable {name:?}"),
                ));
            }
        }
        Ok(scope)
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
            let mut output = self.analyze_node(body, &imported)?;
            if !super::semantic_transaction::query_body_returns_columns(body) {
                output.clear();
            }
            if let Some(name) = status_name
                && output.insert(name.clone(), BindingKind::Value).is_some()
            {
                return Err(self.semantic_error(
                    node.span,
                    format!("transaction status shadows subquery output {name:?}"),
                ));
            }
            return Ok(output);
        }
        let mut local = imported;
        self.bind_pattern(node, &mut local)?;
        for child in &node.children {
            self.validate_references_inner(child, &local, true)?;
        }
        Ok(Scope::new())
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
        for child in &node.children {
            self.validate_references_inner(child, scope, nested_subquery)?;
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
