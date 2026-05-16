package jrif

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

// JrifV0 is the only `jrif` version tag accepted by this reader.
const JrifV0 = "v0"

// ValueType is the discriminator for [Value.Type].
type ValueType string

const (
	// ValueValue is the inline form. The Value carries an arbitrary JSON
	// literal in [Value.Inline] and no range/chunks.
	ValueValue ValueType = "v"
	// ValueString, ValueArray, ValueObject are the ranged forms. The Value
	// carries Range (and optional chunks for arrays/objects).
	ValueString ValueType = "s"
	ValueArray  ValueType = "a"
	ValueObject ValueType = "o"
)

// ArrayChunkKind is the discriminator for [ArrayChunk.Kind].
type ArrayChunkKind string

const (
	ArrayChunkItems ArrayChunkKind = "is"
	ArrayChunkItem  ArrayChunkKind = "i"
)

// ObjectChunkKind is the discriminator for [ObjectChunk.Kind].
type ObjectChunkKind string

const (
	ObjectChunkFields ObjectChunkKind = "fs"
	ObjectChunkField  ObjectChunkKind = "f"
)

// Range is an absolute [start, length] byte range into the payload. start
// is the zero-based offset of the first covered byte; length is the byte
// count and MUST be >= 1 for a well-formed v0 range. The covered bytes are
// payload[start : start+length].
type Range [2]uint64

// Start returns r's absolute starting byte offset.
func (r Range) Start() uint64 { return r[0] }

// Length returns the number of bytes covered by r. Always >= 1 for a
// well-formed v0 range.
func (r Range) Length() uint64 { return r[1] }

// End returns the exclusive end byte offset (Start + Length). Callers using
// it as a slice upper bound (payload[Start():End()]) must have already
// validated that Start + Length does not overflow.
func (r Range) End() uint64 { return r[0] + r[1] }

// Len returns the number of bytes covered by r.
func (r Range) Len() uint64 { return r[1] }

// Contains reports whether child is fully contained in r. Both ranges must
// be well-formed; the caller is expected to have validated them.
func (r Range) Contains(child Range) bool {
	return r[0] <= child[0] && child[0]+child[1] <= r[0]+r[1]
}

// Document is the top-level shape of a .jrif file.
type Document struct {
	Jrif string
	// Meta is the optional document-level metadata object recording
	// information about the indexed payload (e.g. original size, content
	// digest). It is free-form and never used to drive navigation; readers
	// MAY surface its contents to callers. Omitted (nil/empty) when no
	// metadata is recorded.
	Meta map[string]json.RawMessage
	// Keys is the document-level key dictionary. REQUIRED whenever the
	// document contains at least one Fields or Field chunk; every Fields
	// chunk's integer entries and every Field chunk's Name are offsets into
	// Keys. Omitted (empty) when no such chunks exist.
	Keys []string
	Root Value
}

type rawDocument struct {
	Jrif *json.RawMessage           `json:"jrif"`
	Meta map[string]json.RawMessage `json:"meta"`
	Keys []string                   `json:"keys"`
	Root *json.RawMessage           `json:"root"`
}

// UnmarshalJSON parses a JRIF document, rejecting unknown top-level fields.
func (d *Document) UnmarshalJSON(data []byte) error {
	var raw rawDocument
	if err := strictUnmarshal(data, &raw); err != nil {
		return fmt.Errorf("document: %w", err)
	}
	if raw.Jrif == nil {
		return fmt.Errorf("document: missing jrif")
	}
	if raw.Root == nil {
		return fmt.Errorf("document: missing root")
	}
	if err := json.Unmarshal(*raw.Jrif, &d.Jrif); err != nil {
		return fmt.Errorf("document.jrif: %w", err)
	}
	d.Meta = raw.Meta
	d.Keys = raw.Keys
	if err := d.Root.UnmarshalJSON(*raw.Root); err != nil {
		return fmt.Errorf("document.root: %w", err)
	}
	return nil
}

