# JSON Range Index Format

Status: draft

JSON Range Index Format (JRIF) is a JSON sidecar format for partial loading of
large JSON documents from byte-addressable storage, including object stores that
support ranged GET. The indexed payload remains ordinary RFC 8259 JSON encoded
as UTF-8 bytes.

This document is the normative format specification for JRIF draft version.
The JSON Schema appendix defines the syntactic shape of a JRIF index
document. Some requirements in this document are semantic constraints that
cannot be fully expressed in JSON Schema.

## Conformance Language

The key words `MUST`, `MUST NOT`, `REQUIRED`, `SHOULD`, `SHOULD NOT`, and `MAY`
are to be interpreted as described in RFC 2119 and RFC 8174 when, and only when,
they appear in uppercase.

## File Set

A JRIF index is stored next to the JSON payload it describes. The pairing of
payload and index is a deployment concern; this specification does not
mandate a file extension or media type. The draft version does not define
secondary index files, external chunk catalogs, or multi-file indexes.

A JRIF index describes one immutable payload byte stream. A writer MUST
compute the index from the exact payload bytes that readers will later
receive. If the payload bytes change, the existing index MUST be considered
invalid.

## Design Goals

The draft version is designed to:

- Preserve ordinary JSON payloads.
- Map selected JSON paths to byte ranges.
- Chunk large arrays by item ordinal.
- Chunk large objects by field groups.
- Support arrays of large object records.

The draft version intentionally does not define:

- A new JSON syntax.
- Search, predicate, inverted, or secondary indexes.
- In-place mutation.
- Efficient access to every scalar by default.
- Arbitrary range loading of ordinary single-stream `.json.gz` files.
- Integrity verification of fetched ranges; payload integrity is the storage
  layer's responsibility.

## Wire Field Names

The on-wire format uses short keys to keep indexes small. The normative key
names are:

| Where                     | Key                | Meaning                                                                                           |
| ------------------------- | ------------------ | ------------------------------------------------------------------------------------------------- |
| Document                  | `jrif`             | Format version tag.                                                                               |
| Document                  | `meta`             | Optional record-keeping metadata.                                                                 |
| Document                  | `keys`             | Document-level key dictionary.                                                                    |
| Document                  | `root`             | Root `Value`.                                                                                     |
| `Value`                   | `t`                | Type tag: `v`, `s`, `a`, or `o`.                                                                  |
| `Value`                   | `v`                | Inline literal (when `t` is `v`).                                                                 |
| `Value`                   | `r`                | Byte range (when `t` is `s`, `a`, or `o`).                                                        |
| `Value`                   | `c`                | Chunks list (optional, when `t` is `a` or `o`).                                                   |
| Chunk                     | `k`                | Kind tag: `is`, `i`, `fs`, or `f`.                                                                |
| Chunk                     | `r`                | Byte range (always present on `is` and `fs`; on `i`/`f` only when the wrapped `Value` is ranged). |
| Items chunk (`k` = `is`)  | `n`                | Item count (positive integer).                                                                    |
| Fields chunk (`k` = `fs`) | `f`                | Member-name indices (non-empty array of integers into `keys`).                                    |
| Field chunk (`k` = `f`)   | `n`                | Member-name index (non-negative integer into `keys`).                                             |
| Field chunk (`k` = `f`)   | `t`, `v`, `r`, `c` | Flattened `Value` fields (see §Values, §Object Field Chunks).                                     |
| Item chunk (`k` = `i`)    | `t`, `v`, `r`, `c` | Flattened `Value` fields (see §Values, §Array Chunks).                                            |

The `n` key always carries a non-negative integer but its meaning varies by
chunk kind (item count under `is`, member-name index into `keys` under `f`).
The two never co-occur in the same chunk because `k` selects the variant.

## Byte Ranges

All ranges use byte offsets into the payload byte stream, not Unicode scalar,
character, or line offsets.

A range is a two-element JSON array:

```json
[start, length]
```

