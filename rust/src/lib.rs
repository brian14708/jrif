//! JRIF — JSON Range Index Format (v0) reader and writer.
//!
//! Build a sidecar `.jrif` document mapping JSON paths to byte ranges
//! (with [`Indexer`]), then navigate an indexed payload by path
//! (with [`Index`] and [`Cursor`]).
//!
//! See `docs/spec.md` for the normative format specification.
//!
//! # Example
//!
//! ```no_run
//! # async fn example() -> Result<(), jrif::Error> {
//! use bytes::Bytes;
//! use jrif::{Index, Indexer};
//!
//! let payload: Bytes = std::fs::read("data.json").unwrap().into();
//! let jrif = Indexer::new().build(&payload)?;
//!
//! let index = Index::open(&jrif, payload).await?;
//! let name: String = index.root()
//!     .get("records")
//!     .index(0)
//!     .get("name")
//!     .deserialize().await?;
//! # let _ = name;
//! # Ok(()) }
//! ```

#[cfg_attr(not(feature = "reader"), expect(dead_code))]
mod document;
mod error;

#[cfg(feature = "reader")]
mod nav;

#[cfg(feature = "writer")]
mod parser;
#[cfg(feature = "writer")]
mod writer;

#[cfg(feature = "reader")]
mod reader;

pub use document::JsonType;
pub use error::{Error, Path, Result, Segment};

#[cfg(feature = "reader")]
pub use reader::{ArrayIter, BufferReader, Cursor, Index, IndexBuilder, ObjectIter, Source};

#[cfg(all(feature = "reader", feature = "tokio"))]
pub use reader::FileSource;

#[cfg(feature = "writer")]
pub use writer::Indexer;
