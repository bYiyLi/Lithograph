use super::ast::{AstKind, AstNode, ClauseKind, IndexKind};
use super::error::{FrontendError, FrontendErrorKind, Span};
use super::semantic::unescape_identifier;
use super::types_function::parse_cypher_vector_coordinate_type;

pub(super) fn validate_schema_surface(root: &AstNode, source: &str) -> Result<(), FrontendError> {
    validate_known_types(root, source)?;
    validate_type_expressions(root, source)?;
    validate_property_surfaces(root, source)?;
    validate_schema_aliases(root, source)?;
    validate_index_surfaces(root, source)
}

fn validate_schema_aliases(root: &AstNode, source: &str) -> Result<(), FrontendError> {
    for schema_constraint in root.descendants().filter(|node| {
        matches!(
            node.kind,
            AstKind::Clause(ClauseKind::CreateConstraint) | AstKind::GraphConstraint
        )
    }) {
        let Some(requirement) = schema_constraint
            .descendants()
            .find(|node| node.kind == AstKind::ConstraintRequirement)
        else {
            continue;
        };
        let target_alias = schema_constraint_target_alias(schema_constraint);
        validate_requirement_alias(requirement, target_alias.as_deref(), source)?;
    }

    for element in root.descendants().filter(|node| {
        matches!(
            node.kind,
            AstKind::GraphNodeType | AstKind::GraphRelationshipType
        )
    }) {
        let alias = element
            .children
            .iter()
            .find(|child| child.kind == AstKind::GraphAlias)
            .and_then(|child| child.text.as_deref())
            .map(unescape_identifier);
        let property_spans = element
            .descendants()
            .filter(|node| node.kind == AstKind::GraphProperty)
            .map(|node| node.span)
            .collect::<Vec<_>>();
        for requirement in element
            .descendants()
            .filter(|node| matches!(node.kind, AstKind::ConstraintKind(_)))
            .filter(|node| {
                !property_spans
                    .iter()
                    .any(|span| span.start <= node.span.start && node.span.end <= span.end)
            })
        {
            validate_requirement_alias(requirement, alias.as_deref(), source)?;
        }
    }
    Ok(())
}

fn schema_constraint_target_alias(node: &AstNode) -> Option<String> {
    if let Some(relationship) = node
        .descendants()
        .find(|child| child.kind == AstKind::RelationshipPattern)
    {
        return relationship
            .descendants()
            .find(|child| {
                matches!(
                    child.kind,
                    AstKind::RelationshipVariable | AstKind::GraphAlias
                )
            })
            .and_then(|child| child.text.as_deref())
            .map(unescape_identifier);
    }
    node.descendants()
        .find(|child| child.kind == AstKind::NodePattern)
        .and_then(|pattern| {
            pattern
                .descendants()
                .find(|child| matches!(child.kind, AstKind::PatternVariable | AstKind::GraphAlias))
        })
        .and_then(|child| child.text.as_deref())
        .map(unescape_identifier)
}

fn validate_requirement_alias(
    requirement: &AstNode,
    target_alias: Option<&str>,
    source: &str,
) -> Result<(), FrontendError> {
    for variable in requirement
        .descendants()
        .filter(|node| node.kind == AstKind::Variable)
    {
        let actual = unescape_identifier(variable.text.as_deref().unwrap_or_default());
        if target_alias != Some(actual.as_str()) {
            return Err(schema_error(
                source,
                variable.span,
                match target_alias {
                    Some(expected) => format!(
                        "schema REQUIRE variable {actual:?} must match target alias {expected:?}"
                    ),
                    None => format!(
                        "schema REQUIRE variable {actual:?} requires a matching target alias"
                    ),
                },
            ));
        }
    }
    Ok(())
}

fn validate_known_types(root: &AstNode, source: &str) -> Result<(), FrontendError> {
    for type_node in root
        .descendants()
        .filter(|node| node.kind == AstKind::TypeName)
    {
        let Some(name) = type_node.text.as_deref() else {
            continue;
        };
        if !is_known_type_name(name) {
            return Err(type_error(
                source,
                type_node.span,
                format!("unknown Cypher type {name:?}"),
            ));
        }
    }
    for coordinate in root
        .descendants()
        .filter(|node| node.kind == AstKind::VectorCoordinateTypeName)
    {
        let Some(name) = coordinate.text.as_deref() else {
            continue;
        };
        if parse_cypher_vector_coordinate_type(name).is_none() {
            return Err(type_error(
                source,
                coordinate.span,
                format!("unknown VECTOR coordinate type {name:?}"),
            ));
        }
    }
    for dimension in root
        .descendants()
        .filter(|node| node.kind == AstKind::VectorDimension)
    {
        let value = dimension
            .text
            .as_deref()
            .and_then(|text| text.parse::<usize>().ok());
        if !matches!(value, Some(1..=4_096)) {
            return Err(type_error(
                source,
                dimension.span,
                "VECTOR type dimension must be between 1 and 4096",
            ));
        }
    }
    Ok(())
}

