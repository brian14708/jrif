//! [`Cursor`] — navigates an indexed JSON payload by path with deferred I/O.
//!
//! Navigation methods (`.get`, `.index`) are synchronous and infallible: they
//! either advance through the chunk index in memory or push the segment onto a
//! pending list. All I/O and parsing happens in the leaf accessors
//! (`.bytes`, `.value`, `.deserialize`), which run any deferred work in one
//! go and surface the first error encountered.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;

use crate::document::{JsonType, Resolved, Value};
use crate::error::{Error, Path, Result, Segment};
use crate::nav::{self, ArrayHit, Frame, ObjectHitBorrow, Wrap, frame_of};
use crate::reader::Index;
use crate::reader::source::{Source, check_exact_len};

/// Async cursor over an indexed payload.
///
/// Built from [`Index::root`]; descended with sync `.get(&str)` / `.index(u64)`
/// methods (infallible); driven to I/O at the leaves via `.bytes/.value/...`.
pub struct Cursor<'a, F> {
    pub(crate) idx: &'a Index<F>,
    pub(crate) path: Vec<Segment>,
    /// Base resolution at this cursor position: either an inline value (no
    /// I/O needed) or a ranged value (fetch + parse + walk pending).
    pub(crate) base: Base<'a>,
    /// Live navigation frame at the current resolved position. `Frame::Done`
    /// means further descent must defer to a parse step.
    pub(crate) frame: Frame<'a>,
    /// Segments past the base that still need to be resolved by parsing.
    /// Empty when the cursor is fully resolved by the chunk index alone.
    pub(crate) pending: Vec<Segment>,
}

#[derive(Clone)]
pub enum Base<'a> {
    Inline(&'a JsonValue),
    Ranged {
        range: Range<u64>,
        /// How `range` bytes parse as a JSON value (None, [...], or {...}).
        wrap: Wrap,
    },
}

/// Outcome of one cursor descent step. Either we landed on a concrete child
/// `Value` (inline) or the chunk index pointed us at a multi-element range that
/// still needs one more lookup at parse time (items).
enum DescentHit<'a> {
    Inline {
        value: &'a Value,
    },
    Items {
        range: Range<u64>,
        wrap: Wrap,
        /// The pending segment to push for the still-unresolved final step
        /// inside the items range. For arrays this is the ordinal *within* the
        /// chunk; for objects it's just the field name.
        pending: Segment,
    },
}

impl<F> Clone for Cursor<'_, F> {
    fn clone(&self) -> Self {
        Self {
            idx: self.idx,
            path: self.path.clone(),
            base: self.base.clone(),
            frame: self.frame,
            pending: self.pending.clone(),
        }
    }
}

fn base_from_value(value: &Value) -> Base<'_> {
    match value.resolve() {
        Resolved::Inline(v) => Base::Inline(v),
        Resolved::Ranged { range, .. } => Base::Ranged {
            range,
            wrap: Wrap::None,
        },
    }
}

impl<'a, F: Source> Cursor<'a, F> {
    pub(crate) fn root(idx: &'a Index<F>) -> Self {
        let value = &idx.doc.root;
        Self {
            idx,
            path: Vec::new(),
            base: base_from_value(value),
            frame: frame_of(value),
            pending: Vec::new(),
        }
    }

    /// Descend to an object member by name.
    ///
    /// Infallible — does no I/O. If the chunk index can place `name` exactly,
    /// the cursor advances; otherwise the descent is recorded as pending and
    /// will be performed at the next leaf accessor.
    #[must_use]
    pub fn get(self, name: &str) -> Self {
        let hit = if self.can_descend_via_frame() {
            match self.frame {
                Frame::Object(chunks) => nav::find_object_match(
                    chunks,
                    name,
                    &self.idx.doc.keys,
                    self.idx.doc.key_index(name),
                )
                .map(|h| match h {
                    ObjectHitBorrow::Field { value } => DescentHit::Inline { value },
                    ObjectHitBorrow::Fields { range } => DescentHit::Items {
                        range,
                        wrap: Wrap::Object,
                        pending: Segment::field(name),
                    },
                }),
                _ => None,
            }
        } else {
            None
        };
        self.descend(Segment::field(name), hit)
    }

