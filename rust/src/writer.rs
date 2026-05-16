//! Build a JRIF v0 sidecar from a JSON payload.
//!
//! The writer parses the payload with byte-offset tracking and recursively
//! chunks compound values once they exceed a configurable byte threshold.
//! Trivially small values (`null`, booleans, numbers, empty arrays/objects,
//! and short strings) are stored inline without a range. Every object member
//! name covered by a `fields` chunk or named by a `field` chunk is interned
//! into a document-level `keys` dictionary as the tree is built, so each
//! chunk carries integer indices rather than re-listing strings. `keys` is
//! omitted when no `fields` or `field` chunks are emitted. The payload bytes
//! are left unchanged.

use std::collections::HashMap;

use bytes::Bytes;

use crate::document::{ArrayChunk, Document, JRIF_V0_TAG, ObjectChunk, Value};
use crate::error::{Error, Result};
use crate::parser::{self, Member, Value as PValue};

const DEFAULT_MIN_CHUNK_BYTES: u64 = 8 * 1024;
const DEFAULT_MAX_FIELDS_CHUNK_BYTES: u64 = 4 * 1024;
const DEFAULT_MAX_FIELDS_PER_CHUNK: usize = 64;
/// A ranged string spends ~14 bytes on its `range` JSON. Inline strings
/// shorter than this are unambiguously smaller; the writer prefers inline
/// below this threshold.
const DEFAULT_MAX_INLINE_STRING_BYTES: u64 = 14;

/// Builder for a JRIF v0 sidecar document.
///
/// ```no_run
/// # fn main() -> Result<(), jrif::Error> {
/// let payload = std::fs::read("data.json").unwrap();
/// let jrif = jrif::Indexer::new()
///     .min_chunk_bytes(8 * 1024)
///     .build(&payload)?;
/// # let _ = jrif;
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct Indexer {
    min_chunk_bytes: u64,
    max_fields_chunk_bytes: u64,
    max_fields_per_chunk: usize,
    max_inline_string_bytes: u64,
}

impl Default for Indexer {
    fn default() -> Self {
        Self {
            min_chunk_bytes: DEFAULT_MIN_CHUNK_BYTES,
            max_fields_chunk_bytes: DEFAULT_MAX_FIELDS_CHUNK_BYTES,
            max_fields_per_chunk: DEFAULT_MAX_FIELDS_PER_CHUNK,
            max_inline_string_bytes: DEFAULT_MAX_INLINE_STRING_BYTES,
        }
    }
}

impl Indexer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compound values whose byte span is below this threshold are not chunked
    /// (their range becomes the only access path). Default: 32 KiB.
    #[must_use]
    pub const fn min_chunk_bytes(mut self, n: u64) -> Self {
        self.min_chunk_bytes = n;
        self
    }

    /// Maximum byte span covered by a single `fields` chunk. Runs of scalar
    /// members that exceed this size are split into multiple chunks at member
    /// boundaries. Default: 4 KiB.
    #[must_use]
    pub const fn max_fields_chunk_bytes(mut self, n: u64) -> Self {
        self.max_fields_chunk_bytes = n;
        self
    }

    /// Maximum number of object members covered by a single `fields` chunk.
    /// Splits happen at member boundaries when this is exceeded. Default: 64.
    #[must_use]
    pub const fn max_fields_per_chunk(mut self, n: usize) -> Self {
        self.max_fields_per_chunk = n;
        self
    }

    /// Strings whose payload byte length (including the surrounding quotes)
    /// is at or below this threshold are stored inline rather than as a
    /// range pointing into the payload. Default: 14 bytes — the rough size
    /// of the equivalent ranged JSON.
    #[must_use]
    pub const fn max_inline_string_bytes(mut self, n: u64) -> Self {
        self.max_inline_string_bytes = n;
        self
    }

    /// Build a JRIF sidecar from `payload` and serialize it as compact JSON.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDocument`] if `payload` is not valid JSON.
    ///
    /// # Panics
    ///
    /// Panics if `serde_json` fails to serialize the internal `Document` model,
    /// which is infallible for the well-typed primitives used here.
    pub fn build(&self, payload: &[u8]) -> Result<Bytes> {
        let root_value =
            parser::parse(payload).map_err(|e| Error::InvalidDocument(e.to_string()))?;
        let mut keys = KeysBuilder::default();
        let root = build_value(payload, &root_value, self, &mut keys)?;
        let doc = Document::new(
            JRIF_V0_TAG.into(),
            serde_json::Map::new(),
            keys.finish(),
            root,
        );
        let bytes = serde_json::to_vec(&doc).expect("Document is always serializable");
        Ok(Bytes::from(bytes))
    }

    /// Build a JRIF sidecar from a JSONL payload — a sequence of independent
    /// JSON values separated by whitespace.
    ///
    /// The resulting sidecar describes the payload as if it were a JSON
    /// array: the root is `t: "a"` and each record becomes a single
    /// `k: "i"` chunk whose `r` points into the original `.jsonl` bytes.
    /// Records are never grouped into `k: "is"` chunks; the writer emits one
    /// chunk per record so readers can address each record independently
    /// without parsing inter-record bytes as JSON.
    ///
    /// # Limitations
    ///
    /// The payload is not itself a valid JSON array (it has no `[`, `]`, or
    /// `,` separators), so consumers MUST NOT fetch the root range and
    /// expect it to parse. Navigation into specific records via their
    /// `item` chunks works as expected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDocument`] if `payload` is not valid JSONL.
    ///
    /// # Panics
    ///
    /// Panics if `serde_json` fails to serialize the internal `Document`
    /// model.
    pub fn build_jsonl(&self, payload: &[u8]) -> Result<Bytes> {
        let records =
            parser::parse_jsonl(payload).map_err(|e| Error::InvalidDocument(e.to_string()))?;
        let mut keys = KeysBuilder::default();
        let root = build_jsonl_root(payload, &records, self, &mut keys)?;
        let doc = Document::new(
            JRIF_V0_TAG.into(),
            serde_json::Map::new(),
            keys.finish(),
            root,
        );
        let bytes = serde_json::to_vec(&doc).expect("Document is always serializable");
        Ok(Bytes::from(bytes))
    }
}