// MarshalJSON encodes the document with jrif (always), meta (when
// non-empty), keys (when non-empty), and root.
func (d Document) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Jrif string                     `json:"jrif"`
		Meta map[string]json.RawMessage `json:"meta,omitempty"`
		Keys []string                   `json:"keys,omitempty"`
		Root Value                      `json:"root"`
	}{d.Jrif, d.Meta, d.Keys, d.Root})
}

// Value describes a JSON value as represented in the JRIF sidecar. The Type
// tag is the single discriminator:
//
//   - Type == ValueValue: inline form. Inline carries the raw JSON bytes of
//     an arbitrary JSON literal (null, bool, number, string, array, or
//     object). Range/Chunks are unused.
//   - Type == ValueString | ValueArray | ValueObject: ranged form. Range
//     points into the payload byte stream; ArrayChunks/ObjectChunks may be
//     populated for non-trivial arrays/objects. Inline is nil.
type Value struct {
	Type ValueType
	// Inline carries the raw JSON of the inline value when Type == ValueValue,
	// or nil for ranged values.
	Inline       json.RawMessage
	Range        Range
	ArrayChunks  []ArrayChunk
	ObjectChunks []ObjectChunk

	// arrCum is an internal lookup table populated by [populateChunkCaches]
	// (called from [OpenDocument]) for arrays with non-empty ArrayChunks.
	// arrCum[i] is the running ordinal at the start of ArrayChunks[i];
	// arrCum[len(ArrayChunks)] is the array's total length. Empty for inline
	// or unchunked values.
	arrCum    []uint64
	objLookup map[string]objectHit
}

// IsInline reports whether this Value is in inline form.
func (v *Value) IsInline() bool { return v.Type == ValueValue }

// valueFields captures every JSON key that may appear on a Value, plus the
// extra chunk-discriminator keys when a Value is flattened onto an Item or
// Field chunk. `Value` is non-pointer so that a JSON `"v": null` is
// preserved as the literal bytes `null` rather than collapsing to the
// "absent" case.
type valueFields struct {
	Type   ValueType         `json:"t"`
	Value  json.RawMessage   `json:"v"`
	Range  *Range            `json:"r"`
	Chunks []json.RawMessage `json:"c,omitempty"`
}

// fillValueFromFields populates v from the parsed common fields. Returns an
// error for any combination of fields that doesn't match the Type tag.
func fillValueFromFields(v *Value, f valueFields) error {
	switch f.Type {
	case ValueValue:
		if len(f.Value) == 0 {
			return fmt.Errorf("value(value): missing `value`")
		}
		if f.Range != nil || f.Chunks != nil {
			return fmt.Errorf("value(value): inline must not carry range/chunks")
		}
		// Validate the inline payload is well-formed JSON (any literal is allowed).
		var probe any
		dec := json.NewDecoder(bytes.NewReader(f.Value))
		dec.UseNumber()
		if err := dec.Decode(&probe); err != nil {
			return fmt.Errorf("value(value): %w", err)
		}
		if err := assertNoTrailingJSON(dec); err != nil {
			return fmt.Errorf("value(value): %w", err)
		}
		*v = Value{Type: ValueValue, Inline: f.Value}
		return nil
	case ValueString:
		return fillRangedFields(v, f, ValueString, false)
	case ValueArray:
		return fillRangedFields(v, f, ValueArray, true)
	case ValueObject:
		return fillRangedFields(v, f, ValueObject, true)
	default:
		return fmt.Errorf("value: unknown type %q", f.Type)
	}
}

