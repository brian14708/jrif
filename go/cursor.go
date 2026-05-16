package jrif

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strconv"
)

// Cursor is a position inside a JRIF-indexed payload.
//
// Built from [Index.Root]; descended with infallible Get(name)/Index(ordinal)
// methods that record pending path segments when the chunk index cannot
// resolve them in memory; driven to I/O at the leaves via Bytes/Value/
// Deserialize/Len/Iter/Entries, which run any deferred work in one go and
// surface the first error encountered.
//
// Cursors are immutable handles — Get and Index return a new Cursor without
// mutating the receiver, so descending from the same parent into multiple
// children is safe.
type Cursor struct {
	idx        *Index
	path       []Segment
	baseInline *inlineHandle // non-nil for inline-resolved cursors
	baseRange  Range
	baseWrap   wrap
	frame      frame
	pending    []Segment
}

// inlineHandle carries the inline Value's JSON-encoded form so leaf
// accessors can return it without any payload I/O.
type inlineHandle struct {
	jsonType JSONType
	// rawJSON is the serialized JSON form of the inline value — an arbitrary
	// RFC 8259 literal.
	rawJSON []byte
}

// inlineHandleFor builds an inlineHandle for an inline-form Value. The bytes
// are owned by the parsed Document (the json decoder copies them); Bytes()
// clones again before returning to the caller so callers can't mutate them.
func inlineHandleFor(v *Value) *inlineHandle {
	if v.Type != ValueValue {
		return nil
	}
	return &inlineHandle{jsonType: peekJSONType(v.Inline), rawJSON: v.Inline}
}

// peekJSONType identifies the JSON type of the first significant byte in raw.
// raw must be valid JSON (the document parser validates this).
func peekJSONType(raw []byte) JSONType {
	for _, b := range raw {
		switch b {
		case ' ', '\t', '\n', '\r':
			continue
		case '{':
			return JSONTypeObject
		case '[':
			return JSONTypeArray
		case '"':
			return JSONTypeString
		case 't', 'f':
			return JSONTypeBoolean
		case 'n':
			return JSONTypeNull
		default:
			return JSONTypeNumber
		}
	}
	return JSONTypeNull
}

// pathCopy returns an owned copy of the cursor's full path, for error contexts.
func (c *Cursor) pathCopy() Path { return slices.Clone(c.path) }

// Range returns the absolute [start, length] byte range the cursor exactly
// identifies. ok is false when the cursor has deferred navigation pending,
// points at an inline value (which has no range in the payload), or its base
// range only covers a wrapped fragment.
func (c *Cursor) Range() (r Range, ok bool) {
	if !c.IsResolved() || c.baseInline != nil {
		return Range{}, false
	}
	return c.baseRange, true
}

// IsResolved reports whether the cursor exactly identifies a value that can
// be returned without further navigation: either a contiguous JSON value in
// the payload (no pending segments, no wrap) or an inline value.
func (c *Cursor) IsResolved() bool {
	return len(c.pending) == 0 && c.baseWrap == wrapNone
}

// JSONTypeHint returns the best-effort JSON type from the chunk index without
// any I/O. ok is false when the type is unknown (deferred parse pending, or
// the underlying value sits below the chunking threshold).
func (c *Cursor) JSONTypeHint() (t JSONType, ok bool) {
	if len(c.pending) > 0 {
		return "", false
	}
	if c.baseInline != nil {
		return c.baseInline.jsonType, true
	}
	switch c.frame.kind {
	case frameArray:
		return JSONTypeArray, true
	case frameObject:
		return JSONTypeObject, true
	}
	return "", false
}

// Get descends to the named object member. Infallible — does no I/O. If the
// chunk index can place name exactly, the cursor advances; otherwise the
// descent is recorded as pending and resolved by the next leaf accessor.
func (c *Cursor) Get(name string) *Cursor {
	seg := FieldSegment(name)
	out := *c
	out.path = appendSegment(c.path, seg)
	if len(c.pending) > 0 || c.baseInline != nil || c.frame.kind != frameObject {
		out.pending = appendSegment(c.pending, seg)
		return &out
	}
	out.fillFromObjectHit(c.frame.findObjectMatch(name), seg)
	return &out
}