#[derive(Default)]
struct KeysBuilder {
    index_of: HashMap<Box<str>, u32>,
    keys: Vec<Box<str>>,
}

impl KeysBuilder {
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.index_of.get(name) {
            return i;
        }
        let i = u32::try_from(self.keys.len()).expect("keys fits in u32");
        let boxed: Box<str> = name.into();
        self.index_of.insert(boxed.clone(), i);
        self.keys.push(boxed);
        i
    }

    fn finish(self) -> Box<[Box<str>]> {
        self.keys.into_boxed_slice()
    }
}

fn build_value(
    payload: &[u8],
    value: &PValue,
    cfg: &Indexer,
    keys: &mut KeysBuilder,
) -> Result<Value> {
    Ok(match value {
        PValue::Null { .. } => Value::Value {
            value: serde_json::Value::Null,
        },
        PValue::Bool { value, .. } => Value::Value {
            value: serde_json::Value::Bool(*value),
        },
        PValue::Number { start, end } => {
            let s = std::str::from_utf8(&payload[*start..=*end])
                .map_err(|e| Error::InvalidDocument(format!("number is not UTF-8: {e}")))?;
            let n: serde_json::Number = s.parse().map_err(|e| {
                Error::InvalidDocument(format!("invalid number literal {s:?}: {e}"))
            })?;
            Value::Value {
                value: serde_json::Value::Number(n),
            }
        }
        PValue::String { start, end } => {
            let span = (*end as u64) - (*start as u64) + 1;
            if span <= cfg.max_inline_string_bytes {
                let lit = &payload[*start..=*end];
                let s: String = serde_json::from_slice(lit)
                    .map_err(|e| Error::InvalidDocument(format!("invalid string literal: {e}")))?;
                Value::Value {
                    value: serde_json::Value::String(s),
                }
            } else {
                let range = (*start as u64)..(*end as u64 + 1);
                Value::String { range }
            }
        }
        PValue::Array { start, end, items } => {
            if items.is_empty() {
                Value::Value {
                    value: serde_json::Value::Array(Vec::new()),
                }
            } else {
                let range = (*start as u64)..(*end as u64 + 1);
                let chunks = build_array_chunks(payload, items, cfg, keys)?;
                Value::Array {
                    range,
                    chunks: chunks.into(),
                }
            }
        }
        PValue::Object {
            start,
            end,
            members,
        } => {
            if members.is_empty() {
                Value::Value {
                    value: serde_json::Value::Object(serde_json::Map::new()),
                }
            } else {
                let range = (*start as u64)..(*end as u64 + 1);
                let chunks = build_object_chunks(payload, members, cfg, keys)?;
                Value::Object {
                    range,
                    chunks: chunks.into(),
                }
            }
        }
    })
}

