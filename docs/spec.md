# JSON Range Index Format

Status: draft

JSON Range Index Format (JRIF) is a JSON sidecar format for partial loading of
large JSON documents from byte-addressable storage, including object stores that
support ranged GET. The indexed payload remains ordinary RFC 8259 JSON.

This document is the normative format specification for JRIF version 1. The
JSON Schema appendix defines the syntactic shape of a `.jrif` file. Some
requirements in this document are semantic constraints that cannot be fully
expressed in JSON Schema.

## Conformance Language

The key words `MUST`, `MUST NOT`, `REQUIRED`, `SHOULD`, `SHOULD NOT`, and `MAY`
are to be interpreted as described in RFC 2119 and RFC 8174 when, and only when,
they appear in uppercase.

## File Set

A JRIF index is stored next to the JSON payload it describes:

```text
object.json
object.json.jrif
```

`object.json.jrif` is a single JSON document with MIME type
`application/vnd.jrif+json`. Version 1 does not define secondary index
files, external chunk catalogs, or multi-file indexes.

The `.jrif` file describes one immutable payload byte stream. A writer MUST
compute the index from the exact payload bytes that readers will later receive.
If the payload bytes change, the existing index MUST be considered invalid.

## Design Goals

Version 1 is designed to:

- Preserve ordinary JSON payloads.
- Map selected JSON paths to byte ranges.
- Chunk large arrays by item ordinal.
- Chunk large objects by field groups.
- Support arrays of large object records.
- Verify fetched ranges with checksums.

Version 1 intentionally does not define:

- A new JSON syntax.
- Search, predicate, inverted, or secondary indexes.
- In-place mutation.
- Efficient access to every scalar by default.
- Arbitrary range loading of ordinary single-stream `.json.gz` files.

## Byte Ranges

All ranges use byte offsets into the payload byte stream, not Unicode scalar,
character, or line offsets.

A range is a two-element JSON array:

```json
[start, end]
```

Both offsets are zero-based inclusive byte offsets into the payload byte stream.
The covered byte sequence is:

```text
payload[start:end + 1]
```

For every range, `start` and `end` MUST be non-negative integers and `start`
MUST be less than or equal to `end`. `end` MUST be less than the payload byte
stream size.

Empty payload fragments are not representable as ranges in version 1. Empty
arrays and empty objects are represented by their node range, which covers the
complete JSON value (`[]` or `{}`), and MUST NOT contain `items` or `fields`
chunks.

## Checksums

Every independently loadable range MUST have a checksum over the exact payload
bytes covered by that range:

```text
checksum = xxh3-128(payload[start:end + 1])
```

Checksums are encoded as:

```text
hex_digest
```

Hex digests MUST use lowercase hexadecimal characters and MUST be 32 hex
digits long. Version 1 checksums are `xxh3-128` digests.

Readers MUST verify the checksum of each fetched range before parsing or
returning data from that range.

## Top-Level Document

A JRIF document has this top-level shape:

```json
{
  "$schema": "https://brian14708.github.io/jrif/v1/schema.json",
  "root": {
    "type": "object",
    "range": [0, 982341233],
    "checksum": "..."
  }
}
```

Top-level fields:

- `$schema`: MUST be `"https://brian14708.github.io/jrif/v1/schema.json"`.
- `root`: describes the root JSON value.

Readers MUST reject unknown `$schema` values.

## Nodes

A node describes a JSON value. `type` MUST be one of:

```text
null boolean number string array object
```

All nodes have these REQUIRED fields:

```json
{
  "type": "object",
  "range": [100, 2000],
  "checksum": "..."
}
```

The `range` of a node MUST cover the complete serialized JSON value for that
node. For object members, this means the value bytes only, excluding the member
name and colon. For array items, this means the item value bytes only, excluding
neighboring separators.

Writers SHOULD omit `chunks` when it would be empty.

## Chunks

Chunks describe independently loadable fragments inside an array or object node.
A chunk range MUST be contained within the range of its parent node.

Array chunks have two kinds:

- `items`: a contiguous group of array items.
- `item`: exactly one array item.

Object chunks have two kinds:

- `fields`: a contiguous group of object members.
- `field`: exactly one object member value.

Chunk ranges MUST start and end on valid JSON boundaries. Writers MUST NOT split
strings, numbers, literals, object member names, object member values, array
items, or structural tokens.

Chunk ranges MAY include separators needed to parse a fetched fragment after
wrapping, such as commas between covered array items or object members. Chunk
ranges MUST NOT include a separator that would make the wrapped fragment invalid
JSON, including a trailing comma after the last covered item or member. When a
chunk includes separators, the next chunk MUST NOT also claim those bytes. Chunk
ranges MUST NOT include the outer brackets or braces of their parent array or
object.

## Array Chunks

An `items` chunk covers a contiguous inclusive ordinal interval:

```json
{
  "kind": "items",
  "items": [0, 812],
  "range": [1025, 8390011],
  "checksum": "..."
}
```

`items[0]` is the first zero-based ordinal. `items[1]` is the last zero-based
ordinal. `items[0]` MUST be less than or equal to `items[1]`.

The range of an `items` chunk MUST contain complete item values and any
separators between those items. It MUST NOT contain the parent array's outer
`[` or `]`. Readers parse the fetched bytes by wrapping them as:

```text
[<range-bytes>]
```

An `item` chunk covers exactly one array item:

```json
{
  "kind": "item",
  "ordinal": 42,
  "range": [8390012, 16780000],
  "checksum": "...",
  "chunks": [
    {
      "kind": "fields",
      "fields": ["id", "name"],
      "range": [8390013, 12000000],
      "checksum": "..."
    }
  ]
}
```