// Index descends to the array item at the given ordinal. Same deferred-
// resolution model as [Cursor.Get].
func (c *Cursor) Index(ordinal uint64) *Cursor {
	seg := IndexSegment(ordinal)
	out := *c
	out.path = appendSegment(c.path, seg)
	if len(c.pending) > 0 || c.baseInline != nil || c.frame.kind != frameArray {
		out.pending = appendSegment(c.pending, seg)
		return &out
	}
	out.fillFromArrayHit(c.frame.findArrayMatch(ordinal), ordinal, seg)
	return &out
}

// applyArrayHit builds the child cursor as if Index(ordinal) had been called
// against an array frame, but takes a pre-computed hit instead of scanning
// chunks. Used by [ArrayIter] for O(1) amortized sequential access.
func (c *Cursor) applyArrayHit(hit arrayHit, ordinal uint64) *Cursor {
	if hit.kind == arrayHitNone {
		return c.Index(ordinal)
	}
	seg := IndexSegment(ordinal)
	out := *c
	out.path = appendSegment(c.path, seg)
	out.fillFromArrayHit(hit, ordinal, seg)
	return &out
}

// applyObjectHit is the [Cursor.Get] equivalent of [Cursor.applyArrayHit].
func (c *Cursor) applyObjectHit(hit objectHit, name string) *Cursor {
	if hit.kind == objectHitNone {
		return c.Get(name)
	}
	seg := FieldSegment(name)
	out := *c
	out.path = appendSegment(c.path, seg)
	out.fillFromObjectHit(hit, seg)
	return &out
}

// fillFromArrayHit populates the cursor from a chunk hit at the given ordinal.
// origSeg is the segment that was just appended to path; it is also appended
// to pending (rebased on the chunk's start ordinal) when the hit spans an
// Items group, since the wrapped fragment still needs walking.
func (c *Cursor) fillFromArrayHit(hit arrayHit, ordinal uint64, origSeg Segment) {
	switch hit.kind {
	case arrayHitItem:
		c.fillFromValue(hit.value, hit.rng)
	case arrayHitItems:
		c.fillFromWrapped(hit.rng, wrapArray)
		c.pending = appendSegment(c.pending, IndexSegment(ordinal-hit.startOrdinal))
	default:
		c.pending = appendSegment(c.pending, origSeg)
	}
}

// fillFromObjectHit is the object analogue of [fillFromArrayHit]. When the
// hit spans a Fields group, origSeg is deferred onto pending so the wrapped
// fragment is walked to find the requested field.
func (c *Cursor) fillFromObjectHit(hit objectHit, origSeg Segment) {
	switch hit.kind {
	case objectHitField:
		c.fillFromValue(hit.value, hit.rng)
	case objectHitFields:
		c.fillFromWrapped(hit.rng, wrapObject)
		c.pending = appendSegment(c.pending, origSeg)
	default:
		c.pending = appendSegment(c.pending, origSeg)
	}
}

// fillFromValue resolves the cursor onto a chunk-described Value: inline when
// the value is inline, otherwise a contiguous payload range.
func (c *Cursor) fillFromValue(v *Value, rng Range) {
	c.baseInline = nil
	if v.IsInline() {
		c.baseInline = inlineHandleFor(v)
	} else {
		c.baseRange = rng
	}
	c.baseWrap = wrapNone
	c.frame = frameOf(v, c.idx.doc.Keys)
}

// fillFromWrapped resolves the cursor onto a wrapped chunk fragment (Items or
// Fields) — the range still needs bracket-wrapping before parsing.
func (c *Cursor) fillFromWrapped(rng Range, w wrap) {
	c.baseInline = nil
	c.baseRange = rng
	c.baseWrap = w
	c.frame = frame{kind: frameDone}
}

// Bytes returns the cursor's bytes as valid JSON.
//
// Fast path (no pending, no wrap): returns the raw payload slice, or a copy
// of the inline value's JSON when the cursor is inline-resolved. Slow path:
// fetches the deepest resolved chunk range, walks pending segments through
// a streaming JSON decoder, and returns the target value's raw bytes — no
// intermediate generic-value materialization.
func (c *Cursor) Bytes(ctx context.Context) ([]byte, error) {
	if c.IsResolved() {
		if c.baseInline != nil {
			return bytes.Clone(c.baseInline.rawJSON), nil
		}
		return c.fetchRaw(ctx)
	}
	return c.targetBytes(ctx)
}

