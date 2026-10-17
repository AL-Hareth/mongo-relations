use bson::Bson;

/// Broad error classes, used by the server to pick gRPC / HTTP status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request is malformed or violates the schema.
    InvalidArgument,
    /// The request is well-formed but conflicts with stored data
    /// (FK violation, restrict, unique index).
    FailedPrecondition,
    /// A concurrent change won; retrying may succeed.
    Aborted,
    NotFound,
    /// MongoDB is unreachable or timed out.
    Unavailable,
    Internal,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] mr_core::Error),

    #[error("{}", describe_mongo(.0))]
    Mongo(#[from] mongodb::error::Error),

    #[error(
        "foreign key violation: `{collection}.{field}` references missing `{target}` key(s) {}",
        fmt_keys(.missing)
    )]
    ForeignKey {
        collection: String,
        field: String,
        target: String,
        missing: Vec<Bson>,
    },

    #[error(
        "cannot {action} `{collection}` document(s): still referenced by `{referenced_by}` (on_delete: restrict)"
    )]
    Restrict {
        action: &'static str,
        collection: String,
        referenced_by: String,
    },

    #[error("cascade exceeded the maximum depth of {0}")]
    CascadeTooDeep(usize),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("schema conflict: {0}")]
    SchemaConflict(String),

    #[error("transaction aborted after {attempts} attempts: {last}")]
    TooManyRetries { attempts: u32, last: Box<Error> },

    #[error("bson: {0}")]
    Bson(String),

    #[error("internal error: {0}")]
    Internal(String),
}

fn fmt_keys(keys: &[Bson]) -> String {
    const SHOW: usize = 5;
    let mut s: Vec<String> = keys.iter().take(SHOW).map(ToString::to_string).collect();
    if keys.len() > SHOW {
        s.push(format!("... ({} more)", keys.len() - SHOW));
    }
    format!("[{}]", s.join(", "))
}

impl From<bson::error::Error> for Error {
    fn from(e: bson::error::Error) -> Self {
        Error::Bson(e.to_string())
    }
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        use mongodb::error::ErrorKind as MK;
        match self {
            Error::Core(_) => ErrorKind::InvalidArgument,
            Error::ForeignKey { .. }
            | Error::Restrict { .. }
            | Error::CascadeTooDeep(_)
            | Error::Unsupported(_) => ErrorKind::FailedPrecondition,
            Error::SchemaConflict(_) | Error::TooManyRetries { .. } => ErrorKind::Aborted,
            Error::Bson(_) => ErrorKind::InvalidArgument,
            Error::Internal(_) => ErrorKind::Internal,
            Error::Mongo(e) => {
                if self.is_transient() {
                    return ErrorKind::Aborted;
                }
                match e.kind.as_ref() {
                    MK::ServerSelection { .. } | MK::Io(_) | MK::ConnectionPoolCleared { .. } => {
                        ErrorKind::Unavailable
                    }
                    MK::Write(_) | MK::InsertMany(_) | MK::Command(_) => match mongo_code(e) {
                        // Duplicate key, document validation failure.
                        Some(11000) | Some(121) => ErrorKind::FailedPrecondition,
                        // Write conflict.
                        Some(112) => ErrorKind::Aborted,
                        _ => ErrorKind::Internal,
                    },
                    _ => ErrorKind::Internal,
                }
            }
        }
    }

    /// Whether retrying the whole transaction may succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Mongo(e) => {
                e.contains_label(mongodb::error::TRANSIENT_TRANSACTION_ERROR)
                    || mongo_code(e) == Some(112)
            }
            _ => false,
        }
    }

    pub(crate) fn is_unknown_commit(&self) -> bool {
        matches!(self, Error::Mongo(e) if e.contains_label(mongodb::error::UNKNOWN_TRANSACTION_COMMIT_RESULT))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The server error code carried by a driver error, including write errors.
pub(crate) fn mongo_code(e: &mongodb::error::Error) -> Option<i32> {
    use mongodb::error::{ErrorKind as MK, WriteFailure};
    match e.kind.as_ref() {
        MK::Command(c) => Some(c.code),
        MK::Write(WriteFailure::WriteError(w)) => Some(w.code),
        MK::Write(WriteFailure::WriteConcernError(w)) => Some(w.code),
        MK::InsertMany(im) => im
            .write_errors
            .as_ref()
            .and_then(|w| w.first().map(|w| w.code))
            .or_else(|| im.write_concern_error.as_ref().map(|w| w.code)),
        _ => None,
    }
}

/// The server's own message for a driver error (the driver's `Display`
/// includes internal debug structures).
fn mongo_message(e: &mongodb::error::Error) -> String {
    use mongodb::error::{ErrorKind as MK, WriteFailure};
    match e.kind.as_ref() {
        MK::Command(c) => c.message.clone(),
        MK::Write(WriteFailure::WriteError(w)) => w.message.clone(),
        MK::Write(WriteFailure::WriteConcernError(w)) => w.message.clone(),
        MK::InsertMany(im) => im
            .write_errors
            .as_ref()
            .and_then(|w| w.first().map(|w| w.message.clone()))
            .or_else(|| im.write_concern_error.as_ref().map(|w| w.message.clone()))
            .unwrap_or_else(|| e.to_string()),
        _ => e.to_string(),
    }
}

/// A readable description of a driver error. Duplicate-key errors
/// (`E11000 ... collection: db.coll index: name dup key: { f: v }`) become
/// a unique-constraint message.
fn describe_mongo(e: &mongodb::error::Error) -> String {
    let msg = mongo_message(e);
    if mongo_code(e) == Some(11000) {
        let after = |marker: &str| msg.split_once(marker).map(|(_, rest)| rest.trim());
        let collection = after("collection: ")
            .and_then(|r| r.split_whitespace().next())
            .map(|c| c.split_once('.').map_or(c, |(_, coll)| coll));
        let index = after(" index: ").and_then(|r| r.split_whitespace().next());
        if let (Some(c), Some(i), Some(key)) = (collection, index, after("dup key: ")) {
            return format!(
                "unique constraint violated: `{c}` already has a document with {key} (index `{i}`)"
            );
        }
        return format!("unique constraint violated: {msg}");
    }
    format!("mongodb: {msg}")
}
