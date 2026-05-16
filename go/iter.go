package jrif

import "context"

// ArrayIter iterates over the items of an array cursor in source order. Built
// by [Cursor.Iter], which resolves the array length once (possibly fetching
// and parsing) before handing back the iterator.
//
// When the array's chunk index fully describes its items (the common case),
// Next walks chunks sequentially in O(1) amortized per call. When chunks are
// absent or the cursor is wrapping a chunk fragment, Next falls back to
// [Cursor.Index] per ordinal — correct but with worse asymptotics.
type ArrayIter struct {
	root *Cursor
	len  uint64
	next uint64
	walk *arrayWalker // nil → slow path (per-ordinal Cursor.Index)
}

// Len returns the total number of items in the array.
func (it *ArrayIter) Len() uint64 { return it.len }

// Next yields the cursor for the next item. ok is false when iteration is
// complete.
func (it *ArrayIter) Next() (c *Cursor, ok bool) {
	if it.next >= it.len {
		return nil, false
	}
	if it.walk != nil {
		hit, ord, ok := it.walk.next()
		if ok {
			it.next++
			return it.root.applyArrayHit(hit, ord), true
		}
		// Walker exhausted unexpectedly (len disagreed with chunks). Disable
		// the walker and fall through to the slow path.
		it.walk = nil
	}
	c = it.root.Index(it.next)
	it.next++
	return c, true
}

// Reset rewinds the iterator to position 0.
func (it *ArrayIter) Reset() {
	it.next = 0
	if it.walk != nil {
		it.walk = newArrayWalker(&it.root.frame)
	}
}

// ObjectIter iterates over the (field name, child cursor) pairs of an object
// cursor in source order. Built by [Cursor.Entries].
//
// When the object's chunk index fully describes its fields (the common case),
// Next resolves each child without rescanning chunks. When the chunk index
// can't describe the object (deferred / wrapped path), Entries parses the
// bytes to recover key order and Next falls back to [Cursor.Get] per key.
type ObjectIter struct {
	root    *Cursor
	entries []objectEntry // pre-walked when chunk-described; otherwise name-only
	next    int
}

// Len returns the total number of fields.
func (it *ObjectIter) Len() int { return len(it.entries) }

// Next yields the name and child cursor for the next field. ok is false when
// iteration is complete.
func (it *ObjectIter) Next() (name string, c *Cursor, ok bool) {
	if it.next >= len(it.entries) {
		return "", nil, false
	}
	e := it.entries[it.next]
	it.next++
	if e.hit.kind == objectHitNone {
		return e.name, it.root.Get(e.name), true
	}
	return e.name, it.root.applyObjectHit(e.hit, e.name), true
}

// Reset rewinds the iterator to position 0.
func (it *ObjectIter) Reset() { it.next = 0 }

// Iter resolves the array length, then returns an iterator that yields one
// child Cursor per element. ctx governs any I/O performed to resolve the
// length (fast path: zero fetches).
func (c *Cursor) Iter(ctx context.Context) (*ArrayIter, error) {
	n, err := c.Len(ctx)
	if err != nil {
		return nil, err
	}
	return &ArrayIter{root: c, len: n, walk: newArrayWalker(&c.frame)}, nil
}

// Entries resolves the object's field names in source order, then returns an
// iterator that yields (name, child cursor) pairs. ctx governs the fetch +
// parse required to recover key order (object chunks may cover only a subset
// of fields per spec, so we can't enumerate keys from the chunk index alone).
//
// For each key that is described by an object chunk, the iterator descends
// via that chunk without rescanning. Uncovered fields fall back to
// [Cursor.Get], which itself defers to a parse on first I/O.
func (c *Cursor) Entries(ctx context.Context) (*ObjectIter, error) {
	keys, err := c.objectKeys(ctx)
	if err != nil {
		return nil, err
	}
	var hitsByName map[string]objectHit
	if len(c.pending) == 0 && c.frame.kind == frameObject {
		hitsByName = chunkHitsByName(&c.frame)
	}
	entries := make([]objectEntry, len(keys))
	for i, k := range keys {
		entries[i].name = k
		if hitsByName != nil {
			entries[i].hit = hitsByName[k]
		}
	}
	return &ObjectIter{root: c, entries: entries}, nil
}
