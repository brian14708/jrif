//! Serde data model for the JRIF v0 sidecar document. Internal — not part of
//! the public API.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

pub const JRIF_V0_TAG: &str = "v0";

/// JSON type tag, surfaced in [`Error::TypeMismatch`](crate::Error::TypeMismatch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JsonType {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl JsonType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

impl fmt::Display for JsonType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&JsonValue> for JsonType {
    fn from(v: &JsonValue) -> Self {
        match v {
            JsonValue::Null => Self::Null,
            JsonValue::Bool(_) => Self::Boolean,
            JsonValue::Number(_) => Self::Number,
            JsonValue::String(_) => Self::String,
            JsonValue::Array(_) => Self::Array,
            JsonValue::Object(_) => Self::Object,
        }
    }
}

/// Serde adapter: convert between in-memory `Range<u64>` (half-open) and the
/// spec's `[start, length]` wire format.
pub mod range_wire {
    use std::ops::Range;

    use serde::{
        Deserialize, Deserializer, Serialize, Serializer, de::Error as DeError,
        ser::Error as SerError,
    };

    pub fn serialize<S: Serializer>(r: &Range<u64>, s: S) -> Result<S::Ok, S::Error> {
        let length = r
            .end
            .checked_sub(r.start)
            .ok_or_else(|| S::Error::custom("range end < start"))?;
        if length == 0 {
            return Err(S::Error::custom("empty range is not representable in v0"));
        }
        [r.start, length].serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Range<u64>, D::Error> {
        let [start, length] = <[u64; 2]>::deserialize(d)?;
        if length == 0 {
            return Err(D::Error::custom("range length is zero"));
        }
        let end = start
            .checked_add(length)
            .ok_or_else(|| D::Error::custom("start + length overflows u64"))?;
        Ok(start..end)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub jrif: Box<str>,
    /// Optional record-keeping metadata about the indexed payload (e.g.
    /// original size, content digest). Free-form: readers MUST NOT use any
    /// value under `meta` to drive range decoding or member resolution.
    /// Omitted when no metadata is recorded.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub meta: serde_json::Map<String, JsonValue>,
    /// Document-level key dictionary. Required whenever the document contains
    /// at least one `fields` or `field` chunk; every chunk's `fields` integer
    /// entries and every `field` chunk's `name` index into this array. Omitted
    /// when no such chunks exist.
    #[serde(default, skip_serializing_if = "<[Box<str>]>::is_empty")]
    pub keys: Box<[Box<str>]>,
    pub root: Value,
    #[serde(skip)]
    key_lookup: OnceLock<HashMap<Box<str>, u32>>,
}

impl Document {
    #[must_use]
    pub const fn new(
        jrif: Box<str>,
        meta: serde_json::Map<String, JsonValue>,
        keys: Box<[Box<str>]>,
        root: Value,
    ) -> Self {
        Self {
            jrif,
            meta,
            keys,
            root,
            key_lookup: OnceLock::new(),
        }
    }

    /// Resolve a field name to its document-level key-table index.
    #[must_use]
    pub fn key_index(&self, name: &str) -> Option<u32> {
        self.key_lookup().get(name).copied()
    }

    fn key_lookup(&self) -> &HashMap<Box<str>, u32> {
        self.key_lookup.get_or_init(|| {
            self.keys
                .iter()
                .enumerate()
                .filter_map(|(i, k)| u32::try_from(i).ok().map(|idx| (k.clone(), idx)))
                .collect()
        })
    }
}

/// A JSON value as described by JRIF. The `t` tag is the single
/// discriminator: `Inline` carries an arbitrary inline JSON literal; the
/// remaining variants are ranged values that live in the payload byte stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Value {
    #[serde(rename = "v")]
    #[expect(clippy::enum_variant_names)]
    Value {
        #[serde(rename = "v")]
        value: JsonValue,
    },
    #[serde(rename = "s")]
    String {
        #[serde(rename = "r", with = "range_wire")]
        range: Range<u64>,
    },
    #[serde(rename = "a")]
    Array {
        #[serde(rename = "r", with = "range_wire")]
        range: Range<u64>,
        #[serde(rename = "c", default, skip_serializing_if = "ArrayChunks::is_empty")]
        chunks: ArrayChunks,
    },
    #[serde(rename = "o")]
    Object {
        #[serde(rename = "r", with = "range_wire")]
        range: Range<u64>,
        #[serde(rename = "c", default, skip_serializing_if = "ObjectChunks::is_empty")]
        chunks: ObjectChunks,
    },
}

/// Resolution result for a `Value`: either inline (and immediately usable) or
/// ranged (must be fetched before use).
pub enum Resolved<'a> {
    /// Inline value. Carries the materialized JSON literal.
    Inline(&'a JsonValue),
    Ranged {
        range: Range<u64>,
        chunks: ChunksRef<'a>,
    },
}

#[derive(Clone, Copy)]
pub enum ChunksRef<'a> {
    None,
    Array(&'a ArrayChunks),
    Object(&'a ObjectChunks),
}

impl Value {
    pub fn resolve(&self) -> Resolved<'_> {
        match self {
            Self::Value { value } => Resolved::Inline(value),
            Self::String { range } => Resolved::Ranged {
                range: range.clone(),
                chunks: ChunksRef::None,
            },
            Self::Array { range, chunks } => Resolved::Ranged {
                range: range.clone(),
                chunks: ChunksRef::Array(chunks),
            },
            Self::Object { range, chunks } => Resolved::Ranged {
                range: range.clone(),
                chunks: ChunksRef::Object(chunks),
            },
        }
    }
}

/// Owned array chunks list with a lazily-computed cumulative-ordinal table
/// for O(log N) random access via [`find_index`](Self::find_index).
///
/// The table is computed once per instance on first random access (or on
/// `len`/`walker` calls that need it) and cached behind a `OnceLock`. The
/// wire form is just the inner `Box<[ArrayChunk]>`.
pub struct ArrayChunks {
    chunks: Box<[ArrayChunk]>,
    /// `cumulative[i]` is the running ordinal *before* `chunks[i]`, with a
    /// trailing entry equal to the total length. Empty when `chunks` is empty.
    cumulative: OnceLock<Box<[u64]>>,
}

impl ArrayChunks {
    /// Wrap a chunks list, deferring the cumulative-table build.
    #[must_use]
    pub const fn new(chunks: Box<[ArrayChunk]>) -> Self {
        Self {
            chunks,
            cumulative: OnceLock::new(),
        }
    }

