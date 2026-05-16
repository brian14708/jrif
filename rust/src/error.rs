//! Unified error type for the crate.

use std::fmt;
use std::io;

use serde_json::Value as JsonValue;
use thiserror::Error;

use crate::document::JsonType;

/// Path segment used in [`crate::Error`] path context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Segment {
    Field(Box<str>),
    Index(u64),
}

impl Segment {
    pub fn field(s: impl Into<Box<str>>) -> Self {
        Self::Field(s.into())
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(n) => write!(f, ".{n}"),
            Self::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// Path from the document root to the cursor that produced an error.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Path(pub Box<[Segment]>);

impl Path {
    #[must_use]
    pub fn empty() -> Self {
        Self(Box::new([]))
    }

    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.0
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("$")?;
        for seg in &self.0 {
            write!(f, "{seg}")?;
        }
        Ok(())
    }
}

impl<I: IntoIterator<Item = Segment>> From<I> for Path {
    fn from(iter: I) -> Self {
        Self(iter.into_iter().collect::<Vec<_>>().into_boxed_slice())
    }
}

/// Unified error type for all JRIF operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The JRIF sidecar document is malformed or uses an unsupported schema.
    #[error("invalid JRIF document: {0}")]
    InvalidDocument(String),

    /// A recorded range is out of bounds or violates the spec's structural rules.
    #[error("invalid JRIF document at {path}: {reason}")]
    InvalidPayload { path: Path, reason: String },

    /// An object field is missing, or an array index is out of bounds.
    #[error("not found at {path}")]
    NotFound { path: Path },

    /// Navigated into the wrong JSON type (e.g. `.get()` on an array).
    #[error("type mismatch at {path}: expected {expected}, got {got}")]
    TypeMismatch {
        path: Path,
        expected: JsonType,
        got: JsonType,
    },

    /// Underlying [`Source`](crate::Source) failed.
    #[error("read failed at {path}: {source}")]
    Read {
        path: Path,
        #[source]
        source: io::Error,
    },

    /// Parsing the fetched JSON bytes failed.
    #[error("parse error at {path}: {source}")]
    Parse {
        path: Path,
        #[source]
        source: serde_json::Error,
    },
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) const fn read(path: Path, source: io::Error) -> Self {
        Self::Read { path, source }
    }

    pub(crate) const fn parse(path: Path, source: serde_json::Error) -> Self {
        Self::Parse { path, source }
    }

    pub(crate) fn type_mismatch(path: Path, expected: JsonType, got: &JsonValue) -> Self {
        Self::TypeMismatch {
            path,
            expected,
            got: JsonType::from(got),
        }
    }
}
