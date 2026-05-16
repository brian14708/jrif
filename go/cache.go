package jrif

import (
	"container/list"
	"context"
	"fmt"
	"sync"

	"golang.org/x/sync/singleflight"
)

// RangeCache is an LRU cache decorator for any RangeFetcher. It caches raw
// payload bytes by [start, length] key, bounded by total cached bytes.
//
// When BlockSize is set, requests are aligned outward to the next block
// boundary, the larger block is cached, and the requested sub-range is sliced
// out. Hits within already cached blocks avoid the round-trip entirely.
//
// Concurrent calls for the same underlying (aligned) range are coalesced via
// singleflight: only one round-trip to inner is in flight at a time per key,
// followers receive the same bytes. Each caller's context governs its own
// wait — a cancelled follower returns ctx.Err while the leader continues.
//
// The public RangeFetcher.Fetch contract still returns exactly the requested
// bytes.
//
// Safe for concurrent use.
type RangeCache struct {
	inner     RangeFetcher
	blockSize uint64

	mu      sync.Mutex
	maxByte uint64
	bytes   uint64
	lru     *list.List // front = MRU, back = LRU
	idx     map[Range]*list.Element

	sf singleflight.Group
}

type rangeCacheEntry struct {
	rng   Range
	bytes []byte
}

// NewRangeCache wraps inner with an unbounded LRU cache (no block alignment).
// Use MaxBytes / BlockSize to constrain it.
func NewRangeCache(inner RangeFetcher) *RangeCache {
	return &RangeCache{
		inner:   inner,
		maxByte: ^uint64(0),
		lru:     list.New(),
		idx:     make(map[Range]*list.Element),
	}
}

// MaxBytes sets the maximum total cached bytes before LRU eviction kicks in.
// Returns the cache for chaining.
func (c *RangeCache) MaxBytes(n uint64) *RangeCache {
	c.mu.Lock()
	c.maxByte = n
	c.mu.Unlock()
	return c
}

// BlockSize aligns underlying reads to n-byte blocks. The cache stores the
// larger block and serves sub-ranges from it. Setting 0 (default) caches only
// exact requested ranges. Returns the cache for chaining.
func (c *RangeCache) BlockSize(n uint64) *RangeCache {
	c.mu.Lock()
	c.blockSize = n
	c.mu.Unlock()
	return c
}

// CachedBytes returns the total bytes currently cached.
func (c *RangeCache) CachedBytes() uint64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.bytes
}

// CachedEntries returns the number of cached ranges.
func (c *RangeCache) CachedEntries() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return len(c.idx)
}

// Fetch implements RangeFetcher.
func (c *RangeCache) Fetch(ctx context.Context, rng Range) ([]byte, error) {
	fetchRng := c.alignToBlock(rng)
	if out, ok := c.lookupCovering(rng, fetchRng); ok {
		return out, nil
	}
	raw, err := c.fetchAndCache(ctx, fetchRng)
	if err != nil && fetchRng != rng && ctx.Err() == nil {
		// Widened read failed (likely past payload end). Retry exact range.
		fetchRng = rng
		raw, err = c.fetchAndCache(ctx, fetchRng)
	}
	if err != nil {
		return nil, err
	}
	subStart := rng[0] - fetchRng[0]
	subEnd := subStart + rng.Len()
	return raw[subStart:subEnd], nil
}

// fetchAndCache fetches the exact range from inner (coalescing concurrent
// callers via singleflight), validates the length, and inserts the bytes into
// the LRU under that range. The returned slice is owned by the cache.
//
// Followers select on their own ctx against the shared result channel, so a
// cancelled follower returns ctx.Err while the leader's inner fetch continues.
func (c *RangeCache) fetchAndCache(ctx context.Context, rng Range) ([]byte, error) {
	ch := c.sf.DoChan(sfKey(rng), func() (any, error) {
		// Re-check cache: a concurrent caller may have just inserted.
		if out, ok := c.lookupExact(rng); ok {
			return out, nil
		}
		raw, err := c.inner.Fetch(ctx, rng)
		if err != nil {
			return nil, err
		}
		raw, err = checkExactLen(rng, raw)
		if err != nil {
			return nil, err
		}
		c.insert(rng, raw)
		return raw, nil
	})
	select {
	case res := <-ch:
		if res.Err != nil {
			return nil, res.Err
		}
		return res.Val.([]byte), nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// sfKey serializes a Range for use as a singleflight key.
func sfKey(r Range) string {
	return fmt.Sprintf("%d+%d", r[0], r[1])
}

func (c *RangeCache) alignToBlock(r Range) Range {
	if c.blockSize <= 1 {
		return r
	}
	bs := c.blockSize
	start := (r[0] / bs) * bs
	end := r[0] + r[1]
	// If r is malformed and end < r[0] (overflow), bail out unaligned —
	// the inner fetch will reject it.
	if end < r[0] {
		return r
	}
	// Round end up to the next block boundary. Guard the rounding step
	// against overflow at the top of uint64 space.
	if end > ^uint64(0)-(bs-1) {
		return Range{start, end - start}
	}
	alignedEnd := ((end + bs - 1) / bs) * bs
	return Range{start, alignedEnd - start}
}

// lookupCovering returns cached bytes for request when either the exact range
// or its block-aligned super-range is cached. When blockSize is 0 (unaligned
// mode), only the exact range is checked; when blockSize > 1, aligned is the
// expected cache key so a single map lookup suffices.
func (c *RangeCache) lookupCovering(request, aligned Range) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if el, ok := c.idx[request]; ok {
		c.lru.MoveToFront(el)
		return el.Value.(*rangeCacheEntry).bytes, true
	}
	if aligned == request {
		return nil, false
	}
	el, ok := c.idx[aligned]
	if !ok {
		return nil, false
	}
	c.lru.MoveToFront(el)
	off := request[0] - aligned[0]
	return el.Value.(*rangeCacheEntry).bytes[off : off+request.Len()], true
}

// lookupExact returns a hit only when the cache holds the exact requested
// range (no covering-entry scan). Used inside the singleflight closure to
// avoid redundant inner fetches when a concurrent caller already inserted.
func (c *RangeCache) lookupExact(request Range) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	el, ok := c.idx[request]
	if !ok {
		return nil, false
	}
	c.lru.MoveToFront(el)
	return el.Value.(*rangeCacheEntry).bytes, true
}

func (c *RangeCache) insert(rng Range, bytes []byte) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if el, ok := c.idx[rng]; ok {
		c.lru.MoveToFront(el)
		return
	}
	size := rng.Len()
	for c.bytes+size > c.maxByte {
		back := c.lru.Back()
		if back == nil {
			break
		}
		entry := back.Value.(*rangeCacheEntry)
		c.lru.Remove(back)
		delete(c.idx, entry.rng)
		c.bytes -= entry.rng.Len()
	}
	el := c.lru.PushFront(&rangeCacheEntry{rng: rng, bytes: bytes})
	c.idx[rng] = el
	c.bytes += size
}