    /// Descend to an array item by ordinal.
    ///
    /// Infallible — does no I/O. Same deferred-resolution model as [`Self::get`].
    #[must_use]
    pub fn index(self, ordinal: u64) -> Self {
        let hit = if self.can_descend_via_frame() {
            match self.frame {
                Frame::Array(chunks) => nav::find_array_match(chunks, ordinal).map(|h| match h {
                    ArrayHit::Item { value } => DescentHit::Inline { value },
                    ArrayHit::Items {
                        range,
                        start_ordinal,
                    } => DescentHit::Items {
                        range,
                        wrap: Wrap::Array,
                        pending: Segment::Index(ordinal - start_ordinal),
                    },
                }),
                _ => None,
            }
        } else {
            None
        };
        self.descend(Segment::Index(ordinal), hit)
    }

    /// Apply a precomputed [`ArrayHit`] (from [`ArrayWalker`]) at `ordinal`
    /// without rescanning chunks. Equivalent to [`Self::index`] for a hit
    /// the walker has already located. Used by [`ArrayIter`] to keep
    /// per-step cost O(1).
    #[must_use]
    pub(crate) fn apply_array_hit(self, hit: ArrayHit<'a>, ordinal: u64) -> Self {
        let seg = Segment::Index(ordinal);
        let descent = match hit {
            ArrayHit::Item { value } => DescentHit::Inline { value },
            ArrayHit::Items {
                range,
                start_ordinal,
            } => DescentHit::Items {
                range,
                wrap: Wrap::Array,
                pending: Segment::Index(ordinal - start_ordinal),
            },
        };
        self.descend(seg, Some(descent))
    }

    const fn can_descend_via_frame(&self) -> bool {
        self.pending.is_empty() && !matches!(self.base, Base::Inline(_))
    }

    fn descend(mut self, seg: Segment, hit: Option<DescentHit<'a>>) -> Self {
        // Inline/Items hits don't push `seg` to pending — move it into `path`
        // and skip a Segment::Field's Box<str> clone.
        match hit {
            Some(DescentHit::Inline { value }) => {
                self.base = base_from_value(value);
                self.frame = frame_of(value);
                self.path.push(seg);
            }
            Some(DescentHit::Items {
                range,
                wrap,
                pending,
            }) => {
                self.base = Base::Ranged { range, wrap };
                self.frame = Frame::Done;
                self.path.push(seg);
                self.pending.push(pending);
            }
            None => {
                self.path.push(seg.clone());
                self.pending.push(seg);
            }
        }
        self
    }

    /// Exact byte range when the cursor is fully resolved by the chunk index
    /// (no pending segments and no fragment wrap). Returns `None` when the
    /// cursor's value is inline or otherwise not yet resolved.
    #[must_use]
    pub fn range(&self) -> Option<Range<u64>> {
        if !self.pending.is_empty() {
            return None;
        }
        match &self.base {
            Base::Ranged {
                range,
                wrap: Wrap::None,
                ..
            } => Some(range.clone()),
            _ => None,
        }
    }

    /// Best-effort JSON type from the chunk index, without any I/O. `None`
    /// when the type is unknown (deferred parse, or the underlying value is
    /// below the chunking threshold).
    #[must_use]
    pub fn json_type_hint(&self) -> Option<JsonType> {
        if !self.pending.is_empty() {
            return None;
        }
        if let Base::Inline(v) = &self.base {
            return Some(JsonType::from(*v));
        }
        match self.frame {
            Frame::Array(_) => Some(JsonType::Array),
            Frame::Object(_) => Some(JsonType::Object),
            Frame::Done => None,
        }
    }