`start` is the zero-based absolute byte offset of the first covered byte in
the payload byte stream. `length` is the number of bytes covered by the
range. The covered byte sequence is:

```text
payload[start : start + length]
```

For every range, `start` and `length` MUST be non-negative integers and
`length` MUST be at least 1. `start + length` MUST NOT overflow and MUST be
less than or equal to the payload byte stream size.

Empty payload fragments are not representable as ranges in the draft version.

Inline values (see §Values) carry no `r`; they are materialized directly in
the JRIF document.

## Top-Level Document

A JRIF document has this top-level shape:

```json
{
  "jrif": "v0",
  "meta": {
    "size": 982341234,
    "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
  },
  "keys": ["id", "name", "ts"],
  "root": {
    "t": "o",
    "r": [0, 982341234]
  }
}
```

Top-level fields:

- `jrif`: MUST be `"v0"`.
- `meta`: OPTIONAL JSON object carrying record-keeping metadata about the
  indexed payload, such as its original byte size or a content digest. The
  draft version does not normatively define any inner field names; `meta` is
  free-form and is intended for human or tooling consumption, not for
  navigation. Readers MUST NOT use any value under `meta` to drive range
  decoding, member resolution, or payload integrity decisions; readers MAY
  surface its contents to callers. Suggested non-normative inner fields
  include `size` (the payload byte size as a non-negative integer) and
  `sha256` (the hex-encoded SHA-256 digest of the payload bytes as a
  lowercase string).
- `keys`: document-level key dictionary. When present, it is a non-empty array
  of unique strings. `keys` is REQUIRED whenever the document contains at
  least one fields chunk (see §Object Field Chunks) or at least one field
  chunk (see §Object Field Chunks), and MAY be omitted otherwise. Every member
  name referenced by any fields or field chunk MUST appear in `keys`.
- `root`: a `Value` describing the root JSON value.

Readers MUST reject unknown `jrif` values.

## Values

A `Value` is a tagged object describing a JSON value. The `t` field is the
single discriminator and is REQUIRED. It MUST be one of:

```text
v s a o
```

`t` selects the variant:

- `t: "v"` — an **inline** value. The `v` field carries an arbitrary RFC 8259
  JSON literal (any of `null`, boolean, number, string, array, or object) that
  the JRIF document materializes directly. Inline values MUST NOT carry `r` or
  `c`.
- `t: "s"` — a **ranged string**. The serialized JSON string lives in the
  payload byte stream at `r`. MUST NOT carry `v` or `c`.
- `t: "a"` — a **ranged array**. The serialized JSON array lives in the
  payload byte stream at `r`. MAY carry `c` (see §Chunks). MUST NOT carry `v`.
- `t: "o"` — a **ranged object**. The serialized JSON object lives in the
  payload byte stream at `r`. MAY carry `c` (see §Chunks). MUST NOT carry `v`.

The choice between `t: "v"` (inline) and `t: "s" | "a" | "o"` (ranged) is a
writer's choice. Writers MAY inline any JSON value of any size and any type
when the inline form is cheaper or simpler than the ranged form, but SHOULD
avoid inlining large arrays or objects unless there is a clear space or
simplicity benefit. The exact threshold is non-normative.

Inline values are opaque to JRIF descent: a `t: "v"` node has no chunks, no
range, and no partial-coverage semantics. Readers materialize the inline
literal directly as the JSON value at that position.

The `r` of a ranged value MUST cover the complete serialized JSON value, from
the first byte of the value's opening token to the last byte of its closing
token, inclusive.

Examples:

```json
{ "t": "v", "v": null }
{ "t": "v", "v": true }
{ "t": "v", "v": 42 }
{ "t": "v", "v": "ok" }
{ "t": "v", "v": [] }
{ "t": "v", "v": {} }
{ "t": "v", "v": { "id": 1, "name": "small" } }

{ "t": "s", "r": [100, 924] }
{
  "t": "a",
  "r": [0, 982341234],
  "c": [ ... ]
}
{
  "t": "o",
  "r": [0, 982341234],
  "c": [ ... ]
}
```

