//! Chunk-tree navigation primitives shared by the reader Cursor and the
//! writer-side encoder. Internal — not part of the public API.

use std::ops::Range;

use crate::document::{
    ArrayChunk, ArrayChunks, ChunksRef, ObjectChunk, ObjectChunks, Resolved, Value,
};

/// How a fetched byte range relates to ordinary JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    /// The bytes are a complete JSON value — no wrapping required.
    None,
    /// The bytes cover a run of array items separated by commas. Wrap with
    /// `[` `]` before parsing.
    Array,
    /// The bytes cover a run of object members separated by commas. Wrap with
    /// `{` `}` before parsing.
    Object,
}

/// A chunk-navigation context. Knowing the frame lets us interpret the next
/// path segment as either an array index or an object field name.
#[derive(Clone, Copy)]
pub enum Frame<'a> {
    Array(&'a ArrayChunks),
    Object(&'a ObjectChunks),
    /// Reached after stepping into a chunk with no nested chunks, or after
    /// landing on an inline `Value`. Further navigation must defer to a parse
    /// step (or short-circuit if the inline value is the final target).
    Done,
}

pub enum ArrayHit<'a> {
    Item {
        value: &'a Value,
    },
    Items {
        range: Range<u64>,
        start_ordinal: u64,
    },
}

pub enum ObjectHitBorrow<'a> {
    Field { value: &'a Value },
    Fields { range: Range<u64> },
}

/// O(log N) random access: binary searches the chunks' cumulative-ordinal
/// table. Returns `None` when `ordinal` is past the array end.
pub fn find_array_match(chunks: &ArrayChunks, ordinal: u64) -> Option<ArrayHit<'_>> {
    let (chunk_idx, start_ordinal) = chunks.find_index(ordinal)?;
    Some(match &chunks.as_slice()[chunk_idx] {
        ArrayChunk::Item { value } => ArrayHit::Item { value },
        ArrayChunk::Items { range, .. } => ArrayHit::Items {
            range: range.clone(),
            start_ordinal,
        },
    })
}

/// Locate a member by name in this object's chunks. The `keys` table resolves
/// integer indices both in `Fields` chunks' `fields` and in `Field` chunks'
/// `name`; pass `&[]` when the document has no `keys` (in which case neither
/// chunk kind can match — both require a non-empty `keys` table).
pub fn find_object_match<'a>(
    chunks: &'a ObjectChunks,
    name: &str,
    keys: &'a [Box<str>],
    key_idx: Option<u32>,
) -> Option<ObjectHitBorrow<'a>> {
    if let Some(key_idx) = key_idx
        && let Some(chunk) = chunks.find_key(key_idx)
    {
        return object_hit_from_chunk(chunk, key_idx);
    }
    for chunk in chunks {
        match chunk {
            ObjectChunk::Field {
                name: field_idx,
                value,
            } if keys.get(*field_idx as usize).is_some_and(|k| &**k == name) => {
                return Some(ObjectHitBorrow::Field { value });
            }
            ObjectChunk::Fields { fields, range } if fields_chunk_covers(fields, keys, name) => {
                return Some(ObjectHitBorrow::Fields {
                    range: range.clone(),
                });
            }
            _ => {}
        }
    }
    None
}

fn object_hit_from_chunk(chunk: &ObjectChunk, key_idx: u32) -> Option<ObjectHitBorrow<'_>> {
    match chunk {
        ObjectChunk::Field { name, value } if *name == key_idx => {
            Some(ObjectHitBorrow::Field { value })
        }
        ObjectChunk::Fields { fields, range } if fields.contains(&key_idx) => {
            Some(ObjectHitBorrow::Fields {
                range: range.clone(),
            })
        }
        _ => None,
    }
}

fn fields_chunk_covers(fields: &[u32], keys: &[Box<str>], name: &str) -> bool {
    fields
        .iter()
        .any(|&i| keys.get(i as usize).is_some_and(|k| &**k == name))
}

/// Navigation frame for a resolved value. Returns `Frame::Array` /
/// `Frame::Object` only for ranged compounds with non-empty chunk lists;
/// inline values and primitives yield `Frame::Done`.
pub fn frame_of(value: &Value) -> Frame<'_> {
    match value.resolve() {
        Resolved::Ranged {
            chunks: ChunksRef::Array(c),
            ..
        } if !c.is_empty() => Frame::Array(c),
        Resolved::Ranged {
            chunks: ChunksRef::Object(c),
            ..
        } if !c.is_empty() => Frame::Object(c),
        _ => Frame::Done,
    }
}

/// Sequential walker over an array's chunks. Each `next` call yields one
/// logical array element in source order in O(1) amortized — used by
/// `ArrayIter` to avoid an O(N) chunk scan per `Cursor::index` step.
pub struct ArrayWalker<'a> {
    chunks: &'a [ArrayChunk],
    chunk_idx: usize,
    in_chunk: u64,
    ordinal: u64,
}

impl<'a> ArrayWalker<'a> {
    #[must_use]
    pub fn new(chunks: &'a ArrayChunks) -> Self {
        Self {
            chunks: chunks.as_slice(),
            chunk_idx: 0,
            in_chunk: 0,
            ordinal: 0,
        }
    }

    /// Yield `(hit, ordinal)` for the next element, or `None` when exhausted.
    pub fn next(&mut self) -> Option<(ArrayHit<'a>, u64)> {
        loop {
            let chunk = self.chunks.get(self.chunk_idx)?;
            match chunk {
                ArrayChunk::Item { value } => {
                    let ord = self.ordinal;
                    self.chunk_idx += 1;
                    self.in_chunk = 0;
                    self.ordinal += 1;
                    return Some((ArrayHit::Item { value }, ord));
                }
                ArrayChunk::Items { count, range } => {
                    if self.in_chunk >= *count {
                        self.ordinal += *count - self.in_chunk; // typically 0
                        self.chunk_idx += 1;
                        self.in_chunk = 0;
                        continue;
                    }
                    let ord = self.ordinal + self.in_chunk;
                    let hit = ArrayHit::Items {
                        range: range.clone(),
                        start_ordinal: self.ordinal,
                    };
                    self.in_chunk += 1;
                    if self.in_chunk >= *count {
                        self.ordinal += *count;
                        self.chunk_idx += 1;
                        self.in_chunk = 0;
                    }
                    return Some((hit, ord));
                }
            }
        }
    }
}
