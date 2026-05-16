package jrif

import (
	"errors"
	"fmt"
	"strings"
)

// JSONType is the JSON type discriminator surfaced in TypeMismatchError.
type JSONType string

const (
	JSONTypeNull    JSONType = "null"
	JSONTypeBoolean JSONType = "boolean"
	JSONTypeNumber  JSONType = "number"
	JSONTypeString  JSONType = "string"
	JSONTypeArray   JSONType = "array"
	JSONTypeObject  JSONType = "object"
)

// Segment is one step in a Path from the document root to a cursor.
// Exactly one of Field or Index is meaningful, distinguished by IsIndex.
type Segment struct {
	Field   string
	Index   uint64
	IsIndex bool
}

// FieldSegment returns a name-bearing segment.
func FieldSegment(name string) Segment { return Segment{Field: name} }

// IndexSegment returns an ordinal-bearing segment.
func IndexSegment(i uint64) Segment { return Segment{Index: i, IsIndex: true} }

// String formats this segment in JSONPath-like dot/bracket form.
func (s Segment) String() string {
	if s.IsIndex {
		return fmt.Sprintf("[%d]", s.Index)
	}
	return "." + s.Field
}

// Path is the sequence of Segments from the root to the cursor that produced
// an error. Rendered as `$`, `$.field`, `$.field[2].name`, etc.
type Path []Segment

// String renders the path with a leading `$`.
func (p Path) String() string {
	var b strings.Builder
	b.WriteByte('$')
	for _, s := range p {
		b.WriteString(s.String())
	}
	return b.String()
}

// ErrUnsupportedJrif is returned when a document's jrif version tag does not
// match the v0 value. Use errors.As against *InvalidDocumentError for the value.
var ErrUnsupportedJrif = errors.New("unsupported jrif version")

// InvalidDocumentError reports a malformed or unsupported JRIF sidecar.
type InvalidDocumentError struct {
	Reason string
	// Jrif is set when the failure was an unsupported jrif version tag;
	// empty otherwise.
	Jrif string
}

func (e *InvalidDocumentError) Error() string {
	return "invalid JRIF document: " + e.Reason
}

func (e *InvalidDocumentError) Is(target error) bool {
	return e.Jrif != "" && target == ErrUnsupportedJrif
}

// NotFoundError reports an object field that's missing, or an array index
// that's out of bounds. The Segment that failed is the last element of Path.
type NotFoundError struct {
	Path Path
}

func (e *NotFoundError) Error() string {
	return fmt.Sprintf("not found at %s", e.Path)
}

// TypeMismatchError reports a leaf accessor or descent step that did not
// match the underlying JSON value's actual type.
type TypeMismatchError struct {
	Path     Path
	Expected JSONType
	Got      JSONType
}

func (e *TypeMismatchError) Error() string {
	return fmt.Sprintf("type mismatch at %s: expected %s, got %s", e.Path, e.Expected, e.Got)
}

// FetchError wraps an error returned by a RangeFetcher implementation.
type FetchError struct {
	Path  Path
	Cause error
}

func (e *FetchError) Error() string {
	if len(e.Path) == 0 {
		return fmt.Sprintf("fetch failed: %v", e.Cause)
	}
	return fmt.Sprintf("fetch failed at %s: %v", e.Path, e.Cause)
}

func (e *FetchError) Unwrap() error { return e.Cause }

// ParseError reports that fetched JSON bytes did not parse as a complete
// JSON value (e.g. wrapped chunk fragment).
type ParseError struct {
	Path  Path
	Cause error
}

func (e *ParseError) Error() string {
	if len(e.Path) == 0 {
		return fmt.Sprintf("parse error: %v", e.Cause)
	}
	return fmt.Sprintf("parse error at %s: %v", e.Path, e.Cause)
}

func (e *ParseError) Unwrap() error { return e.Cause }

// jsonTypeOf returns the JSONType discriminator for an unmarshaled JSON value.
func jsonTypeOf(v any) JSONType {
	switch v.(type) {
	case nil:
		return JSONTypeNull
	case bool:
		return JSONTypeBoolean
	case string:
		return JSONTypeString
	case []any:
		return JSONTypeArray
	case map[string]any:
		return JSONTypeObject
	default:
		// json.Number, float64, int*, uint* — all numbers.
		return JSONTypeNumber
	}
}