A writer SHOULD prefer the ranged form for any value large enough to benefit
from partial loading, and SHOULD prefer the inline form for small values
where embedding is cheaper than a ranged fetch.

## Chunks

Chunks describe independently loadable fragments inside a ranged array or
object `Value`. Inline values (`t: "v"`) and ranged strings (`t: "s"`) do not
carry chunks. A chunk range MUST be contained within the range of its parent
ranged `Value`.

Array chunks have two kinds:

- `k: "is"`: a contiguous group of array items.
- `k: "i"`: exactly one array item, wrapping its `Value`.

Object chunks have two kinds:

- `k: "fs"`: a contiguous group of object members.
- `k: "f"`: exactly one object member, wrapping its `Value`.

A chunk range MUST start and end on a valid JSON token boundary. A token
boundary is a byte position that is not inside a string literal (including any
`\` escape sequence), not inside a number literal, and not inside one of the
keyword literals `true`, `false`, or `null`. Writers MUST NOT split strings,
numbers, literals, object member names, object member values, or array items.

Chunk ranges MAY include the structural commas that separate covered items or
members, and MAY include insignificant whitespace (spaces, horizontal tab,
line feed, carriage return) before, between, or after the covered values and
commas, provided that the requirement below is met. Chunk ranges MUST NOT
include a trailing comma after the last covered item or member, and MUST NOT
include the outer brackets or braces of their parent array or object. When a
chunk includes a separator, the next sibling chunk MUST NOT also claim those
bytes.

For a covering chunk over a parent array `Value`, the wrapped fragment
`[<range-bytes>]` MUST be valid JSON; for a covering chunk over a parent
object `Value`, the wrapped fragment `{<range-bytes>}` MUST be valid JSON.
This constraint subsumes the whitespace and separator rules above and is the
authoritative test of a well-formed chunk range.

Single-value chunks (`k: "i"` and `k: "f"`) carry the wrapped `Value`'s fields
flattened alongside the chunk's `k` (and `n` for the member-name index, on `k: "f"`).
The chunk-level `k` and value-level `t` discriminators live on the same object
but never collide; see the sections below for the precise rules.

## Array Chunks

A `k: "is"` chunk covers a contiguous run of array items:

```json
{
  "k": "is",
  "n": 813,
  "r": [1025, 8388987]
}
```

`n` is the number of items covered by the chunk and MUST be a positive
integer. `r` is REQUIRED on `k: "is"` chunks.

The range of a `k: "is"` chunk MUST contain complete item values and any
separators between those items. It MUST NOT contain the parent array's outer
`[` or `]`. Readers parse the fetched bytes by wrapping them as:

```text
[<range-bytes>]
```

A `k: "i"` chunk covers exactly one array item by carrying the item's `Value`
fields flattened alongside `k`:

```json
{
  "k": "i",
  "t": "o",
  "r": [8390012, 8389989],
  "c": [
    {
      "k": "fs",
      "f": [0, 1],
      "r": [8390013, 3609988]
    }
  ]
}
```

```json
{ "k": "i", "t": "v", "v": 42 }
```

The wrapped `Value`'s `t`, plus its `v` (for inline) or `r`/`c` (for ranged),
appear directly on the chunk object. The chunk-level `k` and value-level `t`
discriminators are distinct keys and never collide. The wrapped `Value`
follows the rules in §Values. When the wrapped `Value` is `t: "a"`, its `c`
MAY contain only array chunk entries (`k: "is"` and `k: "i"`). When it is
`t: "o"`, its `c` MAY contain only object chunk entries (`k: "fs"` and
`k: "f"`).

Item ordinals are not stored on chunks. The ordinal of the first item covered
by a chunk is the sum of the item counts of all preceding chunks in the
parent array (each `k: "i"` chunk contributes 1; each `k: "is"` chunk
contributes its `n`). The first chunk in a parent array therefore starts at
ordinal 0.

Within a ranged array `Value`, chunks MUST appear in ordinal order and MUST
contiguously cover every item from ordinal 0 to the last item present. Gaps
and trailing uncovered items are not representable in the draft version;
either chunk an array completely or omit `c` entirely and let readers fall
back to the parent array's `r`.

## Object Field Chunks

A `k: "fs"` chunk covers a contiguous group of object members in serialized
object order. Each `k: "fs"` chunk MUST encode the covered member names as a
non-empty array of non-negative integers in its `f` field. Each integer is an
index into the document-level `keys` table (see §Top-Level Document).

```json
{
  "k": "fs",
  "f": [0, 4, 7],
  "r": [2, 2449999]
}
```

The range of a `k: "fs"` chunk MUST contain complete object members and any
separators between those members. It MUST NOT contain the parent object's outer
`{` or `}`. Readers parse the fetched bytes by wrapping them as:

```text
{<range-bytes>}
```

`f` lists the member names covered by the chunk, resolved through the
document-level `keys` table. Field membership is chunk metadata, intended for
chunk selection by name; the draft version does not define separate inverted
or secondary indexes.

Writers SHOULD bound the size of each `k: "fs"` chunk by both a byte cap and
a member-count cap. When a contiguous run of object members would exceed
either cap, writers SHOULD split that run into multiple `k: "fs"` chunks at
member boundaries. Specific cap values are non-normative and are a writer's
choice. The intent is to bound read amplification when readers fetch a
`k: "fs"` chunk to resolve a single member.

A `k: "f"` chunk covers exactly one object member by carrying the wrapped
`Value`'s fields flattened alongside `k` and the member-name index `n`:

```json
{
  "k": "f",
  "n": 3,
  "t": "a",
  "r": [2450002, 5669999],
  "c": [
    {
      "k": "is",
      "n": 100,
      "r": [2450003, 2549998]
    }
  ]
}
```

```json
{ "k": "f", "n": 0, "t": "v", "v": "abc-123" }
```

`n` MUST be a non-negative integer index into the document-level `keys` table
(see §Top-Level Document) and MUST be a valid offset into `keys`. The wrapped
`Value`'s `t`, plus its `v` (for inline) or `r`/`c` (for ranged), appear
directly on the chunk object. When the wrapped `Value` is ranged, its `r`
MUST cover the field value only, excluding the member name, colon, and
neighboring separators. The wrapped `Value` follows the rules in §Values.

Within a ranged object `Value`, chunks MUST be ordered by serialized field
order. Field coverage of chunks MUST NOT overlap. The draft version does not
require chunks to cover every field in the parent object; see §Partial
Coverage for how readers resolve members not directly covered by a chunk.

Payloads containing duplicate object member names are valid JSON but the
behavior of chunk lookups by name follows serialized order: writers MAY index
only the first matching occurrence or the last matching occurrence, but
readers MUST resolve by the first matching chunk in serialized order. Writers
SHOULD NOT index payloads that contain duplicate member names within the same
object.

## Partial Coverage

A ranged object `Value` MAY have `c` that does not cover every member of the
parent object, MAY have empty `c`, or MAY omit `c` entirely. Readers MUST
resolve a member name lookup using the following procedure, in order:

1. If the name matches a `k: "f"` chunk (its `n` resolved through the
   document-level `keys` table equals the requested name) in the parent's
   `c`, the reader MUST use the `Value` wrapped by the first such chunk in
   serialized order.
2. Else if the name appears in the covered names of some `k: "fs"` chunk in
   the parent's `c` — resolved by indexing the document-level `keys` table
   with each integer in the chunk's `f` array — the reader MUST fetch the
   first such chunk in serialized order, parse the wrapped fragment as defined
   in §Object Field Chunks, and resolve the member from the parsed fragment.
3. Otherwise, the reader MUST fall back to the parent object's `r`, fetch
   it, parse the wrapped JSON object, and resolve the member from the parsed
   object.

This procedure is the authoritative semantics for member resolution. It also
applies when an object `Value` has no `c` at all: step 3 applies directly.
Array `Value`s do not permit partial coverage (see §Array Chunks): their
chunks either cover every item contiguously from ordinal 0 or are absent
entirely.

Inline values (`t: "v"`) terminate JRIF descent. Readers MUST NOT attempt
chunk lookup or range fallback on an inline value; the materialized JSON
literal under `v` is the value at that position.

## Compression

JRIF ranges are defined over the payload byte stream, not over any compressed
representation used by storage.

Storage MAY use transparent chunked compression when range reads return the
same payload bytes and byte offsets defined by the index. Ranges MUST refer
to the payload byte stream exposed to JRIF readers.

Ordinary single-stream `.json.gz` is incompatible with arbitrary byte-range
decoding unless a storage layer exposes transparent range reads over the
decompressed payload byte stream.

## Writer Requirements

A conforming draft version writer MUST:

- Parse JSON bytes.
- Record byte offsets into the exact payload byte stream.
- Emit ranges only at valid JSON value, member, item, or fragment boundaries.
- Ensure every emitted range is within the payload byte stream size.
- Ensure ranged `Value` ranges cover complete JSON values.
- Ensure chunk ranges are contained in their parent ranged `Value`'s range.
- Leave payload bytes unchanged after indexing.
- Order array chunks so their coverage runs contiguously from ordinal 0 with no
  gaps.
- Order object chunks by serialized field order.
- Avoid overlapping chunk coverage within the same parent.
- Tag every `Value` with `t`. Emit inline values as `t: "v"` with a `v` field
  and no `r`/`c`. Emit ranged values as `t: "s" | "a" | "o"` with `r`, plus
  `c` for arrays and objects when present.
- Emit a document-level `keys` array whenever the document contains at least
  one `k: "fs"` or `k: "f"` chunk. Every member name referenced by any
  `k: "fs"` chunk's `f` array or by any `k: "f"` chunk's `n` MUST be a valid
  offset into `keys`.

A conforming draft version writer SHOULD:

- Bound each `k: "fs"` chunk by both a byte cap and a member-count cap,
  splitting overlong scalar runs into multiple `k: "fs"` chunks at member
  boundaries. Specific cap values are non-normative.
- Omit `keys` when the document contains no `k: "fs"` or `k: "f"` chunks.
- Prefer inline `t: "v"` for values where embedding is cheaper than a ranged
  fetch, and ranged form for values large enough to benefit from partial
  loading.

## Reader Requirements

A conforming draft version reader MUST:

- Validate `jrif` and reject unsupported `jrif` values.
- Reject unknown fields.
- Validate the payload byte stream size before using any range.
- Reject ranges where `length` is zero, where `start + length` overflows a
  64-bit unsigned integer, or where `start + length` is greater than the
  payload byte stream size.
- Wrap `k: "is"` and `k: "fs"` fragments before parsing.
- Reject malformed fragment parses.
- Reject any `Value` whose fields do not match its `t` tag (e.g., `t: "v"`
  carrying `r`, or `t: "s"` carrying `c`).
- Reject any `k: "fs"` chunk whose `f` array is empty.
- Reject any `k: "fs"` or `k: "f"` chunk in a document that has no
  document-level `keys` table.
- Reject any integer in a `k: "fs"` chunk's `f` array, or any `k: "f"`
  chunk's `n`, that is not a valid offset into the document-level `keys`
  table.
- Treat inline values (`t: "v"`) as terminal: materialize the literal under
  `v` directly without further JRIF descent.
- Resolve object member lookups using the procedure in §Partial Coverage.

A conforming draft version reader SHOULD:

- Coalesce nearby ranges when that reduces storage requests.
- Cache fetched chunks.
- Prefer the smallest available range that satisfies a request.
