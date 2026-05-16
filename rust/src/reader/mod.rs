//! Async reader for JRIF-indexed payloads.

mod cursor;
mod iter;
mod source;

#[cfg(feature = "tokio")]
mod tokio_fs;

use crate::document::{Document, JRIF_V0_TAG};
use crate::error::{Error, Result};

pub use cursor::Cursor;
pub use iter::{ArrayIter, ObjectIter};
pub use source::{BufferReader, Source};

#[cfg(feature = "tokio")]
pub use tokio_fs::FileSource;

/// Reader paired with a byte source.
///
/// Parses the JRIF sidecar at construction. All payload I/O is deferred until
/// the first cursor navigation.
pub struct Index<F> {
    pub(crate) doc: Document,
    source: F,
}

impl<F: Source> Index<F> {
    /// Convenience over the builder.
    ///
    /// # Errors
    ///
    /// See [`IndexBuilder::open`].
    pub async fn open(jrif_bytes: &[u8], source: F) -> Result<Self> {
        IndexBuilder::new().open(jrif_bytes, source).await
    }

    /// Borrow the underlying source (useful for cache introspection on
    /// [`BufferReader`]).
    pub const fn source(&self) -> &F {
        &self.source
    }

    /// Cursor at the document root.
    pub fn root(&self) -> Cursor<'_, F> {
        Cursor::root(self)
    }
}

/// Builder for [`Index`].
#[derive(Clone, Debug, Default)]
pub struct IndexBuilder {}

impl IndexBuilder {
    /// New builder with default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open an [`Index`] from a JRIF sidecar and a payload source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDocument`] for unparseable JRIF or an
    /// unsupported `jrif` version tag.
    #[expect(clippy::unused_async)] // public API; kept async for future I/O
    pub async fn open<F: Source>(self, jrif_bytes: &[u8], source: F) -> Result<Index<F>> {
        let doc: Document = serde_json::from_slice(jrif_bytes)
            .map_err(|e| Error::InvalidDocument(format!("parse jrif: {e}")))?;
        if &*doc.jrif != JRIF_V0_TAG {
            return Err(Error::InvalidDocument(format!(
                "unsupported jrif version: {}",
                doc.jrif
            )));
        }
        Ok(Index { doc, source })
    }
}

impl<'a, F: Source> Cursor<'a, F> {
    /// Resolve array length, then return a sync iterator over elements.
    /// Consumes the cursor; clone first if you need to keep navigating from
    /// the same position.
    ///
    /// # Errors
    ///
    /// See [`Cursor::len`].
    #[expect(clippy::iter_not_returning_iterator)] // returns Result<Iterator> by design
    pub async fn iter(self) -> Result<ArrayIter<'a, F>> {
        let len = self.len().await?;
        Ok(ArrayIter::new(self, len))
    }

    /// Resolve object keys, then return a sync iterator over
    /// `(field_name, cursor)` pairs in source order. Consumes the cursor.
    ///
    /// # Errors
    ///
    /// Surfaces fetch / parse / type-mismatch errors from reading the
    /// cursor's bytes; see [`Cursor::value`].
    pub async fn entries(self) -> Result<ObjectIter<'a, F>> {
        let keys = self.collect_object_keys().await?;
        Ok(ObjectIter {
            root: self,
            keys: keys.into_iter(),
        })
    }
}
