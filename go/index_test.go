package jrif

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func loadFixture(t *testing.T) (payload, jrif []byte) {
	t.Helper()
	payload, err := os.ReadFile("testdata/sample.json")
	if err != nil {
		t.Fatalf("read sample.json: %v", err)
	}
	jrif, err = os.ReadFile("testdata/sample.json.jrif")
	if err != nil {
		t.Fatalf("read sample.json.jrif: %v", err)
	}
	return payload, jrif
}

// recordingFetcher wraps an in-memory fetcher and records every range it serves.
type recordingFetcher struct {
	inner    InMemoryPayload
	mu       sync.Mutex
	fetches  []Range
	failures atomic.Int64
}

func newRecordingFetcher(payload []byte) *recordingFetcher {
	return &recordingFetcher{inner: InMemoryPayload(payload)}
}

func (r *recordingFetcher) Fetch(ctx context.Context, rng Range) ([]byte, error) {
	r.mu.Lock()
	r.fetches = append(r.fetches, rng)
	r.mu.Unlock()
	b, err := r.inner.Fetch(ctx, rng)
	if err != nil {
		r.failures.Add(1)
	}
	return b, err
}

func (r *recordingFetcher) snapshot() []Range {
	r.mu.Lock()
	defer r.mu.Unlock()
	out := make([]Range, len(r.fetches))
	copy(out, r.fetches)
	return out
}

func openOrFatal(t *testing.T, jrif []byte, f RangeFetcher) *Index {
	t.Helper()
	idx, err := Open(jrif, f)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	return idx
}

func TestGetOnChunkIndexDoesNotFetch(t *testing.T) {
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	// metadata is an object-chunked Field — descending into it should walk the
	// index without fetching.
	_ = idx.Root().Get("metadata")
	if got := len(fetcher.snapshot()); got != 0 {
		t.Fatalf("expected zero fetches, got %d", got)
	}
}

func TestValueFetchesExactlyTheChunkRange(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	metadata := idx.Root().Get("metadata")
	rng, ok := metadata.Range()
	if !ok {
		t.Fatal("expected range while index-backed")
	}
	v, err := metadata.Value(ctx)
	if err != nil {
		t.Fatalf("Value: %v", err)
	}
	if _, ok := v.(map[string]any); !ok {
		t.Fatalf("expected object, got %T", v)
	}
	fetches := fetcher.snapshot()
	if len(fetches) != 1 {
		t.Fatalf("expected 1 fetch, got %d (%v)", len(fetches), fetches)
	}
	if fetches[0] != rng {
		t.Fatalf("expected fetch %v, got %v", rng, fetches[0])
	}
}

func TestNestedNavigationThroughChunks(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	name := idx.Root().Get("records").Index(1).Get("name")
	got, err := name.AsString(ctx)
	if err != nil {
		t.Fatalf("AsString: %v", err)
	}
	if got != "bob" {
		t.Fatalf("expected 'bob', got %q", got)
	}
	fetches := fetcher.snapshot()
	if len(fetches) > 2 {
		t.Fatalf("expected ≤2 fetches for records[1].name; got %d (%v)", len(fetches), fetches)
	}
	var total uint64
	for _, f := range fetches {
		total += f.Len()
	}
	if total >= uint64(len(payload)) {
		t.Fatalf("lazy navigation must not fetch the whole payload (%d ≥ %d)", total, len(payload))
	}
}

func TestValueMaterializesRootWhenNeeded(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	v, err := idx.Root().Value(ctx)
	if err != nil {
		t.Fatalf("root.Value: %v", err)
	}
	obj, ok := v.(map[string]any)
	if !ok {
		t.Fatalf("expected object root, got %T", v)
	}
	if _, ok := obj["records"]; !ok {
		t.Fatalf("missing records in materialized root")
	}
	fetches := fetcher.snapshot()
	if len(fetches) != 1 {
		t.Fatalf("expected 1 fetch, got %d", len(fetches))
	}
	rootRange := idx.Document().Root.Range
	if fetches[0] != rootRange {
		t.Fatalf("expected root-range fetch %v, got %v", rootRange, fetches[0])
	}
	_ = payload
}