// fillRangedFields populates v as a ranged Value (string/array/object).
// allowChunks gates the array/object chunk paths; ranged strings reject
// chunks.
func fillRangedFields(v *Value, f valueFields, typ ValueType, allowChunks bool) error {
	if len(f.Value) != 0 {
		return fmt.Errorf("value(%s): ranged form must not carry inline `value`", typ)
	}
	if f.Range == nil {
		return fmt.Errorf("value(%s): ranged form requires range", typ)
	}
	if !allowChunks && f.Chunks != nil {
		return fmt.Errorf("value(%s): must not carry chunks", typ)
	}
	out := Value{Type: typ, Range: *f.Range}
	if allowChunks && len(f.Chunks) > 0 {
		switch typ {
		case ValueArray:
			chunks := make([]ArrayChunk, len(f.Chunks))
			for i, c := range f.Chunks {
				if err := chunks[i].UnmarshalJSON(c); err != nil {
					return fmt.Errorf("value.chunks[%d]: %w", i, err)
				}
			}
			out.ArrayChunks = chunks
		case ValueObject:
			chunks := make([]ObjectChunk, len(f.Chunks))
			for i, c := range f.Chunks {
				if err := chunks[i].UnmarshalJSON(c); err != nil {
					return fmt.Errorf("value.chunks[%d]: %w", i, err)
				}
			}
			out.ObjectChunks = chunks
		}
	}
	*v = out
	return nil
}

// UnmarshalJSON decodes a value, discriminating on the "type" field.
func (v *Value) UnmarshalJSON(data []byte) error {
	var f valueFields
	if err := strictUnmarshal(data, &f); err != nil {
		return fmt.Errorf("value: %w", err)
	}
	return fillValueFromFields(v, f)
}

// MarshalJSON encodes a value according to its Type.
func (v Value) MarshalJSON() ([]byte, error) {
	switch v.Type {
	case ValueValue:
		return json.Marshal(struct {
			Type  ValueType       `json:"t"`
			Value json.RawMessage `json:"v"`
		}{ValueValue, v.Inline})
	case ValueString:
		return json.Marshal(struct {
			Type  ValueType `json:"t"`
			Range Range     `json:"r"`
		}{ValueString, v.Range})
	case ValueArray:
		return json.Marshal(struct {
			Type   ValueType    `json:"t"`
			Range  Range        `json:"r"`
			Chunks []ArrayChunk `json:"c,omitempty"`
		}{ValueArray, v.Range, v.ArrayChunks})
	case ValueObject:
		return json.Marshal(struct {
			Type   ValueType     `json:"t"`
			Range  Range         `json:"r"`
			Chunks []ObjectChunk `json:"c,omitempty"`
		}{ValueObject, v.Range, v.ObjectChunks})
	default:
		return nil, fmt.Errorf("value: unknown type %q", v.Type)
	}
}

// ArrayChunk is one entry in an array value's chunks list. Kind selects
// between a contiguous run of items ([ArrayChunkItems], with Count) and
// exactly one item ([ArrayChunkItem], wrapping a Value flattened onto the
// chunk object).
type ArrayChunk struct {
	Kind ArrayChunkKind
	// Items kind:
	Count uint64
	Range Range
	// Item kind: the wrapped item value. The wrapped Value's fields appear
	// flattened alongside Kind on the wire.
	Value *Value
}

type rawItemsChunk struct {
	Kind  ArrayChunkKind `json:"k"`
	Count *uint64        `json:"n"`
	Range *Range         `json:"r"`
}

type rawChunkKind struct {
	Kind string `json:"k"`
}

