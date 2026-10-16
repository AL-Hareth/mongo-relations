use std::fmt;

/// A single validation failure on a document field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub message: String,
}

impl Violation {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`: {}", self.path, self.message)
    }
}

/// Errors produced by the pure (I/O-free) core.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("schema error: {0}")]
    Schema(String),

    #[error("unknown collection `{0}`")]
    UnknownCollection(String),

    #[error("unknown relation `{relation}` on collection `{collection}`")]
    UnknownRelation {
        collection: String,
        relation: String,
    },

    #[error("invalid query: {0}")]
    InvalidQuery(String),

    #[error("invalid write: {0}")]
    InvalidWrite(String),

    #[error("document validation failed: {}", join(.0))]
    Validation(Vec<Violation>),
}

fn join(v: &[Violation]) -> String {
    v.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

macro_rules! schema_err {
    ($($arg:tt)*) => { $crate::error::Error::Schema(format!($($arg)*)) };
}
macro_rules! query_err {
    ($($arg:tt)*) => { $crate::error::Error::InvalidQuery(format!($($arg)*)) };
}
#[allow(unused_macros)]
macro_rules! write_err {
    ($($arg:tt)*) => { $crate::error::Error::InvalidWrite(format!($($arg)*)) };
}
#[allow(unused_imports)]
pub(crate) use {query_err, schema_err, write_err};