func TestGetFallsBackToParseWhenIndexCantDrillDeeper(t *testing.T) {
	ctx := context.Background()
	// metadata is one object chunk with no nested chunks for its inner fields.
	// Asking for metadata.version forces fetch + parse of the metadata range.
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	version := idx.Root().Get("metadata").Get("version")
	if hint, ok := version.JSONTypeHint(); ok {
		t.Fatalf("type hint should be unknown for pending cursor, got %q", hint)
	}
	got, err := version.AsInt64(ctx)
	if err != nil {
		t.Fatalf("AsInt64: %v", err)
	}
	if got != 1 {
		t.Fatalf("expected 1, got %d", got)
	}
	fetches := fetcher.snapshot()
	if len(fetches) != 1 {
		t.Fatalf("expected one fetch for metadata bytes, got %d (%v)", len(fetches), fetches)
	}
}

func TestFieldNotFoundAfterFallback(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	_, err := idx.Root().Get("metadata").Get("nope").Value(ctx)
	if err == nil {
		t.Fatal("expected NotFoundError")
	}
	var nf *NotFoundError
	if !errors.As(err, &nf) {
		t.Fatalf("expected *NotFoundError, got %T: %v", err, err)
	}
	if got := nf.Path.String(); got != "$.metadata.nope" {
		t.Fatalf("expected path $.metadata.nope, got %s", got)
	}
}

func TestIndexOutOfBounds(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	_, err := idx.Root().Get("records").Index(99).Value(ctx)
	if err == nil {
		t.Fatal("expected NotFoundError")
	}
	var nf *NotFoundError
	if !errors.As(err, &nf) {
		t.Fatalf("expected *NotFoundError, got %T: %v", err, err)
	}
}

func TestBytesReturnsRawWhenResolved(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	metadata := idx.Root().Get("metadata")
	rng, ok := metadata.Range()
	if !ok {
		t.Fatal("metadata cursor should be resolved")
	}
	b, err := metadata.Bytes(ctx)
	if err != nil {
		t.Fatalf("Bytes: %v", err)
	}
	if !bytes.Equal(b, payload[rng.Start():rng.End()]) {
		t.Fatalf("Bytes did not return raw payload slice")
	}
}

func TestDeserializeIntoStruct(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	type record struct {
		Name string `json:"name"`
	}
	var rec record
	if err := idx.Root().Get("records").Index(0).Deserialize(ctx, &rec); err != nil {
		t.Fatalf("Deserialize: %v", err)
	}
	if rec.Name == "" {
		t.Fatalf("expected non-empty name, got %q", rec.Name)
	}
}

func TestLenFromChunkIndexWithoutFetch(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	n, err := idx.Root().Get("records").Len(ctx)
	if err != nil {
		t.Fatalf("Len: %v", err)
	}
	if n < 2 {
		t.Fatalf("expected at least 2 records, got %d", n)
	}
	if got := len(fetcher.snapshot()); got != 0 {
		t.Fatalf("Len should not have fetched, got %d fetches", got)
	}
}

func TestIterYieldsEachItem(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	it, err := idx.Root().Get("records").Iter(ctx)
	if err != nil {
		t.Fatalf("Iter: %v", err)
	}
	var names []string
	for {
		c, ok := it.Next()
		if !ok {
			break
		}
		name, err := c.Get("name").AsString(ctx)
		if err != nil {
			t.Fatalf("AsString: %v", err)
		}
		names = append(names, name)
	}
	if len(names) == 0 || names[0] != "alice" {
		t.Fatalf("unexpected names: %v", names)
	}
}

func TestEntriesYieldsFieldsInSourceOrder(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	it, err := idx.Root().Entries(ctx)
	if err != nil {
		t.Fatalf("Entries: %v", err)
	}
	var keys []string
	for {
		name, _, ok := it.Next()
		if !ok {
			break
		}
		keys = append(keys, name)
	}
	if len(keys) == 0 {
		t.Fatal("expected at least one entry")
	}
}

func TestJSONTypeHintForChunkBackedCursors(t *testing.T) {
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	if hint, ok := idx.Root().JSONTypeHint(); !ok || hint != JSONTypeObject {
		t.Fatalf("root hint: ok=%v hint=%q", ok, hint)
	}
	if hint, ok := idx.Root().Get("records").JSONTypeHint(); !ok || hint != JSONTypeArray {
		t.Fatalf("records hint: ok=%v hint=%q", ok, hint)
	}
}