// UnmarshalJSON decodes an array chunk, discriminating on "kind".
func (c *ArrayChunk) UnmarshalJSON(data []byte) error {
	var hdr rawChunkKind
	if err := json.Unmarshal(data, &hdr); err != nil {
		return fmt.Errorf("array chunk: %w", err)
	}
	switch ArrayChunkKind(hdr.Kind) {
	case ArrayChunkItems:
		var raw rawItemsChunk
		if err := strictUnmarshal(data, &raw); err != nil {
			return fmt.Errorf("array chunk: %w", err)
		}
		if raw.Range == nil {
			return fmt.Errorf("array chunk: missing range")
		}
		if raw.Count == nil || *raw.Count == 0 {
			return fmt.Errorf("array chunk: items count must be > 0")
		}
		*c = ArrayChunk{
			Kind:  ArrayChunkItems,
			Count: *raw.Count,
			Range: *raw.Range,
		}
		return nil
	case ArrayChunkItem:
		var rawAll itemFlat
		if err := strictUnmarshal(data, &rawAll); err != nil {
			return fmt.Errorf("array chunk: %w", err)
		}
		var v Value
		if err := fillValueFromFields(&v, rawAll.valueFields); err != nil {
			return fmt.Errorf("array chunk: %w", err)
		}
		*c = ArrayChunk{Kind: ArrayChunkItem, Range: v.Range, Value: &v}
		return nil
	default:
		return fmt.Errorf("array chunk: unknown kind %q", hdr.Kind)
	}
}

// itemFlat is the on-wire shape of an item chunk: kind + value fields
// flattened.
type itemFlat struct {
	Kind ArrayChunkKind `json:"k"`
	valueFields
}

// MarshalJSON encodes an array chunk for the JRIF wire form.
func (c ArrayChunk) MarshalJSON() ([]byte, error) {
	switch c.Kind {
	case ArrayChunkItems:
		return json.Marshal(struct {
			Kind  ArrayChunkKind `json:"k"`
			Count uint64         `json:"n"`
			Range Range          `json:"r"`
		}{ArrayChunkItems, c.Count, c.Range})
	case ArrayChunkItem:
		if c.Value == nil {
			return nil, fmt.Errorf("array chunk: item missing value")
		}
		return marshalValueChunk(string(ArrayChunkItem), nil, c.Value)
	default:
		return nil, fmt.Errorf("array chunk: unknown kind %q", c.Kind)
	}
}

// ObjectChunk is one entry in an object value's chunks list. Kind selects
// between a contiguous group of fields ([ObjectChunkFields], with Fields as
// integer indices into the document-level Keys table) and exactly one field
// ([ObjectChunkField], with Name as an integer index into the document-level
// Keys table wrapping a Value flattened onto the chunk object).
type ObjectChunk struct {
	Kind ObjectChunkKind
	// Fields kind: non-empty integer indices into the document-level Keys
	// table naming the covered member names.
	Fields []uint32
	Range  Range
	// Field kind: integer index into the document-level Keys table naming
	// the covered member name.
	Name  uint32
	Value *Value
}

type rawFieldsChunk struct {
	Kind   ObjectChunkKind `json:"k"`
	Fields []uint32        `json:"f"`
	Range  *Range          `json:"r"`
}

// fieldFlat is the on-wire shape of a field chunk: kind + name + value fields
// flattened.
type fieldFlat struct {
	Kind ObjectChunkKind `json:"k"`
	Name *uint32         `json:"n"`
	valueFields
}

// UnmarshalJSON decodes an object chunk, discriminating on "kind".
func (c *ObjectChunk) UnmarshalJSON(data []byte) error {
	var hdr rawChunkKind
	if err := json.Unmarshal(data, &hdr); err != nil {
		return fmt.Errorf("object chunk: %w", err)
	}
	switch ObjectChunkKind(hdr.Kind) {
	case ObjectChunkFields:
		var raw rawFieldsChunk
		if err := strictUnmarshal(data, &raw); err != nil {
			return fmt.Errorf("object chunk: %w", err)
		}
		if raw.Range == nil {
			return fmt.Errorf("object chunk: missing range")
		}
		if len(raw.Fields) == 0 {
			return fmt.Errorf("object chunk: fields list must not be empty")
		}
		*c = ObjectChunk{
			Kind:   ObjectChunkFields,
			Fields: raw.Fields,
			Range:  *raw.Range,
		}
		return nil
	case ObjectChunkField:
		var raw fieldFlat
		if err := strictUnmarshal(data, &raw); err != nil {
			return fmt.Errorf("object chunk: %w", err)
		}
		if raw.Name == nil {
			return fmt.Errorf("object chunk: name required for kind=field")
		}
		var v Value
		if err := fillValueFromFields(&v, raw.valueFields); err != nil {
			return fmt.Errorf("object chunk: %w", err)
		}
		*c = ObjectChunk{
			Kind:  ObjectChunkField,
			Range: v.Range,
			Name:  *raw.Name,
			Value: &v,
		}
		return nil
	default:
		return fmt.Errorf("object chunk: unknown kind %q", hdr.Kind)
	}
}