    /// Fetch the cursor's bytes as valid JSON.
    ///
    /// Fast path (no pending, no wrap): returns the raw payload slice. For
    /// inline cursors the bytes are synthesized from the inline `Value` with
    /// no I/O. Slow path (pending segments): fetches the deepest chunk range
    /// and walks pending with a streaming reader that captures the target
    /// value's raw bytes — no `serde_json::Value` round-trip.
    ///
    /// # Errors
    ///
    /// Surfaces fetcher I/O errors, parse failures, and type mismatches
    /// encountered while walking pending segments.
    pub async fn bytes(&self) -> Result<Bytes> {
        if self.is_resolved() {
            return self.fetch_raw().await;
        }
        if let Base::Inline(v) = &self.base {
            let val = (*v).clone();
            let target = walk_pending(val, &self.pending, &self.path)?;
            return serde_json::to_vec(&target)
                .map(Bytes::from)
                .map_err(|e| Error::parse(self.path_box(), e));
        }
        let Base::Ranged { wrap, .. } = &self.base else {
            unreachable!("ranged base after inline guard")
        };
        let raw = self.fetch_raw().await?;
        // pending is non-empty here: descend's Items hit always seeds pending,
        // and `is_resolved` short-circuited the no-pending + Wrap::None case.
        let target = parse_wrapped(&raw, *wrap, |slice| -> Result<Vec<u8>> {
            Ok(stream_target_raw(slice, &self.pending, &self.path)?.into_owned())
        })?;
        Ok(Bytes::from(target))
    }

    /// Fetch + parse + walk-pending. Returns the JSON value at the cursor.
    ///
    /// # Errors
    ///
    /// Surfaces fetcher I/O errors, parse failures, and type mismatches or
    /// missing fields encountered while walking pending segments.
    pub async fn value(&self) -> Result<JsonValue> {
        self.parse_at_target::<JsonValue>().await
    }

    /// Decode the cursor's value directly into a user type via
    /// `serde::Deserialize`. When the cursor is fully resolved this
    /// deserializes from the raw payload bytes. When the cursor has pending
    /// segments, the bytes are streamed through pending to capture the target
    /// value's raw JSON, then `T` is deserialized from those bytes — avoiding
    /// a full `serde_json::Value` round-trip.
    ///
    /// # Errors
    ///
    /// Same as [`Self::value`], plus serde deserialization failures into `T`.
    pub async fn deserialize<T: DeserializeOwned>(&self) -> Result<T> {
        self.parse_at_target::<T>().await
    }

    /// Shared implementation for [`Self::value`] and [`Self::deserialize`]:
    /// fetches the cursor's base bytes, optionally wraps a chunk fragment,
    /// optionally walks pending to locate the target's raw bytes, then
    /// deserializes `T` from those bytes.
    async fn parse_at_target<T: DeserializeOwned>(&self) -> Result<T> {
        if let Base::Inline(v) = &self.base {
            if self.pending.is_empty() {
                return serde_json::from_value((*v).clone())
                    .map_err(|e| Error::parse(self.path_box(), e));
            }
            let val = (*v).clone();
            let target = walk_pending(val, &self.pending, &self.path)?;
            return serde_json::from_value(target).map_err(|e| Error::parse(self.path_box(), e));
        }
        let Base::Ranged { wrap, .. } = &self.base else {
            unreachable!("ranged base after inline guard")
        };
        let bytes = self.fetch_raw().await?;
        parse_wrapped(&bytes, *wrap, |slice| {
            if self.pending.is_empty() {
                return serde_json::from_slice(slice).map_err(|e| Error::parse(self.path_box(), e));
            }
            let target = stream_target_raw(slice, &self.pending, &self.path)?;
            serde_json::from_slice(&target).map_err(|e| Error::parse(self.path_box(), e))
        })
    }

    /// Resolve array length. Returns from the chunk index without I/O when
    /// possible; otherwise fetches and parses the cursor's range.
    ///
    /// # Errors
    ///
    /// Same as [`Self::value`] on the parse path, plus a type mismatch if the
    /// cursor's value is not an array.
    pub async fn len(&self) -> Result<u64> {
        if self.pending.is_empty() {
            if let Base::Inline(v) = &self.base {
                return match v {
                    JsonValue::Array(a) => Ok(a.len() as u64),
                    other => Err(Error::type_mismatch(
                        self.path_box(),
                        JsonType::Array,
                        other,
                    )),
                };
            }
            if let Frame::Array(chunks) = self.frame
                && let Some(n) = chunks.array_len()
            {
                return Ok(n);
            }
        }
        let v = self.value().await?;
        match v {
            JsonValue::Array(a) => Ok(a.len() as u64),
            other => Err(Error::type_mismatch(
                self.path_box(),
                JsonType::Array,
                &other,
            )),
        }
    }