fn validate_type_expressions(root: &AstNode, source: &str) -> Result<(), FrontendError> {
    for expression in root
        .descendants()
        .filter(|node| node.kind == AstKind::TypeExpression)
    {
        validate_type_expression_structure(expression, source)?;
    }
    Ok(())
}

fn validate_property_surfaces(root: &AstNode, source: &str) -> Result<(), FrontendError> {
    for property in root
        .descendants()
        .filter(|node| node.kind == AstKind::GraphProperty)
    {
        let type_expression = property
            .descendants()
            .find(|node| node.kind == AstKind::TypeExpression)
            .ok_or_else(|| {
                schema_error(
                    source,
                    property.span,
                    "Graph Type property is missing its type expression",
                )
            })?;
        validate_property_type_expression(type_expression, true, source)?;
    }
    for requirement in root
        .descendants()
        .filter(|node| node.kind == AstKind::ConstraintRequirement)
    {
        if let Some(type_expression) = requirement
            .descendants()
            .find(|node| node.kind == AstKind::TypeExpression)
        {
            validate_property_type_expression(type_expression, false, source)?;
        }
    }
    Ok(())
}

fn validate_index_surfaces(root: &AstNode, source: &str) -> Result<(), FrontendError> {
    for clause in root
        .descendants()
        .filter(|node| node.kind == AstKind::Clause(ClauseKind::CreateIndex))
    {
        let additional = clause
            .descendants()
            .find(|node| node.kind == AstKind::IndexAdditionalProperties);
        let index_kind = clause.descendants().find_map(|node| match node.kind {
            AstKind::IndexKind(kind) => Some(kind),
            _ => None,
        });
        if let Some(additional) = additional
            && index_kind != Some(IndexKind::Vector)
        {
            return Err(schema_error(
                source,
                additional.span,
                "WITH additional properties is only valid for VECTOR indexes",
            ));
        }
        if index_kind == Some(IndexKind::Vector) {
            validate_vector_index(clause, source)?;
        }
    }
    Ok(())
}

fn validate_vector_index(clause: &AstNode, source: &str) -> Result<(), FrontendError> {
    let target = clause
        .descendants()
        .find(|node| node.kind == AstKind::IndexTarget)
        .ok_or_else(|| {
            schema_error(
                source,
                clause.span,
                "VECTOR index is missing its indexed property",
            )
        })?;
    if target
        .descendants()
        .filter(|node| node.kind == AstKind::PropertyKey)
        .count()
        != 1
    {
        return Err(schema_error(
            source,
            target.span,
            "VECTOR index requires exactly one indexed property",
        ));
    }
    Ok(())
}

fn is_known_type_name(name: &str) -> bool {
    matches!(
        unescape_identifier(name).to_ascii_uppercase().as_str(),
        "ANY"
            | "NOTHING"
            | "NULL"
            | "BOOLEAN"
            | "BOOL"
            | "INTEGER"
            | "INT"
            | "FLOAT"
            | "STRING"
            | "VARCHAR"
            | "LIST"
            | "ARRAY"
            | "MAP"
            | "NODE"
            | "RELATIONSHIP"
            | "PATH"
            | "DATE"
            | "LOCAL TIME"
            | "ZONED TIME"
            | "LOCAL DATETIME"
            | "ZONED DATETIME"
            | "DURATION"
            | "POINT"
            | "PROPERTY VALUE"
            | "VECTOR"
            | "UUID"
    )
}

fn validate_type_expression_structure(
    expression: &AstNode,
    source: &str,
) -> Result<(), FrontendError> {
    let terms = expression
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeTerm)
        .collect::<Vec<_>>();
    if terms.len() > 1 {
        validate_union_nullability(expression, &terms, source)?;
    }
    for term in terms {
        validate_type_term_parameters(term, source)?;
    }
    Ok(())
}

fn validate_union_nullability(
    expression: &AstNode,
    terms: &[&AstNode],
    source: &str,
) -> Result<(), FrontendError> {
    let non_null = term_outer_non_null(terms[0]);
    if terms
        .iter()
        .skip(1)
        .any(|term| term_outer_non_null(term) != non_null)
    {
        return Err(type_error(
            source,
            expression.span,
            "closed dynamic union types must make either every member nullable or every member NOT NULL",
        ));
    }
    Ok(())
}