// Value fetches, parses, and walks any pending segments. Returns a generic
// JSON value (map[string]any / []any / json.Number / string / bool / nil).
func (c *Cursor) Value(ctx context.Context) (any, error) {
	if c.IsResolved() {
		if c.baseInline != nil {
			return decodeJSON(c.baseInline.rawJSON, c.pathCopy)
		}
		b, err := c.fetchRaw(ctx)
		if err != nil {
			return nil, err
		}
		return decodeJSON(b, c.pathCopy)
	}
	b, err := c.targetBytes(ctx)
	if err != nil {
		return nil, err
	}
	return decodeJSON(b, c.pathCopy)
}

// Deserialize decodes the cursor's value into into using encoding/json. into
// must be a non-nil pointer. Skips the intermediate generic-value allocation
// regardless of whether the cursor is fully resolved — the slow path walks
// pending segments through a streaming decoder to capture the target's raw
// JSON bytes and unmarshals them directly into into.
func (c *Cursor) Deserialize(ctx context.Context, into any) error {
	var b []byte
	var err error
	if c.IsResolved() {
		if c.baseInline != nil {
			b = c.baseInline.rawJSON
		} else {
			b, err = c.fetchRaw(ctx)
			if err != nil {
				return err
			}
		}
	} else {
		b, err = c.targetBytes(ctx)
		if err != nil {
			return err
		}
	}
	if err := json.Unmarshal(b, into); err != nil {
		return &ParseError{Path: c.pathCopy(), Cause: err}
	}
	return nil
}

// targetBytes fetches the deepest resolved chunk range (or borrows the
// inline bytes), walks any pending segments through a streaming JSON
// decoder, and returns the raw JSON bytes of the target value. Returned
// bytes are always owned — safe to retain and mutate.
//
// Callers should short-circuit fully-resolved cursors before calling this;
// it always allocates fresh bytes for the slow path.
func (c *Cursor) targetBytes(ctx context.Context) ([]byte, error) {
	var b []byte
	var w wrap
	if c.baseInline != nil {
		b = c.baseInline.rawJSON
		w = wrapNone
	} else {
		var err error
		b, err = c.fetchRaw(ctx)
		if err != nil {
			return nil, err
		}
		w = c.baseWrap
	}
	if len(c.pending) == 0 {
		// No pending walk; the cursor's bytes are the base bytes (with
		// optional wrap brackets). Materialize an owned copy so callers can
		// retain it past any pool-borrowed buffer.
		var out []byte
		err := withWrapped(b, w, func(wrapped []byte) error {
			out = bytes.Clone(wrapped)
			return nil
		})
		return out, err
	}
	var out []byte
	err := withWrapped(b, w, func(wrapped []byte) error {
		raw, err := walkPendingRaw(wrapped, c.pending, c.path)
		if err != nil {
			return err
		}
		out = raw
		return nil
	})
	return out, err
}

// Len resolves the array length at this cursor. Returns from the chunk index
// without I/O when possible; otherwise fetches and parses the cursor's range.
func (c *Cursor) Len(ctx context.Context) (uint64, error) {
	if len(c.pending) == 0 {
		if c.baseInline != nil {
			switch c.baseInline.jsonType {
			case JSONTypeArray:
				return 0, nil
			default:
				return 0, &TypeMismatchError{
					Path:     c.pathCopy(),
					Expected: JSONTypeArray,
					Got:      c.baseInline.jsonType,
				}
			}
		}
		if c.frame.kind == frameArray {
			if n, ok := c.frame.arrayLen(); ok {
				return n, nil
			}
		}
	}
	v, err := c.Value(ctx)
	if err != nil {
		return 0, err
	}
	arr, ok := v.([]any)
	if !ok {
		return 0, &TypeMismatchError{
			Path:     c.pathCopy(),
			Expected: JSONTypeArray,
			Got:      jsonTypeOf(v),
		}
	}
	return uint64(len(arr)), nil
}

