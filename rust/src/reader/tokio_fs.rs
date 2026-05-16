//! Tokio-backed [`Source`] implementation for any `AsyncSeek + AsyncRead`.

use std::io;
use std::io::SeekFrom;
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

use crate::reader::source::Source;

/// [`Source`] over any user-supplied async file-like handle.
///
/// Wraps the handle in a tokio `Mutex` and serializes seek+read per fetch.
/// For parallel-safe reads across tasks, layer a [`BufferReader`](crate::BufferReader)
/// on top — block-aligned cached reads avoid most of the mutex contention.
pub struct FileSource<H> {
    handle: Arc<Mutex<H>>,
}

impl<H> FileSource<H>
where
    H: tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Unpin,
{
    pub fn new(handle: H) -> Self {
        Self {
            handle: Arc::new(Mutex::new(handle)),
        }
    }
}

impl<H> FileSource<H>
where
    H: tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Sync + Unpin,
{
    /// Borrow the underlying handle inside the mutex for advanced use.
    pub async fn with_handle<R>(&self, f: impl AsyncFnOnce(&mut H) -> R) -> R {
        let mut guard = self.handle.lock().await;
        f(&mut *guard).await
    }
}

impl<H> Source for FileSource<H>
where
    H: tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Sync + Unpin,
{
    async fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let mut buf = vec![0u8; len];
        let mut guard = self.handle.lock().await;
        guard.seek(SeekFrom::Start(offset)).await?;
        guard.read_exact(&mut buf).await?;
        drop(guard);
        Ok(Bytes::from(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test(flavor = "current_thread")]
    async fn reads_a_range_from_in_memory_cursor() {
        let data: Vec<u8> = (0..100u8).collect();
        let f = FileSource::new(Cursor::new(data));
        let out = f.read_exact_at(10, 10).await.unwrap();
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], 10);
        assert_eq!(out[9], 19);
    }
}
