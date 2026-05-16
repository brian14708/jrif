//! [`Source`] — the trait Index uses to pull payload bytes.
//!
//! Direct impls for `Bytes` / `Arc<[u8]>` cover the in-memory case.
//! [`BufferReader`] decorates any source with an LRU byte-range cache that
//! may internally over-fetch the wrapped source (block-aligned reads) while
//! still returning exactly the requested bytes.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

/// Source of payload bytes for an [`Index`](crate::Index).
///
/// Implementations MUST return EXACTLY `len` bytes for a successful call.
/// The trait is `Send + Sync` and the returned future is `Send` so that an
/// Index built on a multi-threaded runtime works without ceremony.
pub trait Source: Send + Sync {
    fn read_exact_at(
        &self,
        offset: u64,
        len: usize,
    ) -> impl Future<Output = io::Result<Bytes>> + Send;
}

impl Source for Bytes {
    async fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Self> {
        let (start, end) = validate_in_memory(self.len(), offset, len)?;
        Ok(self.slice(start..end))
    }
}

impl Source for Arc<[u8]> {
    async fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let (start, end) = validate_in_memory(self.len(), offset, len)?;
        Ok(Bytes::copy_from_slice(&self[start..end]))
    }
}

/// Confirm a fetched buffer matches the requested length. Returns the buffer
/// unchanged on success.
pub fn check_exact_len(offset: u64, expected: usize, bytes: Bytes) -> io::Result<Bytes> {
    if bytes.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "source returned {} bytes at offset {offset}; expected {expected}",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

fn validate_in_memory(buf_len: usize, offset: u64, len: usize) -> io::Result<(usize, usize)> {
    let start = usize::try_from(offset).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("offset {offset} exceeds usize"),
        )
    })?;
    let end = start.checked_add(len).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("offset {offset} + len {len} overflows"),
        )
    })?;
    if end > buf_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("read [{start},{end}) past payload length {buf_len}"),
        ));
    }
    Ok((start, end))
}

// ---------- BufferReader ----------

/// LRU cache decorator for any [`Source`]. Caches raw payload bytes by
/// `[start, end)` key, bounded by total cached bytes.
///
/// May internally over-fetch the wrapped source when `block_size` is set —
/// requests are aligned outward to the next block boundary, the larger block
/// is cached, and the requested sub-range is sliced out. Hits within already
/// cached blocks avoid the network/disk round-trip entirely.
///
/// The public [`Source::read_exact_at`] contract still returns exactly the
/// requested bytes.
pub struct BufferReader<S> {
    inner: S,
    state: Mutex<CacheState>,
    block_size: u64, // 0 = no block alignment, cache exact-range hits only
}

struct CacheState {
    entries: HashMap<Range<u64>, Node>,
    head: Option<Range<u64>>,
    tail: Option<Range<u64>>,
    bytes: u64,
    max_bytes: u64,
}

struct Node {
    bytes: Bytes,
    prev: Option<Range<u64>>,
    next: Option<Range<u64>>,
}

impl<S> BufferReader<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            state: Mutex::new(CacheState {
                entries: HashMap::new(),
                head: None,
                tail: None,
                bytes: 0,
                max_bytes: u64::MAX,
            }),
            block_size: 0,
        }
    }

    /// Maximum total cached bytes before LRU eviction kicks in.
    #[must_use]
    pub fn max_bytes(mut self, n: u64) -> Self {
        self.state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .max_bytes = n;
        self
    }

    /// Align underlying reads to `n`-byte blocks. The cache stores the larger
    /// block and serves sub-ranges from it. Set to 0 (default) to cache only
    /// exact requested ranges.
    #[must_use]
    pub const fn block_size(mut self, n: u64) -> Self {
        self.block_size = n;
        self
    }

    /// Total bytes currently cached.
    pub fn cached_bytes(&self) -> u64 {
        self.lock_state().bytes
    }

    /// Number of cached entries.
    pub fn cached_entries(&self) -> usize {
        self.lock_state().entries.len()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<S: Source> Source for BufferReader<S> {
    async fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let end = offset.checked_add(len as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("offset {offset} + len {len} overflows"),
            )
        })?;
        let request = offset..end;
        if let Some(slice) = self.lookup(&request) {
            return Ok(slice);
        }

        let fetch = self.align_to_block(&request);
        let fetch_len = usize::try_from(fetch.end - fetch.start).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "block-aligned len exceeds usize",
            )
        })?;
        let (used, raw) = match self.inner.read_exact_at(fetch.start, fetch_len).await {
            Ok(b) => (fetch.clone(), check_exact_len(fetch.start, fetch_len, b)?),
            Err(e) if fetch != request => {
                // Widened read failed (likely past payload end). Retry the
                // exact requested range.
                let b = self.inner.read_exact_at(offset, len).await.map_err(|_| e)?;
                (request.clone(), check_exact_len(offset, len, b)?)
            }
            Err(e) => return Err(e),
        };

        let sub_start =
            usize::try_from(offset - used.start).expect("sub-range offset fits in usize");
        let out = raw.slice(sub_start..sub_start + len);
        self.insert(used, raw);
        Ok(out)
    }
}