// AsString materializes the cursor and returns it as a string.
func (c *Cursor) AsString(ctx context.Context) (string, error) {
	v, err := c.Value(ctx)
	if err != nil {
		return "", err
	}
	s, ok := v.(string)
	if !ok {
		return "", &TypeMismatchError{Path: c.pathCopy(), Expected: JSONTypeString, Got: jsonTypeOf(v)}
	}
	return s, nil
}

// AsInt64 materializes the cursor and returns it as an int64.
func (c *Cursor) AsInt64(ctx context.Context) (int64, error) {
	v, err := c.Value(ctx)
	if err != nil {
		return 0, err
	}
	n, ok := v.(json.Number)
	if !ok {
		return 0, &TypeMismatchError{Path: c.pathCopy(), Expected: JSONTypeNumber, Got: jsonTypeOf(v)}
	}
	i, err := n.Int64()
	if err != nil {
		return 0, &ParseError{Path: c.pathCopy(), Cause: err}
	}
	return i, nil
}

// AsUint64 materializes the cursor and returns it as a uint64. Accepts the
// full uint64 range (numbers above MaxInt64 are valid here, unlike AsInt64).
func (c *Cursor) AsUint64(ctx context.Context) (uint64, error) {
	v, err := c.Value(ctx)
	if err != nil {
		return 0, err
	}
	n, ok := v.(json.Number)
	if !ok {
		return 0, &TypeMismatchError{Path: c.pathCopy(), Expected: JSONTypeNumber, Got: jsonTypeOf(v)}
	}
	u, err := strconv.ParseUint(string(n), 10, 64)
	if err != nil {
		return 0, &ParseError{Path: c.pathCopy(), Cause: err}
	}
	return u, nil
}

// AsFloat64 materializes the cursor and returns it as a float64.
func (c *Cursor) AsFloat64(ctx context.Context) (float64, error) {
	v, err := c.Value(ctx)
	if err != nil {
		return 0, err
	}
	n, ok := v.(json.Number)
	if !ok {
		return 0, &TypeMismatchError{Path: c.pathCopy(), Expected: JSONTypeNumber, Got: jsonTypeOf(v)}
	}
	f, err := n.Float64()
	if err != nil {
		return 0, &ParseError{Path: c.pathCopy(), Cause: err}
	}
	return f, nil
}

// AsBool materializes the cursor and returns it as a bool.
func (c *Cursor) AsBool(ctx context.Context) (bool, error) {
	v, err := c.Value(ctx)
	if err != nil {
		return false, err
	}
	b, ok := v.(bool)
	if !ok {
		return false, &TypeMismatchError{Path: c.pathCopy(), Expected: JSONTypeBoolean, Got: jsonTypeOf(v)}
	}
	return b, nil
}

// appendSegment returns a freshly allocated slice equal to s with seg
// appended. The result has len==cap so a subsequent append by a sibling
// cursor cannot clobber the parent's view.
func appendSegment(s []Segment, seg Segment) []Segment {
	out := make([]Segment, len(s)+1)
	copy(out, s)
	out[len(s)] = seg
	return out
}

// fetchRaw fetches the base range and confirms its length.
func (c *Cursor) fetchRaw(ctx context.Context) ([]byte, error) {
	b, err := c.idx.fetcher.Fetch(ctx, c.baseRange)
	if err != nil {
		return nil, &FetchError{Path: c.pathCopy(), Cause: err}
	}
	b, lerr := checkExactLen(c.baseRange, b)
	if lerr != nil {
		return nil, &FetchError{Path: c.pathCopy(), Cause: lerr}
	}
	return b, nil
}

// decodeJSON parses b as a single JSON value with number precision preserved.
// pathFn is invoked only on error to attach the cursor path lazily.
func decodeJSON(b []byte, pathFn func() Path) (any, error) {
	dec := json.NewDecoder(bytes.NewReader(b))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		return nil, &ParseError{Path: pathFn(), Cause: err}
	}
	if err := assertNoTrailingJSON(dec); err != nil {
		return nil, &ParseError{Path: pathFn(), Cause: err}
	}
	return v, nil
}

