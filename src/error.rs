use std::fmt;
use std::io;

/// Errors that can occur while operating on a [`crate::Db`].
#[derive(Debug)]
pub enum Error {
    /// An underlying I/O operation (file read/write/sync) failed.
    Io(io::Error),
    /// On-disk data (WAL record or SSTable record) could not be parsed.
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {}", e),
            Error::Corrupt(msg) => write!(f, "corrupt data: {}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Corrupt(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
