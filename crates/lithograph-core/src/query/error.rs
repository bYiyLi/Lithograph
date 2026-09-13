use std::fmt;

use crate::cypher::{FrontendError, FrontendErrorKind, ValueError};
use crate::storage::StorageError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryErrorKind {
    Parse,
    Semantic,
    Type,
    Schema,
    Constraint,
    InvalidArgument,
    VersionNotFound,
    BranchNotFound,
    TagNotFound,
    GraphViewViolation,
    BranchHeadMoved,
    MergeSessionNotFound,
    MergeSessionChanged,
    MergeConflict,
    ReadOnlyAdapter,
    ReadOnlySnapshot,
    TransactionBoundaryRequired,
    Storage,
    Resource,
    Io,
    Interrupted,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub kind: QueryErrorKind,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub sqlite_code: Option<i32>,
}

impl QueryError {
    pub fn new(kind: QueryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            line: None,
            column: None,
            sqlite_code: None,
        }
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::InvalidArgument, message)
    }

    pub fn semantic(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Semantic, message)
    }

    pub fn constraint(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Constraint, message)
    }

    pub fn graph_view_violation(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::GraphViewViolation, message)
    }

    pub fn read_only_adapter(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::ReadOnlyAdapter, message)
    }

    pub fn read_only_snapshot(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::ReadOnlySnapshot, message)
    }

    pub fn transaction_boundary_required(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::TransactionBoundaryRequired, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Internal, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(QueryErrorKind::Io, message)
    }

    pub fn interrupted() -> Self {
        let mut error = Self::new(QueryErrorKind::Interrupted, "query interrupted");
        error.sqlite_code = Some(rusqlite::ffi::SQLITE_INTERRUPT);
        error
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QueryError {}

impl From<FrontendError> for QueryError {
    fn from(error: FrontendError) -> Self {
        let kind = match error.kind {
            FrontendErrorKind::Parse => QueryErrorKind::Parse,
            FrontendErrorKind::Semantic => QueryErrorKind::Semantic,
            FrontendErrorKind::Type => QueryErrorKind::Type,
            FrontendErrorKind::Schema => QueryErrorKind::Schema,
            FrontendErrorKind::InvalidArgument => QueryErrorKind::InvalidArgument,
        };
        Self {
            kind,
            message: error.message,
            line: Some(error.line),
            column: Some(error.column),
            sqlite_code: None,
        }
    }
}

impl From<ValueError> for QueryError {
    fn from(error: ValueError) -> Self {
        Self::new(QueryErrorKind::Type, error.message)
    }
}

impl From<StorageError> for QueryError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Sqlite(error) => Self::from(error),
            StorageError::BranchHeadMoved => Self::new(
                QueryErrorKind::BranchHeadMoved,
                "branch head moved during query commit",
            ),
            error => Self::new(QueryErrorKind::Storage, error.to_string()),
        }
    }
}

pub type QueryResult<T> = Result<T, QueryError>;

impl From<rusqlite::Error> for QueryError {
    fn from(error: rusqlite::Error) -> Self {
        let sqlite_code = match &error {
            rusqlite::Error::SqliteFailure(code, _) => Some(code.extended_code & 0xff),
            _ => None,
        };
        let kind = match sqlite_code {
            Some(code) if code == rusqlite::ffi::SQLITE_INTERRUPT => QueryErrorKind::Interrupted,
            Some(
                rusqlite::ffi::SQLITE_IOERR
                | rusqlite::ffi::SQLITE_CANTOPEN
                | rusqlite::ffi::SQLITE_READONLY,
            ) => QueryErrorKind::Io,
            Some(
                rusqlite::ffi::SQLITE_NOMEM
                | rusqlite::ffi::SQLITE_FULL
                | rusqlite::ffi::SQLITE_TOOBIG,
            ) => QueryErrorKind::Resource,
            _ => QueryErrorKind::Storage,
        };
        let message = match &error {
            rusqlite::Error::SqliteFailure(_, Some(message)) => message.clone(),
            rusqlite::Error::SqliteFailure(code, None) => {
                format!("SQLite error code {}", code.extended_code)
            }
            _ => error.to_string(),
        };
        let mut mapped = Self::new(kind, message);
        mapped.sqlite_code = sqlite_code;
        mapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::Span;

    #[test]
    fn constructors_display_and_value_error_mapping_are_stable() {
        let invalid = QueryError::invalid_argument("invalid");
        assert_eq!(invalid.kind, QueryErrorKind::InvalidArgument);
        assert_eq!(invalid.to_string(), "invalid");
        assert_eq!(
            QueryError::semantic("semantic").kind,
            QueryErrorKind::Semantic
        );
        assert_eq!(
            QueryError::internal("internal").kind,
            QueryErrorKind::Internal
        );
        assert_eq!(QueryError::io("io").kind, QueryErrorKind::Io);

        let value = QueryError::from(ValueError::new("bad value"));
        assert_eq!(value.kind, QueryErrorKind::Type);
        assert_eq!(value.message, "bad value");
        assert_eq!(value.sqlite_code, None);
    }

    #[test]
    fn frontend_error_mapping_preserves_category_and_location() {
        for (source, expected) in [
            (FrontendErrorKind::Parse, QueryErrorKind::Parse),
            (FrontendErrorKind::Semantic, QueryErrorKind::Semantic),
            (FrontendErrorKind::Type, QueryErrorKind::Type),
            (FrontendErrorKind::Schema, QueryErrorKind::Schema),
            (
                FrontendErrorKind::InvalidArgument,
                QueryErrorKind::InvalidArgument,
            ),
        ] {
            let mapped = QueryError::from(FrontendError {
                kind: source,
                message: "frontend".to_owned(),
                span: Span { start: 4, end: 8 },
                line: 3,
                column: 7,
            });
            assert_eq!(mapped.kind, expected);
            assert_eq!(mapped.message, "frontend");
            assert_eq!(mapped.line, Some(3));
            assert_eq!(mapped.column, Some(7));
            assert_eq!(mapped.sqlite_code, None);
        }
    }

    #[test]
    fn storage_and_sqlite_error_mapping_preserves_primary_code() {
        let corrupt = QueryError::from(StorageError::Corrupt("broken".to_owned()));
        assert_eq!(corrupt.kind, QueryErrorKind::Storage);
        assert!(corrupt.message.contains("broken"));
        assert_eq!(corrupt.sqlite_code, None);

        for (code, expected) in [
            (rusqlite::ffi::SQLITE_INTERRUPT, QueryErrorKind::Interrupted),
            (rusqlite::ffi::SQLITE_NOMEM, QueryErrorKind::Resource),
            (rusqlite::ffi::SQLITE_FULL, QueryErrorKind::Resource),
            (rusqlite::ffi::SQLITE_TOOBIG, QueryErrorKind::Resource),
            (rusqlite::ffi::SQLITE_IOERR, QueryErrorKind::Io),
            (rusqlite::ffi::SQLITE_CANTOPEN, QueryErrorKind::Io),
            (rusqlite::ffi::SQLITE_READONLY, QueryErrorKind::Io),
            (rusqlite::ffi::SQLITE_BUSY, QueryErrorKind::Storage),
        ] {
            let sqlite = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            let mapped = QueryError::from(StorageError::Sqlite(sqlite));
            assert_eq!(mapped.kind, expected);
            assert_eq!(mapped.sqlite_code, Some(code));
        }

        let non_sqlite = QueryError::from(rusqlite::Error::InvalidQuery);
        assert_eq!(non_sqlite.kind, QueryErrorKind::Storage);
        assert_eq!(non_sqlite.sqlite_code, None);
    }
}