// walkPending consumes any deferred segments against a parsed value.
//
// For error context, the path attached is the prefix of fullPath up to and
// including the failing segment.
func walkPending(value any, pending []Segment, fullPath []Segment) (any, error) {
	prefixOffset := len(fullPath) - len(pending)
	for i, seg := range pending {
		errPath := func() Path { return slices.Clone(fullPath[:prefixOffset+i+1]) }
		if seg.IsIndex {
			arr, ok := value.([]any)
			if !ok {
				return nil, &TypeMismatchError{
					Path: errPath(), Expected: JSONTypeArray, Got: jsonTypeOf(value),
				}
			}
			idx := int(seg.Index)
			if idx < 0 || idx >= len(arr) {
				return nil, &NotFoundError{Path: errPath()}
			}
			value = arr[idx]
		} else {
			obj, ok := value.(map[string]any)
			if !ok {
				return nil, &TypeMismatchError{
					Path: errPath(), Expected: JSONTypeObject, Got: jsonTypeOf(value),
				}
			}
			sub, ok := obj[seg.Field]
			if !ok {
				return nil, &NotFoundError{Path: errPath()}
			}
			value = sub
		}
	}
	return value, nil
}

// walkPendingRaw walks pending segments through a streaming json.Decoder and
// captures the target value's raw JSON bytes. wrapped MUST be a complete
// JSON value (bracket-wrapped if the cursor sat on a chunk fragment).
//
// The returned slice is freshly allocated, independent of wrapped.
func walkPendingRaw(wrapped []byte, pending, fullPath []Segment) ([]byte, error) {
	dec := json.NewDecoder(bytes.NewReader(wrapped))
	dec.UseNumber()
	if err := walkPendingDecoder(dec, pending, fullPath); err != nil {
		return nil, err
	}
	// dec is now positioned to read the target value as a complete JSON
	// value. Decode into a RawMessage to capture its raw bytes.
	var raw json.RawMessage
	if err := dec.Decode(&raw); err != nil {
		return nil, &ParseError{Path: slices.Clone(fullPath), Cause: err}
	}
	// Decoder may share buffer memory across tokens; make an owned copy.
	return bytes.Clone(raw), nil
}

// walkPendingDecoder advances dec through pending segments. On success, dec
// is positioned to read the target value as a complete JSON value.
func walkPendingDecoder(dec *json.Decoder, pending, fullPath []Segment) error {
	prefixOffset := len(fullPath) - len(pending)
	errPath := func(i int) Path { return slices.Clone(fullPath[:prefixOffset+i+1]) }
	for i, seg := range pending {
		tok, err := dec.Token()
		if err != nil {
			return &ParseError{Path: errPath(i), Cause: err}
		}
		d, ok := tok.(json.Delim)
		if seg.IsIndex {
			if !ok || d != '[' {
				return &TypeMismatchError{
					Path: errPath(i), Expected: JSONTypeArray, Got: tokenType(tok),
				}
			}
			if err := skipToArrayIndex(dec, seg.Index); err != nil {
				if errors.Is(err, errNotFound) {
					return &NotFoundError{Path: errPath(i)}
				}
				return &ParseError{Path: errPath(i), Cause: err}
			}
			continue
		}
		if !ok || d != '{' {
			return &TypeMismatchError{
				Path: errPath(i), Expected: JSONTypeObject, Got: tokenType(tok),
			}
		}
		if err := skipToObjectField(dec, seg.Field); err != nil {
			if errors.Is(err, errNotFound) {
				return &NotFoundError{Path: errPath(i)}
			}
			return &ParseError{Path: errPath(i), Cause: err}
		}
	}
	return nil
}