    /// Underlying chunks list. Read-only view; the cumulative table is kept
    /// consistent because chunks are never mutated after construction.
    #[must_use]
    pub fn as_slice(&self) -> &[ArrayChunk] {
        &self.chunks
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Cumulative ordinal table, computed on first use.
    pub fn cumulative(&self) -> &[u64] {
        self.cumulative.get_or_init(|| {
            let mut out = Vec::with_capacity(self.chunks.len() + 1);
            let mut running: u64 = 0;
            out.push(running);
            for c in &*self.chunks {
                running += match c {
                    ArrayChunk::Item { .. } => 1,
                    ArrayChunk::Items { count, .. } => *count,
                };
                out.push(running);
            }
            out.into_boxed_slice()
        })
    }

    /// Total logical array length (sum of chunk counts). `None` for empty.
    #[must_use]
    pub fn array_len(&self) -> Option<u64> {
        if self.chunks.is_empty() {
            return None;
        }
        // Safety: cumulative has chunks.len() + 1 entries; last() is total.
        Some(*self.cumulative().last().expect("non-empty cumulative"))
    }

    /// Locate the chunk index covering `ordinal` and the running ordinal at
    /// the start of that chunk. O(log N) via binary search over the cumulative
    /// table for large chunk lists; falls back to a linear scan for small
    /// lists where allocating + binary-searching the table costs more than
    /// just walking. Returns `None` when `ordinal` is past the array end.
    #[must_use]
    pub fn find_index(&self, ordinal: u64) -> Option<(usize, u64)> {
        // Threshold tuned so the cumulative-table build pays off on the
        // second random access; below it, linear scan wins both in raw cycle
        // count and by skipping the allocation entirely.
        const LINEAR_SCAN_THRESHOLD: usize = 16;
        if self.chunks.is_empty() {
            return None;
        }
        if self.cumulative.get().is_none() && self.chunks.len() <= LINEAR_SCAN_THRESHOLD {
            return self.find_index_linear(ordinal);
        }
        let cum = self.cumulative();
        let chunk_idx = cum.partition_point(|&c| c <= ordinal);
        if chunk_idx == 0 || chunk_idx > self.chunks.len() {
            return None;
        }
        if ordinal >= *cum.last().expect("non-empty cumulative") {
            return None;
        }
        Some((chunk_idx - 1, cum[chunk_idx - 1]))
    }

    fn find_index_linear(&self, ordinal: u64) -> Option<(usize, u64)> {
        let mut running: u64 = 0;
        for (i, c) in self.chunks.iter().enumerate() {
            let count = match c {
                ArrayChunk::Item { .. } => 1,
                ArrayChunk::Items { count, .. } => *count,
            };
            if ordinal < running + count {
                return Some((i, running));
            }
            running += count;
        }
        None
    }
}

impl Default for ArrayChunks {
    fn default() -> Self {
        Self::new(Box::new([]))
    }
}

// Manual Clone: OnceLock isn't Clone. A clone resets the cache (it will be
// rebuilt on demand). Cheap relative to recomputing from chunks.
impl Clone for ArrayChunks {
    fn clone(&self) -> Self {
        Self::new(self.chunks.clone())
    }
}

impl fmt::Debug for ArrayChunks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `cumulative` is a derived cache rebuilt on demand; omit it from
        // Debug output so structurally-equal `ArrayChunks` print identically
        // regardless of whether the cache has been populated.
        f.debug_struct("ArrayChunks")
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

impl Serialize for ArrayChunks {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.chunks.serialize(s)
    }
}

impl<'de> Deserialize<'de> for ArrayChunks {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Box::<[ArrayChunk]>::deserialize(d).map(Self::new)
    }
}