func TestRangeCacheServesSubRangeFromBlock(t *testing.T) {
	ctx := context.Background()
	inner := newRecordingFetcher(make([]byte, 200))
	for i := range inner.inner {
		inner.inner[i] = byte(i)
	}
	cache := NewRangeCache(inner).BlockSize(64)
	first, err := cache.Fetch(ctx, Range{70, 11})
	if err != nil {
		t.Fatalf("first fetch: %v", err)
	}
	if first[0] != 70 || first[10] != 80 {
		t.Fatalf("first slice wrong: %v", first)
	}
	before := cache.CachedEntries()
	second, err := cache.Fetch(ctx, Range{75, 11})
	if err != nil {
		t.Fatalf("second fetch: %v", err)
	}
	if second[0] != 75 || second[10] != 85 {
		t.Fatalf("second slice wrong: %v", second)
	}
	if cache.CachedEntries() != before {
		t.Fatalf("cache grew on covered-range hit: %d → %d", before, cache.CachedEntries())
	}
	// Inner should only have been hit once.
	if got := len(inner.snapshot()); got != 1 {
		t.Fatalf("expected 1 inner fetch, got %d", got)
	}
}

func TestRangeCacheEvictsOldest(t *testing.T) {
	ctx := context.Background()
	inner := InMemoryPayload(make([]byte, 200))
	cache := NewRangeCache(inner).MaxBytes(20)
	if _, err := cache.Fetch(ctx, Range{0, 10}); err != nil {
		t.Fatal(err)
	}
	if _, err := cache.Fetch(ctx, Range{10, 10}); err != nil {
		t.Fatal(err)
	}
	if _, err := cache.Fetch(ctx, Range{20, 10}); err != nil {
		t.Fatal(err)
	}
	if cache.CachedBytes() > 20 {
		t.Fatalf("expected ≤20 cached bytes, got %d", cache.CachedBytes())
	}
}

func TestRangeCacheRetriesWhenWidenedReadFails(t *testing.T) {
	ctx := context.Background()
	inner := InMemoryPayload(make([]byte, 100))
	cache := NewRangeCache(inner).BlockSize(64)
	// Range [90, length=10] aligned to 64-block would cover [64, 128), which
	// is past the 100-byte payload. The cache must retry with the exact range.
	out, err := cache.Fetch(ctx, Range{90, 10})
	if err != nil {
		t.Fatalf("fetch: %v", err)
	}
	if uint64(len(out)) != 10 {
		t.Fatalf("expected 10 bytes, got %d", len(out))
	}
}

func TestFileFetcherReadsRange(t *testing.T) {
	ctx := context.Background()
	data := make([]byte, 100)
	for i := range data {
		data[i] = byte(i)
	}
	f := NewFileFetcher(bytes.NewReader(data))
	out, err := f.Fetch(ctx, Range{10, 10})
	if err != nil {
		t.Fatalf("Fetch: %v", err)
	}
	if uint64(len(out)) != 10 {
		t.Fatalf("expected 10 bytes, got %d", len(out))
	}
	if out[0] != 10 || out[9] != 19 {
		t.Fatalf("unexpected bytes: %v", out)
	}
}