// MarshalJSON encodes an object chunk for the JRIF wire form.
func (c ObjectChunk) MarshalJSON() ([]byte, error) {
	switch c.Kind {
	case ObjectChunkFields:
		if len(c.Fields) == 0 {
			return nil, fmt.Errorf("object chunk: fields list must not be empty")
		}
		return json.Marshal(struct {
			Kind   ObjectChunkKind `json:"k"`
			Fields []uint32        `json:"f"`
			Range  Range           `json:"r"`
		}{ObjectChunkFields, c.Fields, c.Range})
	case ObjectChunkField:
		if c.Value == nil {
			return nil, fmt.Errorf("object chunk: field missing value")
		}
		return marshalValueChunk(string(ObjectChunkField), &c.Name, c.Value)
	default:
		return nil, fmt.Errorf("object chunk: unknown kind %q", c.Kind)
	}
}

// marshalValueChunk emits a flat-shape Item/Field chunk: kind (+optional
// name) alongside the wrapped value's type and either value (inline) or
// range/chunks (ranged). The wrapped Value's JSON object is spliced onto a
// prefix carrying k and (for fields) n. Since the wrapped Value's fields
// (`t`, `v`, `r`, `c`) never collide with `k` or `n`, flattening is
// unambiguous.
func marshalValueChunk(kind string, name *uint32, v *Value) ([]byte, error) {
	valueBytes, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	if len(valueBytes) < 2 || valueBytes[0] != '{' {
		return nil, fmt.Errorf("value marshal produced non-object: %s", valueBytes)
	}
	var head []byte
	if name == nil {
		head = []byte(fmt.Sprintf(`{"k":%q,`, kind))
	} else {
		head = []byte(fmt.Sprintf(`{"k":%q,"n":%d,`, kind, *name))
	}
	if len(valueBytes) == 2 { // "{}"
		head = head[:len(head)-1]
		head = append(head, '}')
		return head, nil
	}
	out := make([]byte, 0, len(head)+len(valueBytes)-1)
	out = append(out, head...)
	out = append(out, valueBytes[1:]...)
	return out, nil
}

// resolveCoveredNames returns the field names covered by a Fields chunk by
// resolving its integer indices against the document-level Keys table.
// Returns nil when there is no Keys table or any index is out of range.
func resolveCoveredNames(c *ObjectChunk, keys []string) []string {
	if len(c.Fields) == 0 || len(keys) == 0 {
		return nil
	}
	out := make([]string, len(c.Fields))
	for i, idx := range c.Fields {
		if int(idx) >= len(keys) {
			return nil
		}
		out[i] = keys[idx]
	}
	return out
}

// strictUnmarshal is json.Unmarshal with DisallowUnknownFields enabled, and
// it rejects trailing data after the first JSON value.
func strictUnmarshal(data []byte, v any) error {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return err
	}
	return assertNoTrailingJSON(dec)
}

// assertNoTrailingJSON returns nil iff dec has no further JSON values to read.
func assertNoTrailingJSON(dec *json.Decoder) error {
	var trailing json.RawMessage
	err := dec.Decode(&trailing)
	if errors.Is(err, io.EOF) {
		return nil
	}
	if err == nil {
		return fmt.Errorf("trailing data after JSON value")
	}
	return err
}