/// Build the root Value for a JSONL payload. The root is shaped like a
/// ranged JSON array (so existing readers can navigate it via item chunks),
/// but the payload bytes are not actually a JSON array — they are JSONL.
/// Every record becomes a single `item` chunk; no `items` grouping happens,
/// because inter-record bytes are JSONL whitespace rather than JSON commas.
fn build_jsonl_root(
    payload: &[u8],
    records: &[PValue],
    cfg: &Indexer,
    keys: &mut KeysBuilder,
) -> Result<Value> {
    let range = 0u64..payload.len() as u64;
    let mut chunks: Vec<ArrayChunk> = Vec::with_capacity(records.len());
    for record in records {
        chunks.push(ArrayChunk::Item {
            value: build_value(payload, record, cfg, keys)?,
        });
    }
    Ok(Value::Array {
        range,
        chunks: chunks.into(),
    })
}

fn build_array_chunks(
    payload: &[u8],
    items: &[PValue],
    cfg: &Indexer,
    keys: &mut KeysBuilder,
) -> Result<Vec<ArrayChunk>> {
    let (Some(first), Some(last)) = (items.first(), items.last()) else {
        return Ok(vec![]);
    };
    let span = last.end() as u64 - first.start() as u64 + 1;
    if span < cfg.min_chunk_bytes {
        return Ok(vec![]);
    }

    let mut chunks = Vec::new();
    let mut scalar_start: Option<usize> = None;

    for (i, item) in items.iter().enumerate() {
        if item.is_compound() {
            if let Some(start) = scalar_start.take() {
                chunks.push(items_chunk(items, start, i - 1));
            }
            chunks.push(ArrayChunk::Item {
                value: build_value(payload, item, cfg, keys)?,
            });
        } else {
            scalar_start.get_or_insert(i);
        }
    }
    if let Some(start) = scalar_start.take() {
        chunks.push(items_chunk(items, start, items.len() - 1));
    }
    Ok(chunks)
}

fn items_chunk(items: &[PValue], from: usize, to: usize) -> ArrayChunk {
    let range = items[from].start() as u64..items[to].end() as u64 + 1;
    ArrayChunk::Items {
        count: (to - from + 1) as u64,
        range,
    }
}

fn build_object_chunks(
    payload: &[u8],
    members: &[Member],
    cfg: &Indexer,
    keys: &mut KeysBuilder,
) -> Result<Vec<ObjectChunk>> {
    let (Some(first), Some(last)) = (members.first(), members.last()) else {
        return Ok(vec![]);
    };
    let span = last.value.end() as u64 - first.name_start as u64 + 1;
    if span < cfg.min_chunk_bytes {
        return Ok(vec![]);
    }

    let mut chunks = Vec::new();
    let mut scalar_start: Option<usize> = None;

    for (i, member) in members.iter().enumerate() {
        if member.value.is_compound() {
            if let Some(start) = scalar_start.take() {
                emit_fields_chunks(members, start, i - 1, cfg, keys, &mut chunks);
            }
            chunks.push(ObjectChunk::Field {
                name: keys.intern(&member.name),
                value: build_value(payload, &member.value, cfg, keys)?,
            });
        } else {
            scalar_start.get_or_insert(i);
        }
    }
    if let Some(start) = scalar_start.take() {
        emit_fields_chunks(members, start, members.len() - 1, cfg, keys, &mut chunks);
    }
    Ok(chunks)
}

/// Split a contiguous run of scalar members `[from..=to]` into one or more
/// `fields` chunks that each respect both the byte cap and the member-count
/// cap. Splits happen at member boundaries.
fn emit_fields_chunks(
    members: &[Member],
    from: usize,
    to: usize,
    cfg: &Indexer,
    keys: &mut KeysBuilder,
    out: &mut Vec<ObjectChunk>,
) {
    let mut chunk_start = from;
    let mut i = from;
    while i <= to {
        let span = members[i].value.end() as u64 - members[chunk_start].name_start as u64 + 1;
        let count = i - chunk_start + 1;
        let over_bytes = span > cfg.max_fields_chunk_bytes && i > chunk_start;
        let over_count = count > cfg.max_fields_per_chunk;
        if over_bytes || over_count {
            out.push(fields_chunk(members, chunk_start, i - 1, keys));
            chunk_start = i;
            continue;
        }
        i += 1;
    }
    if chunk_start <= to {
        out.push(fields_chunk(members, chunk_start, to, keys));
    }
}

