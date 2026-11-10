//! The crate's error type.
//!
//! [`Error`](enum@Error)'s messages are written for people and never include the archive's URL,
//! which can contain a signed token. Match on [`Error::kind`] to react to a failure in code.

use thiserror::Error;

/// Everything that can go wrong. Each variant's message is shown by its `Display`.
#[derive(Error, Debug)]
pub enum Error {
    /// The request failed: it couldn't connect, timed out, or the connection broke. The URL is
    /// removed from the inner error.
    #[error("Network error: {}", network_cause(.0))]
    Http(reqwest::Error),

    /// The server answered with an error status, after any retries.
    #[error("The server returned HTTP {status}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
    },

    /// The server sent the whole file instead of the part asked for.
    #[error(
        "The server doesn't support partial downloads (HTTP Range requests), so this archive can't be browsed remotely"
    )]
    RangeNotSupported,

    /// The file's `ETag`, `Last-Modified` or length changed since the archive was opened.
    #[error("The archive changed on the server. Scan it again")]
    RemoteChanged,

    /// The server's response broke the HTTP rules for a partial download, such as the wrong
    /// range, a compressed body or more data than was asked for.
    #[error("The server sent an invalid response: {0}")]
    InvalidResponse(&'static str),

    /// The connection ended early, and retrying didn't get the rest.
    #[error("The connection ended before all the data arrived")]
    TruncatedResponse,

    /// No usable end of central directory record was found.
    #[error("This file isn't a ZIP archive, or the end of it is damaged")]
    NotAZip,

    /// The archive claims too many entries or too large a directory, or reading the directory
    /// fetched too much.
    #[error("This archive is too large to open safely: {0}")]
    LimitExceeded(&'static str),

    /// The archive couldn't be parsed.
    #[error("ZIP error: {0}")]
    Zip(String),

    /// No entry has that name or index.
    #[error("File not found: {0}")]
    FileNotFound(String),

    /// An entry's data doesn't match its CRC-32.
    #[error("{path} is corrupted: its checksum doesn't match")]
    CrcMismatch {
        /// The entry's name.
        path: String,
    },

    /// An entry's data isn't the size its directory record claims.
    #[error("{path} is corrupted: expected {expected} bytes but got {actual}")]
    SizeMismatch {
        /// The entry's name.
        path: String,
        /// The size the directory claims.
        expected: u64,
        /// The bytes read before the mismatch was found. Reading stops one byte past `expected`.
        actual: u64,
    },

    /// An entry looks crafted to cause harm, such as data overlapping another entry.
    #[error("{path} looks malicious: {reason}")]
    SuspiciousEntry {
        /// The entry's name.
        path: String,
        /// Why it was refused.
        reason: &'static str,
    },

    /// An entry uses a compression method that can't be extracted, such as LZMA.
    #[error("{path} can't be extracted: its compression method isn't supported ({method})")]
    UnsupportedCompression {
        /// The entry's name.
        path: String,
        /// The method's name.
        method: &'static str,
    },

    /// An entry is encrypted. It was refused after reading only its local header.
    #[error("{path} can't be extracted: it's encrypted")]
    Encrypted {
        /// The entry's name.
        path: String,
    },

    /// Reading or writing a local file failed.
    #[error("IO error: {0}")]
    Io(std::io::Error),

    /// A custom header's name or value is invalid, or it's one the client sets itself. Holds the
    /// header's name, never its value.
    #[error("Invalid header: {0}")]
    InvalidHeader(String),

    /// An entry's name can't be written safely as a path.
    #[error("Unsafe entry \"{entry}\": {reason}")]
    UnsafeEntry {
        /// The name, shortened for display.
        entry: String,
        /// Why it was refused.
        reason: &'static str,
    },

    /// The operation was cancelled.
    #[error("Cancelled")]
    Cancelled,
}

/// What sort of failure an [`Error`](enum@Error) is, for callers that react differently to each.
///
/// It serializes in camelCase, such as `"rangeNotSupported"`.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    /// The connection failed, or the server sent something unusable.
    Network,
    /// The server answered with an error status.
    HttpStatus,
    /// The server doesn't support Range requests, so the archive can't be read remotely.
    RangeNotSupported,
    /// The file changed on the server since the archive was opened. Open it again.
    RemoteChanged,
    /// The file isn't a ZIP archive.
    NotAZip,
    /// The archive is too large to open safely.
    LimitExceeded,
    /// The data didn't match its size or checksum, or couldn't be parsed.
    Corrupt,
    /// The entry looks crafted to cause harm.
    Suspicious,
    /// The entry is encrypted, or its compression method isn't supported.
    Unsupported,
    /// No entry has that name.
    NotFound,
    /// The name can't be written safely.
    Unsafe,
    /// A custom header is invalid.
    InvalidHeader,
    /// Reading or writing a local file failed.
    Io,
    /// The operation was cancelled.
    Cancelled,
}

