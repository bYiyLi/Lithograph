use std::fmt;

/// Frontend failure category before it is mapped to a public adapter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendErrorKind {
    Parse,
    Semantic,
    Type,
    Schema,
    InvalidArgument,
}

/// Byte span in the original Cypher query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Stable parser/semantic/type/schema validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendError {
    pub kind: FrontendErrorKind,
    pub message: String,
    pub span: Span,
    pub line: u32,
    pub column: u32,
}

impl FrontendError {
    pub(crate) fn new(
        kind: FrontendErrorKind,
        message: impl Into<String>,
        span: Span,
        source: &str,
    ) -> Self {
        let (line, column) = line_column(source, span.start);
        Self {
            kind,
            message: message.into(),
            span,
            line,
            column,
        }
    }
}

impl fmt::Display for FrontendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}:{}",
            self.message, self.line, self.column
        )
    }
}

impl std::error::Error for FrontendError {}

pub(crate) fn line_column(source: &str, offset: usize) -> (u32, u32) {
    let bounded = offset.min(source.len());
    let prefix = &source[..bounded];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let column = source[line_start..bounded].chars().count() as u32 + 1;
    (line, column)
}
