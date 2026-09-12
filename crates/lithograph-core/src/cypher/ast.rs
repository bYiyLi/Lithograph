use super::error::Span;

/// Top-level execution prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Execute,
    Explain,
    Profile,
}

/// Lithograph-owned Cypher AST. No external parser node escapes into this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAst {
    pub cypher_version: u8,
    pub execution_mode: ExecutionMode,
    pub query_options: Vec<QueryOption>,
    pub span: Span,
    pub root: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOption {
    pub name: String,
    pub value: QueryOptionValue,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOptionValue {
    Identifier(String),
    StringLiteral(String),
    IntegerLiteral(String),
    FloatLiteral(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstNode {
    pub kind: AstKind,
    pub span: Span,
    pub text: Option<String>,
    pub children: Vec<AstNode>,
}

impl AstNode {
    pub fn descendants(&self) -> impl Iterator<Item = &AstNode> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            stack.extend(node.children.iter().rev());
            Some(node)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AstKind {
    QueryBody,
    ConditionalQuery,
    ConditionalBranch(ConditionalBranchKind),
    ComposedQuery,
    SingleQuery,
    Connector(QueryConnector),
    Clause(ClauseKind),
    Pattern,
    PatternPart,
    QuantifiedPattern,
    RelationshipChain,
    PathAssignment,
    PathMode(PathModeKind),
    PathSelector(PathSelectorKind),
    PathCount,
    Quantifier(QuantifierKind),
    QuantifierLowerBound,
    QuantifierUpperBound,
    NodePattern,
    RelationshipPattern,
    RelationshipLeftArrow,
    RelationshipRightArrow,
    RelationshipDetail,
    RelationshipTypeExpression,
    MatchMode(MatchModeKind),
    VariableLength,
    ProjectionBody,
    SetQuantifier(SetQuantifierKind),
    GroupBy,
    GroupByAll,
    GroupByEmpty,
    OrderBy,
    OrderDirection(OrderDirectionKind),
    Where,
    Skip,
    Limit,
    ProjectionItem,
    StarProjection,
    CaseAlternative,
    LetBinding,
    MergeAction(MergeActionKind),
    SetItem,
    SetOperator(SetOperatorKind),
    AssignableProperty,
    LabelUpdate,
    RemoveItem,
    PropertyExpression,
    MapEntry,
    MapKey,
    ArgumentList,
    SubqueryScope,
    SubqueryImport,
    SubqueryScopeAll,
    TransactionSubclause,
    TransactionConcurrent,
    TransactionBatch,
    TransactionDisjoint(TransactionDisjointKind),
    TransactionError(TransactionErrorKind),
    TransactionRetryFallback(TransactionErrorKind),
    TransactionStatus,
    TransactionStatusBinding,
    YieldAll,
    LoadCsvHeaders,
    LoadCsvFieldTerminator,
    LoadCsvBinding,
    Search,
    Subquery(SubqueryKind),
    Expression(ExpressionKind),
    PatternVariable,
    RelationshipVariable,
    ProjectionAlias,
    YieldItem,
    YieldName,
    BindingVariable,
    PredicateVariable,
    ReductionAccumulator,
    ReductionVariable,
    Variable,
    Parameter,
    FunctionName,
    PropertyKey,
    LabelExpression,
    NameExpression(NameExpressionKind),
    LabelName,
    RelationshipTypeName,
    TypeName,
    VectorCoordinateTypeName,
    VectorDistanceMetric,
    NormalizationForm,
    TrimSpecification,
    TrimFromArguments,
    TypeExpression,
    TypeTerm,
    TypeParameters,
    TypeListSuffix,
    TypeNotNull,
    VectorDimension,
    TypePredicate,
    ConstraintRequirement,
    ConstraintKind(ConstraintKind),
    IndexKind(IndexKind),
    IndexName,
    IndexTarget,
    IndexTargetEach,
    IndexAdditionalProperties,
    ConstraintName,
    ExistenceModifier(ExistenceModifierKind),
    ShowTarget(ShowTargetKind),
    ShowConstraintFilter(ShowConstraintFilterKind),
    ShowAsGraph,
    GraphTypeOperation(GraphTypeOperationKind),
    GraphNodeType,
    GraphRelationshipType,
    GraphEndpoint,
    GraphImplies,
    GraphConstraint,
    GraphAlias,
    ComparisonSuffix,
    Subscript,
    GraphProperty,
    Literal(LiteralKind),
    Operator,
    Syntax,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    Call,
    Braced,
    Exists,
    Count,
    Collect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryConnector {
    Union,
    UnionAll,
    UnionDistinct,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalBranchKind {
    When,
    Else,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchModeKind {
    DifferentRelationships,
    RepeatableElements,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetQuantifierKind {
    All,
    Distinct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirectionKind {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeActionKind {
    Create,
    Match,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOperatorKind {
    Assign,
    AddAssign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameExpressionKind {
    Disjunction,
    Conjunction,
    Negation(usize),
    Atom,
    Dynamic,
    Wildcard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    Key,
    NodeKey,
    RelationshipKey,
    Unique,
    NodeUnique,
    RelationshipUnique,
    NotNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistenceModifierKind {
    IfNotExists,
    IfExists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowTargetKind {
    CurrentGraphType,
    Indexes,
    Constraints,
    Functions,
    Procedures,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowConstraintFilterKind {
    All,
    NodeUnique,
    RelationshipUnique,
    Unique,
    NodePropertyExistence,
    RelationshipPropertyExistence,
    PropertyExistence,
    NodeExistence,
    RelationshipExistence,
    Existence,
    NodePropertyType,
    RelationshipPropertyType,
    PropertyType,
    NodeKey,
    RelationshipKey,
    Key,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphTypeOperationKind {
    Set,
    Add,
    Alter,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathModeKind {
    Walk,
    Trail,
    Acyclic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSelectorKind {
    All,
    Any,
    AllShortest,
    AnyShortest,
    ShortestPaths,
    ShortestGroups,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    ZeroOrMore,
    OneOrMore,
    Fixed,
    Range,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionDisjointKind {
    None,
    Auto,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionErrorKind {
    Continue,
    Break,
    Fail,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Lookup,
    Range,
    Text,
    Point,
    FullText,
    Vector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClauseKind {
    Match,
    OptionalMatch,
    Filter,
    Return,
    With,
    Let,
    Unwind,
    For,
    Finish,
    Create,
    Insert,
    Merge,
    Set,
    Remove,
    Delete,
    DetachDelete,
    Foreach,
    Call,
    LoadCsv,
    CreateIndex,
    DropIndex,
    CreateConstraint,
    DropConstraint,
    Show,
    GraphType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionKind {
    Expression,
    Or,
    Xor,
    And,
    Not,
    Comparison,
    Additive,
    Multiplicative,
    Power,
    Unary,
    Postfix,
    FunctionCall,
    List,
    Map,
    Case,
    InterpolatedString,
    Parenthesized,
    Primary,
    Subquery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralKind {
    Null,
    Boolean,
    Integer,
    Float,
    String,
}