impl<S> BufferReader<S> {
    fn align_to_block(&self, r: &Range<u64>) -> Range<u64> {
        if self.block_size <= 1 {
            return r.clone();
        }
        let bs = self.block_size;
        let start = (r.start / bs) * bs;
        // Round end up to the next block boundary; cap on overflow.
        let end = r
            .end
            .checked_add(bs - 1)
            .map_or(u64::MAX, |v| (v / bs) * bs);
        let end = end.max(r.end);
        start..end
    }

    fn lookup(&self, request: &Range<u64>) -> Option<Bytes> {
        let mut st = self.lock_state();
        // Fast path: exact hit.
        if let Some(node) = st.entries.get(request) {
            let bytes = node.bytes.clone();
            touch(&mut st, request.clone());
            return Some(bytes);
        }
        // Slow path: scan for a covering entry. Range counts are bounded by
        // max_bytes / block_size, so this is small in practice.
        let covering = st
            .entries
            .iter()
            .find(|(k, _)| k.start <= request.start && request.end <= k.end)
            .map(|(k, n)| (k.clone(), n.bytes.clone()));
        drop(st);
        if let Some((key, bytes)) = covering {
            let off = usize::try_from(request.start - key.start).expect("offset fits in usize");
            let len = usize::try_from(request.end - request.start).expect("length fits in usize");
            let out = bytes.slice(off..off + len);
            touch(&mut self.lock_state(), key);
            return Some(out);
        }
        None
    }

    fn insert(&self, range: Range<u64>, bytes: Bytes) {
        let mut st = self.lock_state();
        let size = range.end - range.start;
        if st.entries.contains_key(&range) {
            touch(&mut st, range);
            return;
        }
        evict_until_fits(&mut st, size);
        let old_head = st.head.take();
        let node = Node {
            bytes,
            prev: None,
            next: old_head.clone(),
        };
        st.entries.insert(range.clone(), node);
        if let Some(h) = old_head
            && let Some(h_node) = st.entries.get_mut(&h)
        {
            h_node.prev = Some(range.clone());
        }
        if st.tail.is_none() {
            st.tail = Some(range.clone());
        }
        st.head = Some(range);
        st.bytes += size;
    }
}

fn touch(st: &mut CacheState, key: Range<u64>) {
    if st.head.as_ref() == Some(&key) {
        return;
    }
    let (prev, next) = match st.entries.get(&key) {
        Some(n) => (n.prev.clone(), n.next.clone()),
        None => return,
    };
    if let Some(p) = prev.as_ref()
        && let Some(pn) = st.entries.get_mut(p)
    {
        pn.next.clone_from(&next);
    }
    if let Some(n) = next.as_ref()
        && let Some(nn) = st.entries.get_mut(n)
    {
        nn.prev.clone_from(&prev);
    }
    if st.tail.as_ref() == Some(&key) {
        st.tail = prev;
    }
    let old_head = st.head.take();
    if let Some(node) = st.entries.get_mut(&key) {
        node.prev = None;
        node.next.clone_from(&old_head);
    }
    if let Some(h) = old_head
        && let Some(hn) = st.entries.get_mut(&h)
    {
        hn.prev = Some(key.clone());
    }
    st.head = Some(key);
}

fn evict_until_fits(st: &mut CacheState, incoming: u64) {
    while st.bytes + incoming > st.max_bytes {
        let Some(tail_key) = st.tail.take() else {
            break;
        };
        let node = st.entries.remove(&tail_key).expect("tail entry");
        st.bytes -= tail_key.end - tail_key.start;
        if let Some(p) = node.prev.as_ref()
            && let Some(pn) = st.entries.get_mut(p)
        {
            pn.next = None;
        }
        st.tail = node.prev;
        if st.head.as_ref() == Some(&tail_key) {
            st.head = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn bytes_source_returns_exact_range() {
        let b: Bytes = (0..100u8).collect::<Vec<_>>().into();
        let out = b.read_exact_at(10, 10).await.unwrap();
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], 10);
        assert_eq!(out[9], 19);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_source_rejects_out_of_bounds() {
        let b: Bytes = (0..100u8).collect::<Vec<_>>().into();
        let err = b.read_exact_at(95, 10).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cache_serves_sub_ranges_from_block() {
        let b: Bytes = (0..200u8).collect::<Vec<_>>().into();
        let cache = BufferReader::new(b).block_size(64);
        // First read primes the cache (block-aligned).
        let _ = cache.read_exact_at(70, 11).await.unwrap();
        let entries_after_first = cache.cached_entries();
        // Second read within the same block must not grow the cache.
        let out = cache.read_exact_at(75, 11).await.unwrap();
        assert_eq!(out.len(), 11);
        assert_eq!(out[0], 75);
        assert_eq!(cache.cached_entries(), entries_after_first);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cache_evicts_oldest_when_bound_exceeded() {
        let b: Bytes = (0..200u8).collect::<Vec<_>>().into();
        let cache = BufferReader::new(b).max_bytes(20);
        let _ = cache.read_exact_at(0, 10).await.unwrap();
        let _ = cache.read_exact_at(10, 10).await.unwrap();
        let _ = cache.read_exact_at(20, 10).await.unwrap(); // forces eviction
        assert!(cache.cached_bytes() <= 20);
    }
}
