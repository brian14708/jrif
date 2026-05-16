# JRIF

**JSON Range Index Format** — read parts of a large JSON document without
downloading the whole file. JRIF maps JSON paths to byte ranges so clients
can issue ranged GETs against byte-addressable storage like object stores.

The indexed payload remains ordinary [RFC 8259](https://www.rfc-editor.org/rfc/rfc8259)
JSON; the `.jrif` file lives alongside it and carries the index.

## Where to go next

- **[Specification](spec.md)** — the normative format spec for JRIF v1.
- **[JSON Schema (v1)](v1/schema.json)** — machine-readable schema for `.jrif`
  files.

## Status

Draft. The on-disk format is not yet stable.
