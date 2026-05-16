package jrif

import (
	"context"
	"errors"
	"fmt"
	"io"
)

// RangeFetcher supplies payload bytes for absolute [start, length] byte
// ranges.
//
// Implementations MUST return exactly rng.Len() bytes on success and MUST
// honor ctx cancellation. The Index passes the returned []byte through to user
// code without copying — fetchers should not retain or mutate the slice after
// handing it off.
type RangeFetcher interface {
	Fetch(ctx context.Context, rng Range) ([]byte, error)
}

// InMemoryPayload is a RangeFetcher backed by an in-memory payload slice.
// Returns a slice aliasing the underlying buffer — do not mutate.
type InMemoryPayload []byte

// Fetch returns the slice payload[rng.Start():rng.End()].
func (p InMemoryPayload) Fetch(ctx context.Context, rng Range) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := checkRangeWellFormed(rng); err != nil {
		return nil, err
	}
	end := rng[0] + rng[1]
	if end > uint64(len(p)) {
		return nil, fmt.Errorf("range end %d > payload length %d", end, len(p))
	}
	return p[rng[0]:end], nil
}

// FileFetcher is a RangeFetcher backed by any io.ReaderAt (e.g. *os.File).
//
// io.ReaderAt is safe for concurrent use per its contract, so this fetcher
// supports parallel reads without internal locking. For network or
// per-request-bounded sources, layer a RangeCache on top.
//
// ctx is checked once before reading. io.ReaderAt has no cancellation hook,
// so an in-progress ReadAt cannot be interrupted by ctx.
type FileFetcher struct {
	r io.ReaderAt
}

// NewFileFetcher wraps r so it can serve range fetches.
func NewFileFetcher(r io.ReaderAt) *FileFetcher {
	return &FileFetcher{r: r}
}

// Fetch reads exactly rng.Len() bytes from offset rng.Start() of the
// underlying reader.
func (f *FileFetcher) Fetch(ctx context.Context, rng Range) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := checkRangeWellFormed(rng); err != nil {
		return nil, err
	}
	want := int(rng.Len())
	buf := make([]byte, want)
	n, err := f.r.ReadAt(buf, int64(rng[0]))
	if err != nil {
		if errors.Is(err, io.EOF) {
			return nil, fmt.Errorf("short read: got %d bytes, expected %d", n, want)
		}
		return nil, err
	}
	return buf, nil
}

// checkRangeWellFormed returns an error when rng has length 0 or its
// start + length overflows uint64.
func checkRangeWellFormed(rng Range) error {
	if rng[1] == 0 {
		return fmt.Errorf("range length is zero")
	}
	if rng[0]+rng[1] < rng[0] {
		return fmt.Errorf("range start %d + length %d overflows uint64", rng[0], rng[1])
	}
	return nil
}

// checkExactLen confirms a fetched buffer matches the requested range's
// length. Returns the buffer unchanged on success.
func checkExactLen(rng Range, b []byte) ([]byte, error) {
	if uint64(len(b)) != rng.Len() {
		return nil, fmt.Errorf("fetcher returned %d bytes for [start=%d,len=%d]; expected %d",
			len(b), rng[0], rng[1], rng.Len())
	}
	return b, nil
}
