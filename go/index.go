// Package jrif is a Go reader for the JSON Range Index Format (JRIF) v0.
//
// JRIF is a JSON sidecar that maps paths inside a large JSON payload to byte
// ranges so that readers can fetch only the parts they need from byte
// addressable storage. See the spec at docs/spec.md in the source repository.
//
// Build an [Index] from a JRIF sidecar plus a [RangeFetcher], then navigate
// it by path with [Cursor]:
//
//	idx, err := jrif.Open(jrifBytes, jrif.InMemoryPayload(payload))
//	if err != nil { /* ... */ }
//	var name string
//	err = idx.Root().Get("records").Index(0).Get("name").Deserialize(ctx, &name)
//
// Navigation (Get, Index) is infallible and chainable; I/O and parsing only
// happen at the leaf accessors (Bytes, Value, Deserialize, Len, Iter, Entries),
// which all take a [context.Context] for cancellation.
package jrif

// Open parses jrifBytes as a JRIF v0 document and pairs it with fetcher.
// All payload I/O is deferred until the first cursor leaf accessor.
func Open(jrifBytes []byte, fetcher RangeFetcher) (*Index, error) {
	var doc Document
	if err := doc.UnmarshalJSON(jrifBytes); err != nil {
		return nil, &InvalidDocumentError{Reason: "parse jrif: " + err.Error()}
	}
	return OpenDocument(&doc, fetcher)
}

// OpenDocument pairs an already-parsed document with fetcher, checking only
// the jrif version tag.
func OpenDocument(doc *Document, fetcher RangeFetcher) (*Index, error) {
	if doc.Jrif != JrifV0 {
		return nil, &InvalidDocumentError{
			Reason: "unsupported jrif version: " + doc.Jrif,
			Jrif:   doc.Jrif,
		}
	}
	populateChunkCaches(&doc.Root, doc.Keys)
	return &Index{doc: doc, fetcher: fetcher}, nil
}

// populateChunkCaches walks the value tree once and fills [Value.arrCum] on
// every ranged array with non-empty chunks, so subsequent random access via
// [Cursor.Index] / [Cursor.Len] can binary-search in O(log N) and O(1)
// respectively. Walks deeply into Item chunks' nested values.
func populateChunkCaches(v *Value, keys []string) {
	if v == nil {
		return
	}
	switch v.Type {
	case ValueArray:
		if !v.IsInline() && len(v.ArrayChunks) > 0 {
			cum := make([]uint64, len(v.ArrayChunks)+1)
			var running uint64
			for i := range v.ArrayChunks {
				c := &v.ArrayChunks[i]
				cum[i] = running
				switch c.Kind {
				case ArrayChunkItem:
					running++
					populateChunkCaches(c.Value, keys)
				case ArrayChunkItems:
					running += c.Count
				}
			}
			cum[len(v.ArrayChunks)] = running
			v.arrCum = cum
		}
	case ValueObject:
		if !v.IsInline() && len(v.ObjectChunks) > 0 {
			v.objLookup = buildObjectLookup(v.ObjectChunks, keys)
		}
		for i := range v.ObjectChunks {
			c := &v.ObjectChunks[i]
			if c.Kind == ObjectChunkField {
				populateChunkCaches(c.Value, keys)
			}
		}
	}
}

// Index pairs a parsed JRIF document with a [RangeFetcher] and serves
// [Cursor] handles that navigate the chunk tree, only pulling payload bytes
// when the index can no longer drill deeper.
type Index struct {
	doc     *Document
	fetcher RangeFetcher
}

// Document returns the parsed JRIF document.
func (i *Index) Document() *Document { return i.doc }

// Fetcher returns the underlying range fetcher (useful for introspecting a
// decorated [RangeCache]).
func (i *Index) Fetcher() RangeFetcher { return i.fetcher }

// Root returns a Cursor at the document root.
func (i *Index) Root() *Cursor {
	v := &i.doc.Root
	c := &Cursor{
		idx:      i,
		baseWrap: wrapNone,
		frame:    rootFrame(v, i.doc.Keys),
	}
	if v.IsInline() {
		c.baseInline = inlineHandleFor(v)
	} else {
		c.baseRange = v.Range
	}
	return c
}