func TestFileFetcherShortReadIsAnError(t *testing.T) {
	ctx := context.Background()
	// 10-byte source, ask for [5, length=15] — short by 10 bytes.
	f := NewFileFetcher(bytes.NewReader(make([]byte, 10)))
	_, err := f.Fetch(ctx, Range{5, 15})
	if err == nil {
		t.Fatal("expected short read error")
	}
	if !strings.Contains(err.Error(), "short read") && !errors.Is(err, io.EOF) {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestFileFetcherEndToEnd(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	f := NewFileFetcher(bytes.NewReader(payload))
	idx, err := Open(jrif, f)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	got, err := idx.Root().Get("records").Index(0).Get("name").AsString(ctx)
	if err != nil {
		t.Fatalf("AsString: %v", err)
	}
	if got != "alice" {
		t.Fatalf("expected alice, got %q", got)
	}
}

func TestDecodeJSONRejectsTrailingData(t *testing.T) {
	doc := &Document{
		Jrif: JrifV0,
		Root: Value{Type: ValueValue, Inline: []byte("123")},
	}
	r, err := OpenDocument(doc, InMemoryPayload([]byte("1 2 3")))
	if err != nil {
		t.Fatalf("OpenDocument: %v", err)
	}
	cursor := r.Root()
	if _, err := decodeJSON([]byte("1 2"), cursor.pathCopy); err == nil {
		t.Fatal("expected error for trailing JSON")
	}
	if _, err := decodeJSON([]byte("1"), cursor.pathCopy); err != nil {
		t.Fatalf("expected success for single value, got %v", err)
	}
}

func TestAsUint64AboveMaxInt64(t *testing.T) {
	ctx := context.Background()
	const big = "18446744073709551610"
	payload := []byte(big)
	doc := &Document{
		Jrif: JrifV0,
		Root: Value{
			Type:   ValueValue,
			Inline: []byte(big),
		},
	}
	r, err := OpenDocument(doc, InMemoryPayload(payload))
	if err != nil {
		t.Fatalf("OpenDocument: %v", err)
	}
	got, err := r.Root().AsUint64(ctx)
	if err != nil {
		t.Fatalf("AsUint64: %v", err)
	}
	if got != 18446744073709551610 {
		t.Fatalf("expected 18446744073709551610, got %d", got)
	}
}

func TestPathRenderingOnDeepError(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	// Force a type mismatch: records[0] is an object, not an array, so
	// .Index(0) on it should fail when resolved.
	_, err := idx.Root().Get("records").Index(0).Index(7).Value(ctx)
	if err == nil {
		t.Fatal("expected type mismatch")
	}
	var tm *TypeMismatchError
	if !errors.As(err, &tm) {
		t.Fatalf("expected TypeMismatchError, got %T: %v", err, err)
	}
	want := "$.records[0][7]"
	if tm.Path.String() != want {
		t.Fatalf("expected path %s, got %s", want, tm.Path)
	}
}

// chainErrFetcher always fails. Used to check error paths.
type chainErrFetcher struct{}

func (chainErrFetcher) Fetch(context.Context, Range) ([]byte, error) {
	return nil, fmt.Errorf("disk on fire")
}

func TestFetchErrorCarriesPath(t *testing.T) {
	ctx := context.Background()
	_, jrif := loadFixture(t)
	idx, err := Open(jrif, chainErrFetcher{})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	_, err = idx.Root().Get("metadata").Value(ctx)
	var fe *FetchError
	if !errors.As(err, &fe) {
		t.Fatalf("expected FetchError, got %T: %v", err, err)
	}
	if fe.Path.String() != "$.metadata" {
		t.Fatalf("expected path $.metadata, got %s", fe.Path)
	}
}

// Ensure encoding/json's number-precision behavior is preserved.
func TestValuePreservesNumberPrecision(t *testing.T) {
	ctx := context.Background()
	const v = `9223372036854775807`
	payload := []byte(v)
	doc := &Document{
		Jrif: JrifV0,
		Root: Value{
			Type:   ValueValue,
			Inline: []byte(v),
		},
	}
	idx, err := OpenDocument(doc, InMemoryPayload(payload))
	if err != nil {
		t.Fatalf("OpenDocument: %v", err)
	}
	got, err := idx.Root().Value(ctx)
	if err != nil {
		t.Fatalf("Value: %v", err)
	}
	n, ok := got.(json.Number)
	if !ok {
		t.Fatalf("expected json.Number, got %T", got)
	}
	if string(n) != v {
		t.Fatalf("number was lossy: got %q", n)
	}
}

// strictUnmarshal must reject a JRIF document with trailing bytes after the
// outermost object.
func TestOpenRejectsTrailingBytes(t *testing.T) {
	_, jrif := loadFixture(t)
	trailing := append([]byte{}, jrif...)
	trailing = append(trailing, '{', '}')
	if _, err := Open(trailing, InMemoryPayload(nil)); err == nil {
		t.Fatal("expected error for trailing bytes, got nil")
	}
}

// Entries must preserve source order even when navigation defers through a
// "fields" chunk (slow path with pending segments).
func TestEntriesPreservesOrderThroughPendingSegments(t *testing.T) {
	ctx := context.Background()
	// A record's inner fields are described by one "fields" chunk listing
	// id, name, score, notes. Descending into that record yields a cursor
	// with pending=[item ordinal 0], so Entries() must walk the bytes to
	// recover source order rather than re-marshalling a map.
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	rec := idx.Root().Get("records").Index(0)
	it, err := rec.Entries(ctx)
	if err != nil {
		t.Fatalf("Entries: %v", err)
	}
	want := []string{"id", "name", "score", "notes"}
	var got []string
	for {
		name, _, ok := it.Next()
		if !ok {
			break
		}
		got = append(got, name)
	}
	if len(got) != len(want) {
		t.Fatalf("expected %d keys, got %d: %v", len(want), len(got), got)
	}
	for i, k := range want {
		if got[i] != k {
			t.Fatalf("key[%d] = %q, want %q (full order: %v)", i, got[i], k, got)
		}
	}
}

// slowBlockingFetcher delays each Fetch by gating on a channel, so the test
// can interleave concurrent callers deterministically.
type slowBlockingFetcher struct {
	inner   InMemoryPayload
	release chan struct{}
	calls   atomic.Int64
}

func (f *slowBlockingFetcher) Fetch(ctx context.Context, rng Range) ([]byte, error) {
	f.calls.Add(1)
	select {
	case <-f.release:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	return f.inner.Fetch(ctx, rng)
}

// TestRangeCacheSingleflightCoalescesConcurrentMisses: two goroutines
// racing on the same uncached range must trigger only one inner.Fetch.
func TestRangeCacheSingleflightCoalescesConcurrentMisses(t *testing.T) {
	ctx := context.Background()
	payload := make([]byte, 256)
	for i := range payload {
		payload[i] = byte(i)
	}
	inner := &slowBlockingFetcher{
		inner:   InMemoryPayload(payload),
		release: make(chan struct{}),
	}
	cache := NewRangeCache(inner)

	rng := Range{10, 91}
	var wg sync.WaitGroup
	results := make([][]byte, 2)
	errs := make([]error, 2)
	for i := 0; i < 2; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			results[idx], errs[idx] = cache.Fetch(ctx, rng)
		}(i)
	}
	// Spin until both goroutines are inside inner.Fetch (or, well, at least one
	// is — singleflight is supposed to coalesce so the second waits before it
	// reaches inner.Fetch). Give the second goroutine time to join.
	for i := 0; i < 100 && inner.calls.Load() == 0; i++ {
		time.Sleep(100 * time.Microsecond)
	}
	time.Sleep(2 * time.Millisecond) // let follower park on the singleflight
	close(inner.release)
	wg.Wait()

	for i := 0; i < 2; i++ {
		if errs[i] != nil {
			t.Fatalf("goroutine %d: %v", i, errs[i])
		}
		if uint64(len(results[i])) != rng.Len() {
			t.Fatalf("goroutine %d: wrong len %d", i, len(results[i]))
		}
	}
	if got := inner.calls.Load(); got != 1 {
		t.Fatalf("expected 1 inner.Fetch, got %d", got)
	}
}

// TestCursorBytesRespectsCanceledContext checks ctx cancellation surfaces
// from a leaf call through the fetcher.
func TestCursorBytesRespectsCanceledContext(t *testing.T) {
	payload, jrif := loadFixture(t)
	idx := openOrFatal(t, jrif, InMemoryPayload(payload))
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	_, err := idx.Root().Get("metadata").Bytes(ctx)
	if err == nil {
		t.Fatal("expected error from canceled ctx")
	}
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("expected context.Canceled, got %v", err)
	}
}