impl From<Box<[ArrayChunk]>> for ArrayChunks {
    fn from(chunks: Box<[ArrayChunk]>) -> Self {
        Self::new(chunks)
    }
}

impl From<Vec<ArrayChunk>> for ArrayChunks {
    fn from(chunks: Vec<ArrayChunk>) -> Self {
        Self::new(chunks.into_boxed_slice())
    }
}

impl std::ops::Deref for ArrayChunks {
    type Target = [ArrayChunk];
    fn deref(&self) -> &Self::Target {
        &self.chunks
    }
}

impl<'a> IntoIterator for &'a ArrayChunks {
    type Item = &'a ArrayChunk;
    type IntoIter = std::slice::Iter<'a, ArrayChunk>;
    fn into_iter(self) -> Self::IntoIter {
        self.chunks.iter()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k")]
pub enum ArrayChunk {
    #[serde(rename = "is")]
    Items {
        #[serde(rename = "n")]
        count: u64,
        #[serde(rename = "r", with = "range_wire")]
        range: Range<u64>,
    },
    #[serde(rename = "i")]
    Item {
        #[serde(flatten)]
        value: Value,
    },
}

/// Owned object chunks list with a lazily-computed key-index lookup.
///
/// The lookup maps document-level key IDs to chunk positions and preserves
/// chunk order by keeping the first chunk that claims a key.
pub struct ObjectChunks {
    chunks: Box<[ObjectChunk]>,
    lookup: OnceLock<HashMap<u32, usize>>,
}

impl ObjectChunks {
    #[must_use]
    pub const fn new(chunks: Box<[ObjectChunk]>) -> Self {
        Self {
            chunks,
            lookup: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[ObjectChunk] {
        &self.chunks
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    #[must_use]
    pub fn find_key(&self, key_idx: u32) -> Option<&ObjectChunk> {
        let lookup = self.lookup.get_or_init(|| {
            let mut out = HashMap::new();
            for (chunk_idx, chunk) in self.chunks.iter().enumerate() {
                match chunk {
                    ObjectChunk::Field { name, .. } => {
                        out.entry(*name).or_insert(chunk_idx);
                    }
                    ObjectChunk::Fields { fields, .. } => {
                        for &field in &**fields {
                            out.entry(field).or_insert(chunk_idx);
                        }
                    }
                }
            }
            out
        });
        lookup
            .get(&key_idx)
            .and_then(|&chunk_idx| self.chunks.get(chunk_idx))
    }
}

impl Default for ObjectChunks {
    fn default() -> Self {
        Self::new(Box::new([]))
    }
}

impl Clone for ObjectChunks {
    fn clone(&self) -> Self {
        Self::new(self.chunks.clone())
    }
}

impl fmt::Debug for ObjectChunks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectChunks")
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

impl Serialize for ObjectChunks {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.chunks.serialize(s)
    }
}

impl<'de> Deserialize<'de> for ObjectChunks {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Box::<[ObjectChunk]>::deserialize(d).map(Self::new)
    }
}

impl From<Box<[ObjectChunk]>> for ObjectChunks {
    fn from(chunks: Box<[ObjectChunk]>) -> Self {
        Self::new(chunks)
    }
}

impl From<Vec<ObjectChunk>> for ObjectChunks {
    fn from(chunks: Vec<ObjectChunk>) -> Self {
        Self::new(chunks.into_boxed_slice())
    }
}

impl std::ops::Deref for ObjectChunks {
    type Target = [ObjectChunk];
    fn deref(&self) -> &Self::Target {
        &self.chunks
    }
}

impl<'a> IntoIterator for &'a ObjectChunks {
    type Item = &'a ObjectChunk;
    type IntoIter = std::slice::Iter<'a, ObjectChunk>;
    fn into_iter(self) -> Self::IntoIter {
        self.chunks.iter()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k")]
pub enum ObjectChunk {
    #[serde(rename = "fs")]
    Fields {
        /// Non-empty array of indices into the document-level `keys` table,
        /// naming the member names covered by this chunk.
        #[serde(rename = "f")]
        fields: Box<[u32]>,
        #[serde(rename = "r", with = "range_wire")]
        range: Range<u64>,
    },
    #[serde(rename = "f")]
    Field {
        /// Index into the document-level `keys` table, naming the member
        /// covered by this chunk.
        #[serde(rename = "n")]
        name: u32,
        #[serde(flatten)]
        value: Value,
    },
}
