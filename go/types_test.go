package jrif

import (
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

func TestValueRangedStringRoundTrip(t *testing.T) {
	in := []byte(`{"t":"s","r":[10,11]}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal scalar: %v", err)
	}
	if v.Type != ValueString || v.Range != (Range{10, 11}) || v.IsInline() {
		t.Fatalf("bad ranged string: %#v", v)
	}
	out, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	if !strings.Contains(string(out), `"t":"s"`) {
		t.Fatalf("marshal missing type: %s", out)
	}
	if !strings.Contains(string(out), `"r":[10,11]`) {
		t.Fatalf("marshal missing range: %s", out)
	}
}

func TestValueInlineNull(t *testing.T) {
	in := []byte(`{"t":"v","v":null}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal null: %v", err)
	}
	if v.Type != ValueValue || !v.IsInline() {
		t.Fatalf("bad inline null: %#v", v)
	}
	out, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	if string(out) != `{"t":"v","v":null}` {
		t.Fatalf("expected {\"t\":\"v\",\"v\":null}, got %s", out)
	}
}

func TestValueInlineBoolean(t *testing.T) {
	in := []byte(`{"t":"v","v":true}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal bool: %v", err)
	}
	if v.Type != ValueValue || !v.IsInline() {
		t.Fatalf("bad inline bool: %#v", v)
	}
}

func TestValueInlineNumber(t *testing.T) {
	in := []byte(`{"t":"v","v":42}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal num: %v", err)
	}
	if v.Type != ValueValue || !v.IsInline() {
		t.Fatalf("bad inline num: %#v", v)
	}
}

func TestValueInlineString(t *testing.T) {
	in := []byte(`{"t":"v","v":"ok"}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal inline string: %v", err)
	}
	if v.Type != ValueValue || !v.IsInline() {
		t.Fatalf("bad inline string: %#v", v)
	}
}

func TestValueInlineEmptyArray(t *testing.T) {
	in := []byte(`{"t":"v","v":[]}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal empty array: %v", err)
	}
	if v.Type != ValueValue || !v.IsInline() {
		t.Fatalf("bad inline empty array: %#v", v)
	}
}

func TestValueInlineEmptyObject(t *testing.T) {
	in := []byte(`{"t":"v","v":{}}`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal empty object: %v", err)
	}
	if v.Type != ValueValue || !v.IsInline() {
		t.Fatalf("bad inline empty object: %#v", v)
	}
}

func TestValueArrayWithItemsChunk(t *testing.T) {
	in := []byte(`{
        "t":"a",
        "r":[0,101],
        "c":[
            {"k":"is","n":3,"r":[1,99]}
        ]
    }`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if v.Type != ValueArray || len(v.ArrayChunks) != 1 {
		t.Fatalf("bad array: %#v", v)
	}
	c := v.ArrayChunks[0]
	if c.Kind != ArrayChunkItems || c.Count != 3 || c.Range != (Range{1, 99}) {
		t.Fatalf("bad items chunk: %#v", c)
	}
}

// TestValueArrayWithItemWrappingValue covers the flat-shape item chunk:
// k="i" sits alongside the wrapped value's t/r/c.
func TestValueArrayWithItemWrappingValue(t *testing.T) {
	in := []byte(`{
        "t":"a",
        "r":[0,101],
        "c":[
            {"k":"i","t":"o","r":[1,50],
             "c":[
                {"k":"fs","f":[0,1],"r":[2,48]}
             ]}
        ]
    }`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	item := v.ArrayChunks[0]
	if item.Kind != ArrayChunkItem || item.Value == nil {
		t.Fatalf("bad item chunk: %#v", item)
	}
	if item.Value.Type != ValueObject {
		t.Fatalf("expected item value type=object, got %q", item.Value.Type)
	}
	if len(item.Value.ObjectChunks) != 1 {
		t.Fatalf("expected one nested object chunk, got %d", len(item.Value.ObjectChunks))
	}
	oc := item.Value.ObjectChunks[0]
	if oc.Kind != ObjectChunkFields {
		t.Fatalf("expected nested fields chunk, got %v", oc.Kind)
	}
	if len(oc.Fields) != 2 || oc.Fields[0] != 0 || oc.Fields[1] != 1 {
		t.Fatalf("unexpected fields list: %v", oc.Fields)
	}
}

func TestValueArrayWithItemWrappingInlineNumber(t *testing.T) {
	in := []byte(`{
        "t":"a",
        "r":[0,101],
        "c":[
            {"k":"i","t":"v","v":42}
        ]
    }`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	item := v.ArrayChunks[0]
	if item.Kind != ArrayChunkItem || item.Value == nil {
		t.Fatalf("bad item chunk: %#v", item)
	}
	if item.Value.Type != ValueValue || !item.Value.IsInline() {
		t.Fatalf("expected inline value, got %#v", item.Value)
	}
}

func TestValueObjectWithFieldChunkByName(t *testing.T) {
	in := []byte(`{
        "t":"o",
        "r":[0,101],
        "c":[
            {"k":"f","n":2,"t":"s","r":[10,81]}
        ]
    }`)
	var v Value
	if err := json.Unmarshal(in, &v); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if len(v.ObjectChunks) != 1 {
		t.Fatalf("bad object chunks: %#v", v)
	}
	fc := v.ObjectChunks[0]
	if fc.Kind != ObjectChunkField || fc.Name != 2 || fc.Value == nil {
		t.Fatalf("bad field chunk: %#v", fc)
	}
	if fc.Value.Type != ValueString {
		t.Fatalf("expected field value type=string, got %q", fc.Value.Type)
	}
}

func TestFieldsChunkRejectsEmptyFields(t *testing.T) {
	in := []byte(`{"k":"fs","r":[0,2]}`)
	var c ObjectChunk
	if err := json.Unmarshal(in, &c); err == nil {
		t.Fatal("expected error when fields is missing/empty")
	}
}

func TestFieldsChunkRoundTrip(t *testing.T) {
	in := []byte(`{"k":"fs","f":[0,2,4],"r":[0,2]}`)
	var c ObjectChunk
	if err := json.Unmarshal(in, &c); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if len(c.Fields) != 3 || c.Fields[0] != 0 || c.Fields[2] != 4 {
		t.Fatalf("unexpected fields: %v", c.Fields)
	}
	out, err := json.Marshal(c)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	if !strings.Contains(string(out), `"f":[0,2,4]`) {
		t.Fatalf("marshal missing fields: %s", out)
	}
}

func TestDocumentWithKeysRoundTrip(t *testing.T) {
	in := []byte(`{
        "jrif":"v0",
        "keys":["a","b","c"],
        "root":{"t":"v","v":null}
    }`)
	var d Document
	if err := json.Unmarshal(in, &d); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if len(d.Keys) != 3 || d.Keys[0] != "a" || d.Keys[2] != "c" {
		t.Fatalf("unexpected keys: %v", d.Keys)
	}
	out, err := json.Marshal(d)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	if !strings.Contains(string(out), `"keys":["a","b","c"]`) {
		t.Fatalf("marshal missing keys: %s", out)
	}
}

func TestDocumentRejectsUnknownTopLevelField(t *testing.T) {
	in := []byte(`{
        "jrif":"v0",
        "root":{"t":"v","v":null},
        "extra":"nope"
    }`)
	var d Document
	if err := json.Unmarshal(in, &d); err == nil {
		t.Fatal("expected unknown field error")
	}
}

func TestDocumentRoundTrip(t *testing.T) {
	payload, jrif := loadFixture(t)
	_ = payload
	var d Document
	if err := json.Unmarshal(jrif, &d); err != nil {
		t.Fatalf("Unmarshal fixture: %v", err)
	}
	if d.Jrif != JrifV0 {
		t.Fatalf("bad jrif: %q", d.Jrif)
	}
	out, err := json.Marshal(d)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}
	var d2 Document
	if err := json.Unmarshal(out, &d2); err != nil {
		t.Fatalf("Re-unmarshal: %v", err)
	}
	if d2.Jrif != d.Jrif {
		t.Fatalf("jrif diverged: %q vs %q", d.Jrif, d2.Jrif)
	}
	if d2.Root.Type != d.Root.Type {
		t.Fatalf("root type diverged: %q vs %q", d.Root.Type, d2.Root.Type)
	}
}

func TestUnsupportedJrif(t *testing.T) {
	bad := []byte(`{"jrif":"vX","root":{"t":"v","v":null}}`)
	_, err := Open(bad, InMemoryPayload(nil))
	if err == nil {
		t.Fatal("expected error")
	}
	if !errors.Is(err, ErrUnsupportedJrif) {
		t.Fatalf("expected errors.Is(err, ErrUnsupportedJrif), got %v", err)
	}
	var ide *InvalidDocumentError
	if !errors.As(err, &ide) {
		t.Fatalf("expected *InvalidDocumentError, got %T", err)
	}
	if ide.Jrif != "vX" {
		t.Fatalf("bad jrif field: %q", ide.Jrif)
	}
}

func TestRangedValueRejectsMissingRange(t *testing.T) {
	in := []byte(`{"t":"s"}`)
	var v Value
	if err := json.Unmarshal(in, &v); err == nil {
		t.Fatal("expected missing-range error")
	}
}

func TestArrayChunkRejectsMissingFields(t *testing.T) {
	// k="is" needs count/range.
	in := []byte(`{"k":"is"}`)
	var c ArrayChunk
	if err := json.Unmarshal(in, &c); err == nil {
		t.Fatal("expected error for missing fields on items chunk")
	}
	// k="i" needs the wrapped value's type at minimum.
	in = []byte(`{"k":"i"}`)
	if err := (&c).UnmarshalJSON(in); err == nil {
		t.Fatal("expected error for missing fields on item chunk")
	}
}

func TestObjectChunkRejectsMissingFields(t *testing.T) {
	in := []byte(`{"k":"f","n":0}`)
	var c ObjectChunk
	if err := json.Unmarshal(in, &c); err == nil {
		t.Fatal("expected error for missing fields on field chunk")
	}
}
