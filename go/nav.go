package jrif

import (
	"sort"
	"sync"
)

// wrap describes how a fetched byte range relates to ordinary JSON.
type wrap int

const (
	wrapNone wrap = iota
	wrapArray
	wrapObject
)

// frameKind identifies what container the current navigation cursor is
// inside.
type frameKind int

const (
	frameDone frameKind = iota
	frameArray
	frameObject
)

// frame is the internal navigation context: it remembers what container we
// are inside and the chunks list to search.
type frame struct {
	kind         frameKind
	arrayChunks  []ArrayChunk
	objectChunks []ObjectChunk
	// arrCum is the cumulative-ordinal table from the source Value, copied
	// in by [frameOf]. Non-empty only for chunked arrays opened through
	// [OpenDocument]; enables O(log N) random access in [findArrayMatch]
	// and O(1) length in [arrayLen].
	arrCum []uint64
	// keys is the document-level `keys` table, used to resolve the integer
	// indices in Fields chunks while navigating an object frame.
	keys      []string
	objLookup map[string]objectHit
}

type arrayHitKind int

const (
	arrayHitNone arrayHitKind = iota
	arrayHitItem
	arrayHitItems
)

type arrayHit struct {
	kind         arrayHitKind
	rng          Range
	value        *Value // populated for arrayHitItem
	startOrdinal uint64
}

type objectHitKind int

const (
	objectHitNone objectHitKind = iota
	objectHitField
	objectHitFields
)

type objectHit struct {
	kind  objectHitKind
	rng   Range
	value *Value // populated for objectHitField
}

// frameOf builds a navigation frame for a Value. Ranged compounds with
// non-empty chunks yield a chunk frame; everything else (inline values,
// primitives, unchunked compounds) yields frameDone.
func frameOf(v *Value, keys []string) frame {
	switch v.Type {
	case ValueArray:
		if v.IsInline() || len(v.ArrayChunks) == 0 {
			return frame{kind: frameDone}
		}
		return frame{kind: frameArray, arrayChunks: v.ArrayChunks, arrCum: v.arrCum, keys: keys}
	case ValueObject:
		if v.IsInline() || len(v.ObjectChunks) == 0 {
			return frame{kind: frameDone}
		}
		return frame{kind: frameObject, objectChunks: v.ObjectChunks, keys: keys, objLookup: v.objLookup}
	default:
		return frame{kind: frameDone}
	}
}

// rootFrame builds a navigation frame for a document's root.
func rootFrame(v *Value, keys []string) frame { return frameOf(v, keys) }

// findArrayMatch locates the chunk covering the given array ordinal. Uses
// the cached cumulative-ordinal table for O(log N) binary search when
// available; falls back to a linear scan when the cache is empty (e.g.
// frames built without going through [OpenDocument]).
func (f frame) findArrayMatch(ordinal uint64) arrayHit {
	if len(f.arrCum) > 0 {
		cum := f.arrCum
		if ordinal >= cum[len(cum)-1] {
			return arrayHit{}
		}
		// Largest i with cum[i] <= ordinal: search for the first cum[i] > ordinal,
		// then back up one.
		hi := sort.Search(len(cum), func(i int) bool { return cum[i] > ordinal })
		if hi == 0 {
			return arrayHit{}
		}
		chunkIdx := hi - 1
		c := &f.arrayChunks[chunkIdx]
		switch c.Kind {
		case ArrayChunkItem:
			return arrayHit{kind: arrayHitItem, rng: c.Range, value: c.Value}
		case ArrayChunkItems:
			return arrayHit{kind: arrayHitItems, rng: c.Range, startOrdinal: cum[chunkIdx]}
		}
		return arrayHit{}
	}
	var running uint64
	for i := range f.arrayChunks {
		if h, ok := matchArray(&f.arrayChunks[i], ordinal, &running); ok {
			return h
		}
	}
	return arrayHit{}
}

func matchArray(c *ArrayChunk, ordinal uint64, running *uint64) (arrayHit, bool) {
	switch c.Kind {
	case ArrayChunkItem:
		if *running == ordinal {
			return arrayHit{
				kind:  arrayHitItem,
				rng:   c.Range,
				value: c.Value,
			}, true
		}
		*running++
	case ArrayChunkItems:
		if ordinal >= *running && ordinal < *running+c.Count {
			return arrayHit{
				kind:         arrayHitItems,
				rng:          c.Range,
				startOrdinal: *running,
			}, true
		}
		*running += c.Count
	}
	return arrayHit{}, false
}

// findObjectMatch locates the chunk covering the given field name.
func (f frame) findObjectMatch(name string) objectHit {
	if f.objLookup != nil {
		return f.objLookup[name]
	}
	for i := range f.objectChunks {
		if h, ok := matchObject(&f.objectChunks[i], name, f.keys); ok {
			return h
		}
	}
	return objectHit{}
}

func buildObjectLookup(chunks []ObjectChunk, keys []string) map[string]objectHit {
	if len(chunks) == 0 || len(keys) == 0 {
		return nil
	}
	out := make(map[string]objectHit)
	for i := range chunks {
		c := &chunks[i]
		switch c.Kind {
		case ObjectChunkField:
			if int(c.Name) < len(keys) {
				name := keys[c.Name]
				if _, exists := out[name]; !exists {
					out[name] = objectHit{
						kind:  objectHitField,
						rng:   c.Range,
						value: c.Value,
					}
				}
			}
		case ObjectChunkFields:
			hit := objectHit{kind: objectHitFields, rng: c.Range}
			for _, idx := range c.Fields {
				if int(idx) < len(keys) {
					name := keys[idx]
					if _, exists := out[name]; !exists {
						out[name] = hit
					}
				}
			}
		}
	}
	return out
}