    /// Resolve the list of object field names in source order. Per spec
    /// §Object Field Chunks, chunks MAY cover only a subset of an object's
    /// fields, so the chunk index can't be trusted to enumerate every key.
    /// Always fetches the cursor's bytes and walks them with a streaming
    /// reader that preserves source order.
    pub(crate) async fn collect_object_keys(&self) -> Result<Vec<Box<str>>> {
        if self.pending.is_empty() {
            if let Base::Inline(v) = &self.base {
                return match v {
                    JsonValue::Object(map) => Ok(map.keys().map(|k| k.as_str().into()).collect()),
                    other => Err(Error::type_mismatch(
                        self.path_box(),
                        JsonType::Object,
                        other,
                    )),
                };
            }
            // Type-check via chunk index without I/O when the frame says array.
            if matches!(self.frame, Frame::Array(_)) {
                return Err(Error::TypeMismatch {
                    path: self.path_box(),
                    expected: JsonType::Object,
                    got: JsonType::Array,
                });
            }
        }
        if let Base::Inline(v) = &self.base {
            // Inline + pending: walk in JsonValue (cheap, inline literals are
            // already parsed) and collect the target object's keys.
            let target = walk_pending((*v).clone(), &self.pending, &self.path)?;
            return match target {
                JsonValue::Object(map) => Ok(map.keys().map(|k| k.as_str().into()).collect()),
                other => Err(Error::type_mismatch(
                    self.path_box(),
                    JsonType::Object,
                    &other,
                )),
            };
        }
        let Base::Ranged { wrap, .. } = &self.base else {
            unreachable!("ranged base after inline guard")
        };
        let bytes = self.fetch_raw().await?;
        parse_wrapped(&bytes, *wrap, |slice| {
            let target = stream_target_raw(slice, &self.pending, &self.path)?;
            read_object_keys(&target, &self.path)
        })
    }

    /// True when the cursor exactly identifies a contiguous JSON value in the
    /// payload (no deferred parse needed) or an inline value (always
    /// resolvable without I/O).
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        self.pending.is_empty()
            && match &self.base {
                Base::Inline(_) => true,
                Base::Ranged { wrap, .. } => *wrap == Wrap::None,
            }
    }

    // ---------- internals ----------

    fn path_box(&self) -> Path {
        self.path.clone().into()
    }

    async fn fetch_raw(&self) -> Result<Bytes> {
        match &self.base {
            Base::Inline(v) => {
                let bytes = serde_json::to_vec(*v).map_err(|e| Error::parse(self.path_box(), e))?;
                Ok(Bytes::from(bytes))
            }
            Base::Ranged { range, .. } => {
                let offset = range.start;
                let len = usize::try_from(range.end - range.start).map_err(|_| {
                    Error::InvalidPayload {
                        path: self.path_box(),
                        reason: "range length exceeds usize".into(),
                    }
                })?;
                let bytes = self
                    .idx
                    .source()
                    .read_exact_at(offset, len)
                    .await
                    .map_err(|source| Error::read(self.path_box(), source))?;
                check_exact_len(offset, len, bytes)
                    .map_err(|source| Error::read(self.path_box(), source))
            }
        }
    }
}

/// Apply a chunk-fragment wrap to `bytes` if needed, then invoke `f` with a
/// slice of bracket-delimited JSON ready to feed `serde_json::from_slice`.
///
/// Uses a thread-local scratch `Vec` to avoid allocating per call when the
/// chunk fragment shape is `Wrap::Array` or `Wrap::Object` — only the bytes
/// need copying. `Wrap::None` skips the buffer entirely and passes `bytes`
/// through verbatim.
fn parse_wrapped<T, F>(bytes: &[u8], wrap: Wrap, f: F) -> Result<T>
where
    F: FnOnce(&[u8]) -> Result<T>,
{
    // Don't keep arbitrarily large fetches pinned per thread; release
    // capacity once the buffer outgrows its typical working set.
    const SCRATCH_RETAIN: usize = 64 * 1024;

    let (open, close) = match wrap {
        Wrap::None => return f(bytes),
        Wrap::Array => (b'[', b']'),
        Wrap::Object => (b'{', b'}'),
    };
    thread_local! {
        static WRAP_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    WRAP_SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        buf.reserve(bytes.len() + 2);
        buf.push(open);
        buf.extend_from_slice(bytes);
        buf.push(close);
        let out = f(&buf);
        if buf.capacity() > SCRATCH_RETAIN {
            buf.shrink_to(SCRATCH_RETAIN);
        }
        out
    })
}