// TestIterAvoidsRescanningChunks verifies the [ArrayIter] walker only
// requires one chunk-walk total, not one per Next call. The fixture's
// `records` array is described by a single `items` chunk, so descending into
// each item must defer to pending without fetching — but the walker still
// has to advance N times, not 0+1+2+...+N times.
//
// We can't directly observe chunk scans, but we can observe that iterating
// performs no extra fetches beyond the per-item leaf accessor, which is the
// behavior callers care about.
func TestIterAvoidsRescanningChunks(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	fetcher := newRecordingFetcher(payload)
	idx := openOrFatal(t, jrif, fetcher)
	it, err := idx.Root().Get("records").Iter(ctx)
	if err != nil {
		t.Fatalf("Iter: %v", err)
	}
	// Iter on a chunk-described array must not fetch.
	if got := len(fetcher.snapshot()); got != 0 {
		t.Fatalf("Iter should not fetch, got %d", got)
	}
	for {
		c, ok := it.Next()
		if !ok {
			break
		}
		if c == nil {
			t.Fatal("nil cursor from Next")
		}
	}
	// Next alone must not fetch (each item's cursor is index-resolved).
	if got := len(fetcher.snapshot()); got != 0 {
		t.Fatalf("Next should not fetch, got %d", got)
	}
}