impl Error {
    /// The sort of failure this is.
    ///
    /// # Examples
    ///
    /// ```
    /// use lazy_zip_core::{Error, ErrorKind};
    ///
    /// let error = Error::CrcMismatch { path: "data/a.csv".to_string() };
    /// assert_eq!(error.kind(), ErrorKind::Corrupt);
    /// assert_eq!(error.to_string(), "data/a.csv is corrupted: its checksum doesn't match");
    ///
    /// assert_eq!(Error::RemoteChanged.kind(), ErrorKind::RemoteChanged);
    /// ```
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Http(_) | Error::InvalidResponse(_) | Error::TruncatedResponse => {
                ErrorKind::Network
            }
            Error::HttpStatus { .. } => ErrorKind::HttpStatus,
            Error::RangeNotSupported => ErrorKind::RangeNotSupported,
            Error::RemoteChanged => ErrorKind::RemoteChanged,
            Error::NotAZip => ErrorKind::NotAZip,
            Error::LimitExceeded(_) => ErrorKind::LimitExceeded,
            Error::Zip(_) | Error::CrcMismatch { .. } | Error::SizeMismatch { .. } => {
                ErrorKind::Corrupt
            }
            Error::SuspiciousEntry { .. } => ErrorKind::Suspicious,
            Error::UnsupportedCompression { .. } | Error::Encrypted { .. } => {
                ErrorKind::Unsupported
            }
            Error::FileNotFound(_) => ErrorKind::NotFound,
            Error::UnsafeEntry { .. } => ErrorKind::Unsafe,
            Error::InvalidHeader(_) => ErrorKind::InvalidHeader,
            Error::Io(_) => ErrorKind::Io,
            Error::Cancelled => ErrorKind::Cancelled,
        }
    }
}

/// The most specific reason available, such as "Connection refused", since a request error's own
/// message only says that the request failed.
fn network_cause(error: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

impl From<reqwest::Error> for Error {
    /// Drops the URL from the error, since it can contain a signed token.
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e.without_url())
    }
}

/// Recovers an error that passed through an I/O interface intact.
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        if e.get_ref().is_some_and(|inner| inner.is::<Error>()) {
            if let Some(inner) = e.into_inner() {
                if let Ok(error) = inner.downcast::<Error>() {
                    return *error;
                }
            }
            unreachable!("the inner error was checked above");
        }
        Error::Io(e)
    }
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            other => std::io::Error::other(other),
        }
    }
}

impl From<async_zip::error::ZipError> for Error {
    fn from(e: async_zip::error::ZipError) -> Self {
        use async_zip::error::ZipError;
        match e {
            ZipError::UpstreamReadError(e) => e.into(),
            ZipError::UnableToLocateEOCDR => Error::NotAZip,
            other => Error::Zip(other.to_string()),
        }
    }
}

/// A [`std::result::Result`] with this crate's [`Error`](enum@Error).
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = Error::HttpStatus { status: 404 };
        assert_eq!(err.to_string(), "The server returned HTTP 404");

        let err = Error::FileNotFound("foo.txt".to_string());
        assert_eq!(err.to_string(), "File not found: foo.txt");
    }

    #[test]
    fn typed_errors_survive_an_io_error_round_trip() {
        let io: std::io::Error = Error::RangeNotSupported.into();
        assert!(matches!(Error::from(io), Error::RangeNotSupported));

        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let round_trip: std::io::Error = Error::from(io).into();
        assert_eq!(round_trip.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn zip_errors_recover_the_upstream_error() {
        let upstream = async_zip::error::ZipError::UpstreamReadError(Error::RemoteChanged.into());
        assert!(matches!(Error::from(upstream), Error::RemoteChanged));
        assert!(matches!(
            Error::from(async_zip::error::ZipError::UnableToLocateEOCDR),
            Error::NotAZip
        ));
    }
}