/// Build a `Path` ending at `pending[step]` given the full path-so-far and
/// the pending segments still to walk. Used by both byte and value walks
/// when constructing error contexts mid-iteration.
fn pending_path_at(full_path: &[Segment], pending_len: usize, step: usize) -> Path {
    let prefix = full_path.len() - pending_len;
    full_path[..=prefix + step].to_vec().into()
}

/// Walk `pending` through JSON `bytes` and return the target value's raw
/// JSON bytes. With empty pending the result borrows from `bytes`; otherwise
/// each step captures a fresh owned `Vec<u8>`.
fn stream_target_raw<'b>(
    bytes: &'b [u8],
    pending: &[Segment],
    full_path: &[Segment],
) -> Result<Cow<'b, [u8]>> {
    use serde::de::DeserializeSeed;

    if pending.is_empty() {
        return Ok(Cow::Borrowed(bytes));
    }
    let path_at = |i: usize| pending_path_at(full_path, pending.len(), i);

    let mut current: Cow<'b, [u8]> = Cow::Borrowed(bytes);
    for (i, seg) in pending.iter().enumerate() {
        match seg {
            Segment::Field(name) => {
                let got = peek_json_type(&current);
                if got != JsonType::Object {
                    return Err(Error::TypeMismatch {
                        path: path_at(i),
                        expected: JsonType::Object,
                        got,
                    });
                }
                let mut de = serde_json::Deserializer::from_slice(&current);
                let found = FindField { target: name }
                    .deserialize(&mut de)
                    .map_err(|e| Error::parse(path_at(i), e))?;
                let raw = found.ok_or_else(|| Error::NotFound { path: path_at(i) })?;
                current = Cow::Owned(raw);
            }
            Segment::Index(idx) => {
                let got = peek_json_type(&current);
                if got != JsonType::Array {
                    return Err(Error::TypeMismatch {
                        path: path_at(i),
                        expected: JsonType::Array,
                        got,
                    });
                }
                let target =
                    usize::try_from(*idx).map_err(|_| Error::NotFound { path: path_at(i) })?;
                let mut de = serde_json::Deserializer::from_slice(&current);
                let found = FindIndex { target }
                    .deserialize(&mut de)
                    .map_err(|e| Error::parse(path_at(i), e))?;
                let raw = found.ok_or_else(|| Error::NotFound { path: path_at(i) })?;
                current = Cow::Owned(raw);
            }
        }
    }
    Ok(current)
}

/// Walk `pending` through an already-parsed `JsonValue` (used by the inline
/// + pending path, where we already have a materialized value in hand).
fn walk_pending(
    mut value: JsonValue,
    pending: &[Segment],
    full_path: &[Segment],
) -> Result<JsonValue> {
    let path_at = |i: usize| pending_path_at(full_path, pending.len(), i);
    for (i, seg) in pending.iter().enumerate() {
        match seg {
            Segment::Field(name) => {
                let mut obj = match value {
                    JsonValue::Object(map) => map,
                    other => {
                        return Err(Error::type_mismatch(path_at(i), JsonType::Object, &other));
                    }
                };
                value = obj
                    .remove(name.as_ref())
                    .ok_or_else(|| Error::NotFound { path: path_at(i) })?;
            }
            Segment::Index(j) => {
                let mut arr = match value {
                    JsonValue::Array(a) => a,
                    other => {
                        return Err(Error::type_mismatch(path_at(i), JsonType::Array, &other));
                    }
                };
                let idx = usize::try_from(*j).map_err(|_| Error::NotFound { path: path_at(i) })?;
                if idx >= arr.len() {
                    return Err(Error::NotFound { path: path_at(i) });
                }
                value = arr.swap_remove(idx);
            }
        }
    }
    Ok(value)
}