// TestEntriesIteratesPartiallyChunkedObject verifies that Entries does not
// silently drop fields when an object's chunks only cover a subset of its
// members (allowed by JRIF v0 spec). Constructs a document whose root is
// `{"covered":1,"uncovered":2}` with a single `field` chunk for "covered"
// only, and confirms both keys are iterated in source order.
func TestEntriesIteratesPartiallyChunkedObject(t *testing.T) {
	ctx := context.Background()
	payload := []byte(`{"covered":1,"uncovered":2}`)
	// `field` chunks cover the value only, excluding the member name and colon.
	// `"covered":1` puts value `1` at byte index 11.
	coveredValueRange := Range{11, 1}
	doc := &Document{
		Jrif: JrifV0,
		Keys: []string{"covered"},
		Root: Value{
			Type:  ValueObject,
			Range: Range{0, uint64(len(payload))},
			ObjectChunks: []ObjectChunk{{
				Kind:  ObjectChunkField,
				Range: coveredValueRange,
				Name:  0,
				Value: &Value{
					Type:   ValueValue,
					Inline: []byte("1"),
				},
			}},
		},
	}
	idx, err := OpenDocument(doc, InMemoryPayload(payload))
	if err != nil {
		t.Fatalf("OpenDocument: %v", err)
	}
	it, err := idx.Root().Entries(ctx)
	if err != nil {
		t.Fatalf("Entries: %v", err)
	}
	var got []string
	for {
		name, _, ok := it.Next()
		if !ok {
			break
		}
		got = append(got, name)
	}
	want := []string{"covered", "uncovered"}
	if len(got) != len(want) {
		t.Fatalf("expected %d entries, got %d: %v", len(want), len(got), got)
	}
	for i, k := range want {
		if got[i] != k {
			t.Fatalf("key[%d] = %q, want %q (full order: %v)", i, got[i], k, got)
		}
	}
	// And both values resolve.
	it2, _ := idx.Root().Entries(ctx)
	for {
		name, c, ok := it2.Next()
		if !ok {
			break
		}
		n, err := c.AsInt64(ctx)
		if err != nil {
			t.Fatalf("AsInt64 for %s: %v", name, err)
		}
		switch name {
		case "covered":
			if n != 1 {
				t.Fatalf("covered: got %d, want 1", n)
			}
		case "uncovered":
			if n != 2 {
				t.Fatalf("uncovered: got %d, want 2", n)
			}
		}
	}
}

// TestInlineRootSkipsPayloadFetch verifies that an index whose root is an
// inline value resolves entirely without payload I/O.
func TestInlineRootSkipsPayloadFetch(t *testing.T) {
	ctx := context.Background()
	cases := []struct {
		name string
		v    Value
		want string
	}{
		{"null", Value{Type: ValueValue, Inline: []byte("null")}, "null"},
		{"true", Value{Type: ValueValue, Inline: []byte("true")}, "true"},
		{"42", Value{Type: ValueValue, Inline: []byte("42")}, "42"},
		{"emptyArray", Value{Type: ValueValue, Inline: []byte("[]")}, "[]"},
		{"emptyObject", Value{Type: ValueValue, Inline: []byte("{}")}, "{}"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fetcher := newRecordingFetcher(nil)
			doc := &Document{Jrif: JrifV0, Root: tc.v}
			idx, err := OpenDocument(doc, fetcher)
			if err != nil {
				t.Fatalf("OpenDocument: %v", err)
			}
			b, err := idx.Root().Bytes(ctx)
			if err != nil {
				t.Fatalf("Bytes: %v", err)
			}
			if string(b) != tc.want {
				t.Fatalf("Bytes: got %s, want %s", b, tc.want)
			}
			if got := fetcher.snapshot(); len(got) != 0 {
				t.Fatalf("expected zero fetches for inline root, got %d: %v", len(got), got)
			}
		})
	}
}

// TestKeysRoundTripFromFixture exercises the sample fixture (which carries
// `keys` and integer-indexed Fields chunks) end-to-end through the reader.
func TestKeysRoundTripFromFixture(t *testing.T) {
	ctx := context.Background()
	payload, jrif := loadFixture(t)
	// Confirm the fixture carries `keys`.
	var raw struct {
		Keys []string `json:"keys"`
	}
	if err := json.Unmarshal(jrif, &raw); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if len(raw.Keys) == 0 {
		t.Fatal("fixture should carry `keys`")
	}
	idx, err := Open(jrif, InMemoryPayload(payload))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	// Records resolution should work through the integer-indexed Fields chunks.
	var name string
	if err := idx.Root().Get("records").Index(1).Get("name").Deserialize(ctx, &name); err != nil {
		t.Fatalf("Deserialize: %v", err)
	}
	if name != "bob" {
		t.Fatalf("expected bob, got %q", name)
	}
}