`ordinal` identifies the zero-based item ordinal. The range MUST cover the
complete serialized item value. If the item value is an object, the `item` chunk
MAY include nested `chunks` containing only `ObjectChunk` entries. If the item
value is an array, the `item` chunk MAY include nested `chunks` containing only
`ArrayChunk` entries. Nested `chunks` MUST NOT mix chunk families. Primitive
item values MUST NOT have nested `chunks`.

Within an array node, chunks MUST be ordered by ascending ordinal coverage. The
ordinal coverage of chunks MUST NOT overlap. A reader can find ordinal `n` by
binary-searching chunk starts and then checking whether the matching chunk is an
`items` interval containing `n` or an `item` with `ordinal == n`.

Version 1 does not require chunks to cover every item in the parent array. If an
ordinal is not covered by any chunk, readers MUST fall back to a containing node
range if one is available; otherwise the requested path is not
range-addressable through the index.

## Object Field Chunks

A `fields` chunk covers a contiguous group of object members in serialized
object order:

```json
{
  "kind": "fields",
  "fields": ["id", "metadata", "prompt"],
  "range": [2, 2450000],
  "checksum": "..."
}
```

The range of a `fields` chunk MUST contain complete object members and any
separators between those members. It MUST NOT contain the parent object's outer
`{` or `}`. Readers parse the fetched bytes by wrapping them as:

```text
{<range-bytes>}
```

`fields` lists the member names covered by the chunk, or indexes into an
applicable `field_table`. Field membership is chunk metadata, not a secondary
index. Entries in one `fields` array MUST be all strings or all integers.
Integer entries are only valid when nested under an array node that defines
`field_table`; otherwise the entries MUST be literal strings.

A `field` chunk covers exactly one object member value:

```json
{
  "kind": "field",
  "name": "trace",
  "range": [2450002, 8120000],
  "checksum": "...",
  "chunks": [
    {
      "kind": "items",
      "items": [0, 99],
      "range": [2450003, 5000000],
      "checksum": "..."
    }
  ]
}
```

`name` identifies the field name, or an index into an applicable `field_table`.
Integer values are only valid when nested under an array node that defines
`field_table`; otherwise `name` MUST be a literal string.
The range MUST cover the field value only, excluding the member name, colon, and
neighboring separators. If the field value is an object, the `field` chunk MAY
include nested `chunks` containing only `ObjectChunk` entries. If the field
value is an array, the `field` chunk MAY include nested `chunks` containing only
`ArrayChunk` entries. Nested `chunks` MUST NOT mix chunk families. Primitive
field values MUST NOT have nested `chunks`.

Within an object node, chunks MUST be ordered by serialized field order. Field
coverage of chunks MUST NOT overlap. Version 1 does not require chunks to cover
every field in the parent object.

## Field Tables

An array node MAY define `field_table` to compress repeated field names in arrays
of object records:

```json
{
  "type": "array",
  "range": [1000, 900000000],
  "field_table": [
    "id",
    "metadata",
    "prompt",
    "messages",
    "artifacts",
    "metrics"
  ],
  "chunks": [
    {
      "kind": "item",
      "ordinal": 42,
      "range": [500001, 2450000],
      "checksum": "...",
      "chunks": [
        {
          "kind": "fields",
          "fields": [0, 1, 2],
          "range": [500002, 1200000],
          "checksum": "..."
        }
      ]
    }
  ]
}
```

When `field_table` is present on an array node, nested `fields.fields` entries
and `field.name` values inside that array's item chunks MAY use integer indexes
into the field table. A field table index MUST be non-negative and less than the
length of `field_table`.

Integer field table references and literal string field names MUST NOT be mixed
within a single `fields` array. A `field.name` value is either one integer
field table reference or one literal string.

`field_table` applies only to chunks nested under the array node that declares
it. It does not apply globally to unrelated object nodes unless a future version
explicitly defines such behavior.

## Compression

JRIF ranges are defined over the payload byte stream, not over any compressed
representation used by storage.

Storage MAY use transparent chunked compression when range reads return the same
payload bytes and byte offsets defined by the index. Ranges and checksums MUST
refer to the payload byte stream exposed to JRIF readers.

Ordinary single-stream `.json.gz` is incompatible with arbitrary byte-range
decoding unless a storage layer exposes transparent range reads over the
decompressed payload byte stream.

## Writer Requirements

A conforming version 1 writer MUST:

- Parse JSON bytes.
- Record byte offsets into the exact payload byte stream.
- Emit ranges only at valid JSON value, member, item, or fragment boundaries.
- Ensure every emitted range is within the payload byte stream size.
- Ensure node ranges cover complete JSON values.
- Ensure chunk ranges are contained in parent node ranges.
- Compute checksums over exact payload byte ranges.
- Leave payload bytes unchanged after indexing.
- Order array chunks by ascending ordinal coverage.
- Order object chunks by serialized field order.
- Avoid overlapping chunk coverage within the same parent.

## Reader Requirements

A conforming version 1 reader MUST:

- Validate `$schema` and reject unsupported `$schema` values.
- Reject unknown fields.
- Validate the payload byte stream size before using any range.
- Reject ranges where `start > end` or `end` is greater than or equal to the
  payload byte stream size.
- Verify range checksums before parsing or returning fetched bytes.
- Wrap `items` and `fields` fragments before parsing.
- Reject malformed fragment parses.

A conforming version 1 reader SHOULD:

- Coalesce nearby ranges when that reduces storage requests.
- Cache fetched and verified chunks.
- Prefer the smallest available verified range that satisfies a request.
- Fall back to containing chunks when exact metadata is absent.
