// Reclaim empty read buffers when a socket would block, preserving transfer batching.
// The write side remains Tokio's BufWriter, including its partial-write handling.
use std::cell::RefCell;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, BufWriter, ReadBuf};

const POOLED_SIZE: usize = 64 * 1024;
const POOL_LIMIT: usize = 8;
thread_local! {
    // At most 512 KiB per polling thread, independent of connection count.
    static READ_BUFFERS: RefCell<Vec<Box<[u8]>>> = const { RefCell::new(Vec::new()) };
}

fn acquire(size: usize) -> Box<[u8]> {
    if size == POOLED_SIZE {
        if let Some(buffer) = READ_BUFFERS.with(|pool| pool.borrow_mut().pop()) {
            return buffer;
        }
    }
    vec![0; size].into_boxed_slice()
}

fn recycle(buffer: Box<[u8]>) {
    if buffer.len() == POOLED_SIZE {
        // Destruction can happen during thread-local teardown. In that case drop
        // the buffer instead of trying to resurrect the pool.
        let _ = READ_BUFFERS.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < POOL_LIMIT {
                pool.push(buffer);
            }
        });
    }
}

#[derive(Debug)]
pub struct IdleBufStream<S> {
    writer: BufWriter<S>,
    buffer: Option<Box<[u8]>>,
    capacity: usize,
    pos: usize,
    filled: usize,
}

impl<S: AsyncRead + AsyncWrite + Unpin> IdleBufStream<S> {
    pub fn with_capacity(read: usize, write: usize, stream: S) -> Self {
        Self {
            writer: BufWriter::with_capacity(write, stream),
            buffer: None,
            capacity: read,
            pos: 0,
            filled: 0,
        }
    }
    pub fn into_inner(self) -> S {
        self.writer.into_inner()
    }
    pub fn get_ref(&self) -> &S {
        self.writer.get_ref()
    }
    pub fn get_mut(&mut self) -> &mut S {
        self.writer.get_mut()
    }
    #[allow(dead_code)] // Exposed to the isolated allocation-lifetime tests.
    pub fn retained_read_capacity(&self) -> usize {
        self.buffer.as_ref().map_or(0, |buffer| buffer.len())
    }
    fn release_empty(&mut self) {
        debug_assert_eq!(self.pos, self.filled);
        if let Some(buffer) = self.buffer.take() {
            recycle(buffer);
        }
        self.pos = 0;
        self.filled = 0;
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for IdleBufStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.pos == this.filled {
            // Retain Tokio's direct-read path for callers asking for >=64 KiB.
            if out.remaining() >= this.capacity {
                let result = Pin::new(&mut this.writer).poll_read(cx, out);
                this.release_empty();
                return result;
            }
            let buffer = this.buffer.get_or_insert_with(|| acquire(this.capacity));
            let mut input = ReadBuf::new(buffer);
            match Pin::new(&mut this.writer).poll_read(cx, &mut input) {
                Poll::Pending => {
                    this.release_empty();
                    return Poll::Pending;
                }
                Poll::Ready(Err(err)) => {
                    this.release_empty();
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Ok(())) => {
                    this.filled = input.filled().len();
                    this.pos = 0;
                    if this.filled == 0 {
                        this.release_empty();
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }
        let n = out.remaining().min(this.filled - this.pos);
        out.put_slice(&this.buffer.as_ref().unwrap()[this.pos..this.pos + n]);
        this.pos += n;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for IdleBufStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pool_is_bounded_and_does_not_retain_other_sizes() {
        READ_BUFFERS.with(|pool| pool.borrow_mut().clear());
        for _ in 0..POOL_LIMIT + 20 {
            recycle(vec![0; POOLED_SIZE].into_boxed_slice());
        }
        recycle(vec![0; POOLED_SIZE * 2].into_boxed_slice());
        READ_BUFFERS.with(|pool| assert_eq!(pool.borrow().len(), POOL_LIMIT));
        let buffers: Vec<_> = (0..POOL_LIMIT).map(|_| acquire(POOLED_SIZE)).collect();
        READ_BUFFERS.with(|pool| assert!(pool.borrow().is_empty()));
        assert!(buffers.iter().all(|buffer| buffer.len() == POOLED_SIZE));
    }
}

#[cfg(test)]
#[path = "idle_buffer_tests.rs"]
mod read_tests;