fn fields_chunk(members: &[Member], from: usize, to: usize, keys: &mut KeysBuilder) -> ObjectChunk {
    let range = members[from].name_start as u64..members[to].value.end() as u64 + 1;
    let fields: Box<[u32]> = members[from..=to]
        .iter()
        .map(|m| keys.intern(&m.name))
        .collect();
    ObjectChunk::Fields { fields, range }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn build_doc(payload: &[u8], cfg: &Indexer) -> Document {
        let v = parser::parse(payload).unwrap();
        let mut keys = KeysBuilder::default();
        let root = build_value(payload, &v, cfg, &mut keys).unwrap();
        Document::new(
            JRIF_V0_TAG.into(),
            serde_json::Map::new(),
            keys.finish(),
            root,
        )
    }

    fn build_jsonl_doc(payload: &[u8], cfg: &Indexer) -> Document {
        let records = parser::parse_jsonl(payload).unwrap();
        let mut keys = KeysBuilder::default();
        let root = build_jsonl_root(payload, &records, cfg, &mut keys).unwrap();
        Document::new(
            JRIF_V0_TAG.into(),
            serde_json::Map::new(),
            keys.finish(),
            root,
        )
    }

    #[test]
    fn jsonl_emits_one_item_per_record() {
        let payload = b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n";
        let doc = build_jsonl_doc(payload, &Indexer::default());
        let Value::Array { range, chunks, .. } = &doc.root else {
            panic!("expected ranged array root");
        };
        assert_eq!(*range, 0..payload.len() as u64);
        assert_eq!(chunks.len(), 3);
        for c in chunks {
            assert!(matches!(c, ArrayChunk::Item { .. }));
        }
    }

    #[test]
    fn jsonl_item_ranges_point_at_original_lines() {
        let payload = b"{\"id\":1}\n{\"id\":2}\n";
        let doc = build_jsonl_doc(payload, &Indexer::default());
        let Value::Array { chunks, .. } = &doc.root else {
            panic!("expected ranged array root");
        };
        // First record: bytes 0..=7 ("{\"id\":1}"), second: 9..=16 ("{\"id\":2}").
        let ranges: Vec<_> = chunks
            .iter()
            .map(|c| match c {
                ArrayChunk::Item {
                    value: Value::Object { range, .. },
                } => range.clone(),
                _ => panic!("unexpected chunk shape"),
            })
            .collect();
        assert_eq!(ranges, vec![0..8, 9..17]);
    }

    #[test]
    fn jsonl_empty_payload_yields_empty_chunks() {
        let payload = b"";
        let doc = build_jsonl_doc(payload, &Indexer::default());
        let Value::Array { chunks, range, .. } = &doc.root else {
            panic!("expected array root");
        };
        assert!(chunks.is_empty());
        assert_eq!(*range, 0..0);
    }

    #[test]
    fn root_covers_entire_payload() {
        let payload = br#"{"a":1}"#;
        let doc = build_doc(payload, &Indexer::default());
        let Value::Object { range, .. } = doc.root else {
            panic!("expected ranged object root");
        };
        assert_eq!(range, 0..payload.len() as u64);
    }

    #[test]
    fn skips_chunking_for_small_compound_values() {
        let payload = br#"{"a":1,"b":2}"#;
        let doc = build_doc(payload, &Indexer::default());
        let Value::Object { chunks, .. } = doc.root else {
            panic!("expected ranged object root");
        };
        assert!(chunks.is_empty());
    }

    #[test]
    fn empty_array_is_inline() {
        let payload = br"[]";
        let doc = build_doc(payload, &Indexer::default());
        match &doc.root {
            Value::Value {
                value: serde_json::Value::Array(arr),
            } if arr.is_empty() => {}
            other => panic!("expected inline empty array, got {other:?}"),
        }
    }

    #[test]
    fn empty_object_is_inline() {
        let payload = br"{}";
        let doc = build_doc(payload, &Indexer::default());
        match &doc.root {
            Value::Value {
                value: serde_json::Value::Object(obj),
            } if obj.is_empty() => {}
            other => panic!("expected inline empty object, got {other:?}"),
        }
    }

    #[test]
    fn primitives_at_root_are_inline() {
        for payload in [&b"null"[..], b"true", b"false", b"42", b"\"ok\""] {
            let doc = build_doc(payload, &Indexer::default());
            match doc.root {
                Value::Value { .. } => {}
                other => panic!("expected inline primitive, got {other:?} for {payload:?}"),
            }
        }
    }

    #[test]
    fn long_strings_stay_ranged() {
        // Payload well above DEFAULT_MAX_INLINE_STRING_BYTES.
        let payload = b"\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"";
        let doc = build_doc(payload, &Indexer::default());
        assert!(matches!(doc.root, Value::String { .. }));
    }

    #[test]
    fn fields_chunk_splits_on_byte_cap() {
        let mut payload = String::from("{");
        for i in 0..100 {
            if i > 0 {
                payload.push(',');
            }
            let _ = write!(payload, "\"k{i:02}\":\"v{i:02}\"");
        }
        payload.push('}');
        let doc = build_doc(
            payload.as_bytes(),
            &Indexer::new()
                .min_chunk_bytes(16)
                .max_fields_chunk_bytes(128)
                .max_fields_per_chunk(usize::MAX),
        );
        let Value::Object { chunks, .. } = doc.root else {
            panic!("expected ranged object");
        };
        let fields_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| matches!(c, ObjectChunk::Fields { .. }))
            .collect();
        assert!(
            fields_chunks.len() >= 2,
            "expected splitter to emit >=2 fields chunks, got {}",
            fields_chunks.len()
        );
        for c in fields_chunks {
            if let ObjectChunk::Fields { range, .. } = c {
                let span = range.end - range.start;
                assert!(span <= 128 || range.end - range.start == 0);
            }
        }
    }

    #[test]
    fn fields_chunk_splits_on_count_cap() {
        let mut payload = String::from("{");
        for i in 0..50 {
            if i > 0 {
                payload.push(',');
            }
            let _ = write!(payload, "\"k{i:02}\":{i}");
        }
        payload.push('}');
        let doc = build_doc(
            payload.as_bytes(),
            &Indexer::new()
                .min_chunk_bytes(16)
                .max_fields_chunk_bytes(u64::MAX)
                .max_fields_per_chunk(10),
        );
        let Value::Object { chunks, .. } = doc.root else {
            panic!("expected ranged object");
        };
        let fields_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| matches!(c, ObjectChunk::Fields { .. }))
            .collect();
        // 50 members / 10-per-chunk → 5 chunks.
        assert_eq!(fields_chunks.len(), 5);
        for c in fields_chunks {
            if let ObjectChunk::Fields { fields, .. } = c {
                assert!(fields.len() <= 10);
            }
        }
    }

    #[test]
    fn keys_emitted_when_object_has_fields_chunks() {
        fn check(v: &Value, keys_len: usize) {
            match v {
                Value::Array { chunks, .. } => {
                    for c in chunks {
                        if let ArrayChunk::Item { value } = c {
                            check(value, keys_len);
                        }
                    }
                }
                Value::Object { chunks, .. } => {
                    for c in chunks {
                        match c {
                            ObjectChunk::Fields { fields, .. } => {
                                assert!(!fields.is_empty(), "expected non-empty `fields`");
                                for &i in fields {
                                    assert!((i as usize) < keys_len, "fields index out of range");
                                }
                            }
                            ObjectChunk::Field { value, .. } => check(value, keys_len),
                        }
                    }
                }
                _ => {}
            }
        }

        let mut payload = String::from("[");
        for i in 0..20 {
            if i > 0 {
                payload.push(',');
            }
            let _ = write!(payload, "{{\"id\":{i},\"name\":\"row{i}\",\"score\":{i}}}");
        }
        payload.push(']');
        let doc = build_doc(payload.as_bytes(), &Indexer::new().min_chunk_bytes(16));
        assert!(!doc.keys.is_empty(), "expected `keys` to be populated");
        assert!(doc.keys.iter().any(|k| &**k == "id"));
        assert!(doc.keys.iter().any(|k| &**k == "name"));
        assert!(doc.keys.iter().any(|k| &**k == "score"));

        check(&doc.root, doc.keys.len());
    }

    #[test]
    fn keys_omitted_when_no_fields_chunks() {
        // Payload too small to trigger object chunking → no `fields` chunks
        // are emitted, so `keys` stays empty (and serializes as absent).
        let payload = br#"{"id":1,"name":"alone","score":0.5,"extra":"x"}"#;
        let doc = build_doc(payload, &Indexer::new().min_chunk_bytes(1024));
        assert!(
            doc.keys.is_empty(),
            "expected no `keys` when no fields chunks are emitted"
        );
    }
}