// objectKeys resolves the list of field names at this cursor's position, in
// source order. Always recovers keys from the payload bytes via a streaming
// parse — JRIF v0 allows object chunks to cover only a subset of an object's
// fields, so the chunk index can't be trusted to enumerate every key.
func (c *Cursor) objectKeys(ctx context.Context) ([]string, error) {
	if c.baseInline != nil {
		if len(c.pending) == 0 && c.baseInline.jsonType != JSONTypeObject {
			return nil, &TypeMismatchError{
				Path: c.pathCopy(), Expected: JSONTypeObject, Got: c.baseInline.jsonType,
			}
		}
		return streamObjectKeys(c.baseInline.rawJSON, c.pending, c.path)
	}
	if len(c.pending) == 0 && c.frame.kind == frameArray {
		return nil, &TypeMismatchError{
			Path: c.pathCopy(), Expected: JSONTypeObject, Got: JSONTypeArray,
		}
	}
	b, err := c.fetchRaw(ctx)
	if err != nil {
		return nil, err
	}
	var keys []string
	err = withWrapped(b, c.baseWrap, func(wrapped []byte) error {
		ks, err := streamObjectKeys(wrapped, c.pending, c.path)
		if err != nil {
			return err
		}
		keys = ks
		return nil
	})
	return keys, err
}

// streamObjectKeys walks pending segments through wrapped JSON bytes with a
// streaming decoder, then collects the target object's keys in source order.
// pending may be empty (target is the wrapped value itself).
func streamObjectKeys(wrapped []byte, pending, fullPath []Segment) ([]string, error) {
	dec := json.NewDecoder(bytes.NewReader(wrapped))
	dec.UseNumber()
	if err := walkPendingDecoder(dec, pending, fullPath); err != nil {
		return nil, err
	}
	keys, err := readObjectKeysFromDecoder(dec)
	if err != nil {
		path := slices.Clone(fullPath)
		if tmErr, ok := err.(*typeMismatchAtCursor); ok {
			return nil, &TypeMismatchError{Path: path, Expected: JSONTypeObject, Got: tmErr.got}
		}
		return nil, &ParseError{Path: path, Cause: err}
	}
	return keys, nil
}

var errNotFound = errors.New("not found")

// skipToArrayIndex advances dec past array entries until it is positioned to
// read the entry at ordinal. The opening '[' has already been consumed.
func skipToArrayIndex(dec *json.Decoder, ordinal uint64) error {
	for i := uint64(0); dec.More(); i++ {
		if i == ordinal {
			return nil
		}
		var skip json.RawMessage
		if err := dec.Decode(&skip); err != nil {
			return err
		}
	}
	return errNotFound
}

// skipToObjectField advances dec past field name/value pairs until it is
// positioned to read the value for name. The opening '{' has already been
// consumed.
func skipToObjectField(dec *json.Decoder, name string) error {
	for dec.More() {
		tok, err := dec.Token()
		if err != nil {
			return err
		}
		key, ok := tok.(string)
		if !ok {
			return fmt.Errorf("expected field name, got %v", tok)
		}
		if key == name {
			return nil
		}
		var skip json.RawMessage
		if err := dec.Decode(&skip); err != nil {
			return err
		}
	}
	return errNotFound
}

type typeMismatchAtCursor struct{ got JSONType }

func (e *typeMismatchAtCursor) Error() string { return "expected object" }

// readObjectKeysFromDecoder reads the next JSON value from dec, which must be
// an object, and returns its top-level keys in source order.
func readObjectKeysFromDecoder(dec *json.Decoder) ([]string, error) {
	tok, err := dec.Token()
	if err != nil {
		return nil, err
	}
	d, ok := tok.(json.Delim)
	if !ok || d != '{' {
		return nil, &typeMismatchAtCursor{got: tokenType(tok)}
	}
	var keys []string
	for dec.More() {
		tok, err := dec.Token()
		if err != nil {
			return nil, err
		}
		name, ok := tok.(string)
		if !ok {
			return nil, fmt.Errorf("expected field name, got %v", tok)
		}
		keys = append(keys, name)
		var skip json.RawMessage
		if err := dec.Decode(&skip); err != nil {
			return nil, err
		}
	}
	if _, err := dec.Token(); err != nil {
		return nil, err
	}
	return keys, nil
}

// tokenType reports the JSONType for a token produced by json.Decoder.Token.
// Open delimiters '{' and '[' map to object/array; closing delimiters never
// appear here. Non-delimiter tokens are dispatched through jsonTypeOf.
func tokenType(tok json.Token) JSONType {
	if d, ok := tok.(json.Delim); ok {
		switch d {
		case '{':
			return JSONTypeObject
		case '[':
			return JSONTypeArray
		}
	}
	return jsonTypeOf(tok)
}
