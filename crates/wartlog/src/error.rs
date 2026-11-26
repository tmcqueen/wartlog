use wartlog_store::StoreError;

/// What went wrong, for callers that act on it (the C ABI maps each kind to a status code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// another process holds the store lock
    Locked,
    /// private mode on a store wartlogd owns
    DaemonStore,
    /// wartlogd (or daemon mode) on a store the library created
    ExternalStore,
    /// shared mode, but no running wartlogd serves this store
    NoDaemon,
    /// no store at that path (exclusive mode, or reading a path that does not exist)
    NotFound,
    /// the store or a segment is newer than this library reads
    NewerFormat,
    /// the KQL text is wrong; `Error::span` points into it
    Query,
    /// an argument the API cannot use (bad UTF-8, a closed writer, ...)
    InvalidArgument,
    /// anything else: I/O, damage, the daemon refusing a request
    Other,
}

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    span: Option<(usize, usize)>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error { kind, message: message.into(), span: None }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// For `ErrorKind::Query`: the byte range of the KQL text the error is about.
    pub fn span(&self) -> Option<(usize, usize)> {
        self.span
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Error {
        let kind = match StoreError::find(&e) {
            Some(StoreError::Locked(_)) => ErrorKind::Locked,
            Some(StoreError::DaemonStore(_)) => ErrorKind::DaemonStore,
            Some(StoreError::ExternalStore(_)) => ErrorKind::ExternalStore,
            Some(StoreError::NotFound(_)) => ErrorKind::NotFound,
            Some(StoreError::NewerFormat(_)) => ErrorKind::NewerFormat,
            None => ErrorKind::Other,
        };
        Error::new(kind, format!("{e:#}"))
    }
}

impl From<wartlog_query::QueryError> for Error {
    fn from(e: wartlog_query::QueryError) -> Error {
        let span = (e.end > e.start).then_some((e.start, e.end));
        Error { kind: ErrorKind::Query, message: e.message, span }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(ErrorKind::Other, e.to_string())
    }
}