/// Collect the top-level keys of the JSON object encoded in `bytes`, in
/// source order. The target object's members come out of `MapAccess` in
/// their on-wire order, so source order is preserved without needing a
/// streaming parser.
fn read_object_keys(bytes: &[u8], full_path: &[Segment]) -> Result<Vec<Box<str>>> {
    let target_path: Path = full_path.to_vec().into();
    let got = peek_json_type(bytes);
    if got != JsonType::Object {
        return Err(Error::TypeMismatch {
            path: target_path,
            expected: JsonType::Object,
            got,
        });
    }
    let ObjectKeysOnly(keys) =
        serde_json::from_slice(bytes).map_err(|e| Error::parse(target_path, e))?;
    Ok(keys)
}

/// `DeserializeSeed` that walks a JSON object and returns the bytes of the
/// first value whose key equals `target`. Subsequent values are skipped (not
/// materialized) and the iteration stops on the first match.
struct FindField<'a> {
    target: &'a str,
}

impl<'de> serde::de::DeserializeSeed<'de> for FindField<'_> {
    type Value = Option<Vec<u8>>;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct Vis<'a> {
            target: &'a str,
        }
        impl<'de> serde::de::Visitor<'de> for Vis<'_> {
            type Value = Option<Vec<u8>>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut hit: Option<Vec<u8>> = None;
                while let Some(k) = m.next_key::<Box<str>>()? {
                    if hit.is_none() && &*k == self.target {
                        let rv: Box<serde_json::value::RawValue> = m.next_value()?;
                        hit = Some(rv.get().as_bytes().to_vec());
                    } else {
                        let _: serde::de::IgnoredAny = m.next_value()?;
                    }
                }
                Ok(hit)
            }
        }
        d.deserialize_map(Vis {
            target: self.target,
        })
    }
}

/// `DeserializeSeed` that walks a JSON array and returns the bytes of the
/// element at ordinal `target`. Earlier elements are skipped (not materialized).
struct FindIndex {
    target: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for FindIndex {
    type Value = Option<Vec<u8>>;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct Vis {
            target: usize,
        }
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = Option<Vec<u8>>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON array")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut i = 0usize;
                let mut hit: Option<Vec<u8>> = None;
                loop {
                    if hit.is_none() && i == self.target {
                        let Some(rv): Option<Box<serde_json::value::RawValue>> =
                            a.next_element()?
                        else {
                            break;
                        };
                        hit = Some(rv.get().as_bytes().to_vec());
                    } else if a.next_element::<serde::de::IgnoredAny>()?.is_none() {
                        break;
                    }
                    i += 1;
                }
                Ok(hit)
            }
        }
        d.deserialize_seq(Vis {
            target: self.target,
        })
    }
}

/// Peek the JSON type of the next value in `bytes` by scanning past
/// whitespace and looking at the first significant byte. JSON literals are
/// unambiguous from their first character.
fn peek_json_type(bytes: &[u8]) -> JsonType {
    for &b in bytes {
        if crate::parser::is_json_ws(b) {
            continue;
        }
        return match b {
            b'{' => JsonType::Object,
            b'[' => JsonType::Array,
            b'"' => JsonType::String,
            b't' | b'f' => JsonType::Boolean,
            b'n' => JsonType::Null,
            _ => JsonType::Number,
        };
    }
    // Empty/whitespace-only input — let the downstream parser produce the
    // actual error. Returning `Null` here is arbitrary; callers always do a
    // real parse after the type check.
    JsonType::Null
}

/// `Deserialize` that captures an object's top-level keys in the order they
/// appear in the JSON source, without materializing values.
struct ObjectKeysOnly(Vec<Box<str>>);

impl<'de> serde::Deserialize<'de> for ObjectKeysOnly {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ObjectKeysOnly;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut keys: Vec<Box<str>> = Vec::with_capacity(m.size_hint().unwrap_or(0));
                while let Some(k) = m.next_key::<Box<str>>()? {
                    let _: serde::de::IgnoredAny = m.next_value()?;
                    keys.push(k);
                }
                Ok(ObjectKeysOnly(keys))
            }
        }
        d.deserialize_map(V)
    }
}
