//! [`ArrayIter`] and [`ObjectIter`] — sync iteration over container cursors.
//!
//! `iter().await?` (arrays) and `entries().await?` (objects) each resolve
//! their container shape once, possibly fetching+parsing, then yield child
//! cursors infallibly via std `Iterator`.

use crate::nav::{ArrayWalker, Frame};
use crate::reader::Cursor;
use crate::reader::source::Source;

/// Iterator returned by [`Cursor::iter`](crate::Cursor::iter).
///
/// When the array is described by chunks in the index, items are emitted via
/// an [`ArrayWalker`] for O(1) amortized work per step (no per-item chunk
/// rescans). When the iterator's root cursor has pending segments or isn't
/// chunk-described (e.g. an inline array), iteration falls back to per-ordinal
/// `Cursor::index`, which is correct but asymptotically worse.
pub struct ArrayIter<'a, F> {
    pub(crate) root: Cursor<'a, F>,
    pub(crate) len: u64,
    pub(crate) next: u64,
    /// Walker over the root's array chunks, when available. We hold it as
    /// `Option<...>` so the slow-path (no chunk frame, e.g. inline arrays) is
    /// also expressible.
    pub(crate) walker: Option<ArrayWalker<'a>>,
}

impl<'a, F: Source> Iterator for ArrayIter<'a, F> {
    type Item = Cursor<'a, F>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.len {
            return None;
        }
        if let Some(w) = self.walker.as_mut()
            && let Some((hit, ord)) = w.next()
        {
            self.next += 1;
            return Some(self.root.clone().apply_array_hit(hit, ord));
        }
        let ord = self.next;
        self.next += 1;
        Some(self.root.clone().index(ord))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::try_from(self.len - self.next).expect("array length fits in usize");
        (remaining, Some(remaining))
    }
}

impl<F: Source> ExactSizeIterator for ArrayIter<'_, F> {}
impl<F: Source> std::iter::FusedIterator for ArrayIter<'_, F> {}

impl<'a, F> ArrayIter<'a, F> {
    /// Build a new iterator. Used by [`Cursor::iter`]; selects the walker
    /// fast path when the root cursor exposes a chunk-described array frame.
    pub(crate) fn new(root: Cursor<'a, F>, len: u64) -> Self {
        let walker = if root.pending.is_empty() {
            match root.frame {
                Frame::Array(chunks) => Some(ArrayWalker::new(chunks)),
                _ => None,
            }
        } else {
            None
        };
        Self {
            root,
            len,
            next: 0,
            walker,
        }
    }
}

/// Iterator returned by [`Cursor::entries`](crate::Cursor::entries).
/// Yields `(field_name, cursor)` pairs in source order.
pub struct ObjectIter<'a, F> {
    pub(crate) root: Cursor<'a, F>,
    pub(crate) keys: std::vec::IntoIter<Box<str>>,
}

impl<'a, F: Source> Iterator for ObjectIter<'a, F> {
    type Item = (Box<str>, Cursor<'a, F>);

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.keys.next()?;
        let cursor = self.root.clone().get(&key);
        Some((key, cursor))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.keys.len();
        (r, Some(r))
    }
}

impl<F: Source> ExactSizeIterator for ObjectIter<'_, F> {}
impl<F: Source> std::iter::FusedIterator for ObjectIter<'_, F> {}