func matchObject(c *ObjectChunk, name string, keys []string) (objectHit, bool) {
	switch c.Kind {
	case ObjectChunkField:
		if int(c.Name) < len(keys) && keys[c.Name] == name {
			return objectHit{
				kind:  objectHitField,
				rng:   c.Range,
				value: c.Value,
			}, true
		}
	case ObjectChunkFields:
		if fieldsChunkCovers(c, keys, name) {
			return objectHit{
				kind: objectHitFields,
				rng:  c.Range,
			}, true
		}
	}
	return objectHit{}, false
}

func fieldsChunkCovers(c *ObjectChunk, keys []string, name string) bool {
	for _, idx := range c.Fields {
		if int(idx) < len(keys) && keys[idx] == name {
			return true
		}
	}
	return false
}

// wrapScratchPool reuses bracket-prefixed buffers across [withWrapped]
// callers to avoid an allocation per chunk-fragment parse.
var wrapScratchPool = sync.Pool{
	New: func() any {
		b := make([]byte, 0, 4096)
		return &b
	},
}

// withWrapped invokes fn with a slice of valid JSON: for wrapNone fn sees
// b verbatim; for wrapArray/wrapObject fn sees a bracket-wrapped buffer
// borrowed from the pool. The buffer is returned to the pool on exit.
//
// fn MUST NOT retain the slice across the call — it's reused.
func withWrapped(b []byte, w wrap, fn func([]byte) error) error {
	var open, close byte
	switch w {
	case wrapArray:
		open, close = '[', ']'
	case wrapObject:
		open, close = '{', '}'
	default:
		return fn(b)
	}
	bufp := wrapScratchPool.Get().(*[]byte)
	defer func() {
		// Cap pool retention so a single huge fragment doesn't pin a
		// proportionally huge buffer in the pool forever.
		const maxRetainCap = 1 << 20
		if cap(*bufp) > maxRetainCap {
			return
		}
		wrapScratchPool.Put(bufp)
	}()
	*bufp = append((*bufp)[:0], open)
	*bufp = append(*bufp, b...)
	*bufp = append(*bufp, close)
	return fn(*bufp)
}

// arrayLen returns the total length of the array described by this frame's
// chunks. Returns ok=false for object frames or empty chunk lists.
func (f frame) arrayLen() (uint64, bool) {
	if f.kind != frameArray || len(f.arrayChunks) == 0 {
		return 0, false
	}
	if len(f.arrCum) > 0 {
		return f.arrCum[len(f.arrCum)-1], true
	}
	var total uint64
	for i := range f.arrayChunks {
		c := &f.arrayChunks[i]
		if c.Kind == ArrayChunkItem {
			total++
		} else {
			total += c.Count
		}
	}
	return total, true
}

// arrayWalker advances through an array frame's chunks in source order,
// emitting one [arrayHit] per logical array item.
type arrayWalker struct {
	chunks []ArrayChunk

	chunkIdx int    // index into chunks
	inChunk  uint64 // offset within current Items chunk
	ordinal  uint64 // array-relative ordinal at the start of current chunk
}

// newArrayWalker builds a walker for frame f. Returns nil when f is not an
// array frame — caller falls back to [Cursor.Index] in that case.
func newArrayWalker(f *frame) *arrayWalker {
	if f.kind != frameArray {
		return nil
	}
	return &arrayWalker{chunks: f.arrayChunks}
}

// next returns the hit for the current item and advances the walker.
func (w *arrayWalker) next() (arrayHit, uint64, bool) {
	for {
		if w.chunkIdx >= len(w.chunks) {
			return arrayHit{}, 0, false
		}
		c := &w.chunks[w.chunkIdx]
		switch c.Kind {
		case ArrayChunkItem:
			ord := w.ordinal
			w.advanceChunk(1)
			return arrayHit{
				kind:  arrayHitItem,
				rng:   c.Range,
				value: c.Value,
			}, ord, true
		case ArrayChunkItems:
			if w.inChunk >= c.Count {
				w.advanceChunk(c.Count)
				continue
			}
			ord := w.ordinal + w.inChunk
			hit := arrayHit{
				kind:         arrayHitItems,
				rng:          c.Range,
				startOrdinal: w.ordinal,
			}
			w.inChunk++
			if w.inChunk >= c.Count {
				w.advanceChunk(c.Count)
			}
			return hit, ord, true
		default:
			// Unknown chunk kind — skip it without consuming an ordinal.
			w.chunkIdx++
		}
	}
}

func (w *arrayWalker) advanceChunk(count uint64) {
	w.chunkIdx++
	w.inChunk = 0
	w.ordinal += count
}

// objectEntry pairs a field name with the chunk hit that resolves it, if any.
type objectEntry struct {
	name string
	hit  objectHit
}

// chunkHitsByName returns a lookup table from field name to its describing
// chunk hit. Used by [Cursor.Entries] to apply chunk-derived navigation to
// fields that happen to be covered, without rescanning chunks per field.
func chunkHitsByName(f *frame) map[string]objectHit {
	if f.kind != frameObject {
		return nil
	}
	out := make(map[string]objectHit)
	for i := range f.objectChunks {
		c := &f.objectChunks[i]
		switch c.Kind {
		case ObjectChunkField:
			if int(c.Name) < len(f.keys) {
				out[f.keys[c.Name]] = objectHit{
					kind:  objectHitField,
					rng:   c.Range,
					value: c.Value,
				}
			}
		case ObjectChunkFields:
			hit := objectHit{kind: objectHitFields, rng: c.Range}
			for _, name := range resolveCoveredNames(c, f.keys) {
				out[name] = hit
			}
		}
	}
	if len(out) == 0 {
		return nil
	}
	return out
}