fn validate_type_term_parameters(term: &AstNode, source: &str) -> Result<(), FrontendError> {
    let Some(type_node) = direct_type_name(term) else {
        return Ok(());
    };
    let name = canonical_type_name(type_node);
    let parameters = term
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeParameters)
        .collect::<Vec<_>>();
    if parameters.len() > 1 {
        return Err(type_error(
            source,
            term.span,
            "type expression contains multiple generic parameter groups",
        ));
    }
    let Some(parameters) = parameters.first().copied() else {
        return validate_missing_type_parameters(&name, term, source);
    };
    if !matches!(name.as_str(), "LIST" | "ARRAY" | "ANY") {
        return Err(type_error(
            source,
            parameters.span,
            format!("type {name} does not accept generic type parameters"),
        ));
    }
    let type_arguments = parameters
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeExpression)
        .count();
    if type_arguments != 1 {
        return Err(type_error(
            source,
            parameters.span,
            format!("type {name} requires exactly one type argument"),
        ));
    }
    if name == "ANY" && term_outer_non_null(term) {
        return Err(type_error(
            source,
            term.span,
            "closed dynamic union types cannot append NOT NULL to the union itself",
        ));
    }
    Ok(())
}

fn validate_missing_type_parameters(
    name: &str,
    term: &AstNode,
    source: &str,
) -> Result<(), FrontendError> {
    if matches!(name, "LIST" | "ARRAY") {
        Err(type_error(
            source,
            term.span,
            "LIST and ARRAY types require exactly one inner type",
        ))
    } else {
        Ok(())
    }
}

fn validate_property_type_expression(
    expression: &AstNode,
    allow_any_not_null: bool,
    source: &str,
) -> Result<(), FrontendError> {
    for term in expression
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeTerm)
    {
        validate_property_type_term(term, allow_any_not_null, source)?;
    }
    Ok(())
}

fn validate_property_type_term(
    term: &AstNode,
    allow_any_not_null: bool,
    source: &str,
) -> Result<(), FrontendError> {
    let Some(type_node) = direct_type_name(term) else {
        return Err(schema_error(
            source,
            term.span,
            "property type is missing its base type",
        ));
    };
    let name = canonical_type_name(type_node);
    let parameters = direct_type_parameters(term);
    let suffixes = type_list_suffix_count(term);
    match name.as_str() {
        "ANY" => validate_any_property_type(term, parameters, suffixes, allow_any_not_null, source),
        "VECTOR" => validate_vector_property_type(term, type_node, parameters, suffixes, source),
        "LIST" | "ARRAY" => validate_generic_property_list(term, parameters, suffixes, source),
        _ if suffixes > 0 => {
            validate_suffix_property_list(term, parameters, suffixes, &name, source)
        }
        _ if parameters.is_none() && is_simple_property_type(&name) => Ok(()),
        _ => Err(schema_error(
            source,
            term.span,
            format!("{name} is not a valid persistent property constraint type"),
        )),
    }
}

fn validate_any_property_type(
    term: &AstNode,
    parameters: Option<&AstNode>,
    suffixes: usize,
    allow_any_not_null: bool,
    source: &str,
) -> Result<(), FrontendError> {
    if let Some(parameters) = parameters {
        if suffixes != 0 || term_outer_non_null(term) {
            return Err(schema_error(
                source,
                term.span,
                "dynamic union property types cannot be wrapped in LIST or append NOT NULL to the union itself",
            ));
        }
        let nested = parameters
            .children
            .iter()
            .find(|node| node.kind == AstKind::TypeExpression)
            .ok_or_else(|| {
                schema_error(
                    source,
                    parameters.span,
                    "ANY dynamic union is missing its member types",
                )
            })?;
        return validate_property_type_expression(nested, false, source);
    }
    if allow_any_not_null && suffixes == 0 && term_outer_non_null(term) {
        return Ok(());
    }
    Err(schema_error(
        source,
        term.span,
        "ANY is only valid as ANY NOT NULL on a Graph Type property or as ANY<...> dynamic union syntax",
    ))
}

fn validate_vector_property_type(
    term: &AstNode,
    type_node: &AstNode,
    parameters: Option<&AstNode>,
    suffixes: usize,
    source: &str,
) -> Result<(), FrontendError> {
    let exact_coordinate = type_node
        .children
        .iter()
        .filter(|node| node.kind == AstKind::VectorCoordinateTypeName)
        .count()
        == 1;
    let exact_dimension = type_node
        .children
        .iter()
        .filter(|node| node.kind == AstKind::VectorDimension)
        .count()
        == 1;
    if parameters.is_none() && suffixes == 0 && exact_coordinate && exact_dimension {
        return Ok(());
    }
    Err(schema_error(
        source,
        term.span,
        "property VECTOR constraints require an exact coordinate type and dimension and cannot be nested in LIST",
    ))
}

fn validate_generic_property_list(
    term: &AstNode,
    parameters: Option<&AstNode>,
    suffixes: usize,
    source: &str,
) -> Result<(), FrontendError> {
    if suffixes != 0 {
        return Err(schema_error(
            source,
            term.span,
            "nested LIST values are not valid persistent property types",
        ));
    }
    let nested = parameters.and_then(single_type_argument).ok_or_else(|| {
        schema_error(
            source,
            term.span,
            "property LIST requires exactly one inner type",
        )
    })?;
    validate_property_list_inner(nested, source)
}

fn validate_suffix_property_list(
    term: &AstNode,
    parameters: Option<&AstNode>,
    suffixes: usize,
    name: &str,
    source: &str,
) -> Result<(), FrontendError> {
    if parameters.is_some() || suffixes != 1 || !is_simple_property_type(name) {
        return Err(schema_error(
            source,
            term.span,
            "persistent property lists must contain one non-null scalar property type",
        ));
    }
    let suffix_index = term
        .children
        .iter()
        .position(|node| node.kind == AstKind::TypeListSuffix)
        .unwrap_or_default();
    let inner_non_null = term.children[..suffix_index]
        .iter()
        .any(|node| node.kind == AstKind::TypeNotNull);
    if inner_non_null {
        Ok(())
    } else {
        Err(schema_error(
            source,
            term.span,
            "persistent property list element types must be NOT NULL",
        ))
    }
}

fn validate_property_list_inner(expression: &AstNode, source: &str) -> Result<(), FrontendError> {
    let mut terms = expression
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeTerm);
    let Some(term) = terms.next() else {
        return Err(schema_error(
            source,
            expression.span,
            "property LIST is missing its element type",
        ));
    };
    if terms.next().is_some() || !term_outer_non_null(term) {
        return Err(schema_error(
            source,
            expression.span,
            "persistent property LIST requires one NOT NULL scalar element type",
        ));
    }
    let Some(type_node) = direct_type_name(term) else {
        return Err(schema_error(
            source,
            term.span,
            "property LIST is missing its element type",
        ));
    };
    let nested = term
        .children
        .iter()
        .any(|node| matches!(node.kind, AstKind::TypeParameters | AstKind::TypeListSuffix));
    if nested || !is_simple_property_type(&canonical_type_name(type_node)) {
        return Err(schema_error(
            source,
            term.span,
            "persistent property LIST elements must use a non-null scalar property type",
        ));
    }
    Ok(())
}

fn direct_type_name(term: &AstNode) -> Option<&AstNode> {
    term.children
        .iter()
        .find(|node| node.kind == AstKind::TypeName)
}

fn direct_type_parameters(term: &AstNode) -> Option<&AstNode> {
    term.children
        .iter()
        .find(|node| node.kind == AstKind::TypeParameters)
}

fn single_type_argument(parameters: &AstNode) -> Option<&AstNode> {
    let mut expressions = parameters
        .children
        .iter()
        .filter(|node| node.kind == AstKind::TypeExpression);
    let first = expressions.next()?;
    expressions.next().is_none().then_some(first)
}

fn type_list_suffix_count(term: &AstNode) -> usize {
    term.children
        .iter()
        .filter(|node| node.kind == AstKind::TypeListSuffix)
        .count()
}

fn canonical_type_name(node: &AstNode) -> String {
    unescape_identifier(node.text.as_deref().unwrap_or_default()).to_ascii_uppercase()
}

fn term_outer_non_null(term: &AstNode) -> bool {
    term.children
        .last()
        .is_some_and(|node| node.kind == AstKind::TypeNotNull)
}

fn is_simple_property_type(name: &str) -> bool {
    matches!(
        name,
        "BOOLEAN"
            | "BOOL"
            | "INTEGER"
            | "INT"
            | "FLOAT"
            | "STRING"
            | "VARCHAR"
            | "DATE"
            | "LOCAL TIME"
            | "ZONED TIME"
            | "LOCAL DATETIME"
            | "ZONED DATETIME"
            | "DURATION"
            | "POINT"
            | "UUID"
    )
}

fn type_error(source: &str, span: Span, message: impl Into<String>) -> FrontendError {
    FrontendError::new(FrontendErrorKind::Type, message, span, source)
}

fn schema_error(source: &str, span: Span, message: impl Into<String>) -> FrontendError {
    FrontendError::new(FrontendErrorKind::Schema, message, span, source)
}
