use super::IdleBufStream;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

const SIZE: usize = 64 * 1024;

#[derive(Default, Debug)]
struct State {
    data: Vec<u8>,
    reads: Vec<usize>,
    writes: Vec<u8>,
    waker: Option<Waker>,
    eof: bool,
    error: bool,
    writes_blocked: bool,
}
#[derive(Clone, Default, Debug)]
struct Socket(Arc<Mutex<State>>);
impl Socket {
    fn send(&self, data: &[u8]) {
        let mut s = self.0.lock().unwrap();
        s.data.extend_from_slice(data);
        if let Some(waker) = s.waker.take() {
            waker.wake();
        }
    }
}
impl AsyncRead for Socket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut s = self.0.lock().unwrap();
        s.reads.push(out.remaining());
        if s.error {
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        }
        if s.data.is_empty() && !s.eof {
            s.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = out.remaining().min(s.data.len());
        out.put_slice(&s.data[..n]);
        s.data.drain(..n);
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for Socket {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut s = self.0.lock().unwrap();
        if s.writes_blocked {
            return Poll::Pending;
        }
        // Exercise partial underlying writes.
        let n = data.len().min(7);
        s.writes.extend_from_slice(&data[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
struct Noop;
impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}
fn poll(stream: &mut IdleBufStream<Socket>, size: usize) -> Poll<io::Result<Vec<u8>>> {
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut bytes = vec![0; size];
    let mut buf = ReadBuf::new(&mut bytes);
    match Pin::new(stream).poll_read(&mut cx, &mut buf) {
        Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.filled().to_vec())),
        Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        Poll::Pending => Poll::Pending,
    }
}
fn read(stream: &mut IdleBufStream<Socket>, size: usize) -> Vec<u8> {
    match poll(stream, size) {
        Poll::Ready(Ok(bytes)) => bytes,
        other => panic!("{other:?}"),
    }
}

#[test]
fn empty_pending_buffer_is_released_and_reactivated_at_64k() {
    let socket = Socket::default();
    socket.send(b"first");
    let mut stream = IdleBufStream::with_capacity(SIZE, 1460, socket.clone());
    assert_eq!(read(&mut stream, 16), b"first");
    assert!(poll(&mut stream, 16).is_pending());
    assert_eq!(stream.retained_read_capacity(), 0);
    socket.send(b"second");
    assert_eq!(read(&mut stream, 16), b"second");
    assert!(socket.0.lock().unwrap().reads.iter().all(|n| *n == SIZE));
}

#[test]
fn read_ahead_survives_partial_reads_and_idle_cycles() {
    let socket = Socket::default();
    let mut stream = IdleBufStream::with_capacity(SIZE, 1460, socket.clone());
    for _ in 0..3 {
        socket.send(b"GET /one\r\nGET /two\r\n");
        assert_eq!(read(&mut stream, 10), b"GET /one\r\n");
        let calls = socket.0.lock().unwrap().reads.len();
        assert_eq!(read(&mut stream, 10), b"GET /two\r\n");
        assert_eq!(socket.0.lock().unwrap().reads.len(), calls);
        assert!(poll(&mut stream, 1).is_pending());
        assert_eq!(stream.retained_read_capacity(), 0);
    }
}

#[test]
fn large_reads_bypass_buffer_and_zero_reads_never_consume() {
    let socket = Socket::default();
    socket.send(&vec![42; SIZE * 2]);
    let mut stream = IdleBufStream::with_capacity(SIZE, 1460, socket.clone());
    assert!(read(&mut stream, 0).is_empty());
    assert!(socket.0.lock().unwrap().reads.is_empty());
    assert_eq!(read(&mut stream, SIZE * 2), vec![42; SIZE * 2]);
    assert_eq!(stream.retained_read_capacity(), 0);
    assert_eq!(socket.0.lock().unwrap().reads, vec![SIZE * 2]);
}

#[test]
fn eof_and_error_release_only_empty_buffers() {
    for error in [false, true] {
        let socket = Socket::default();
        socket.send(b"abc");
        let mut stream = IdleBufStream::with_capacity(SIZE, 1460, socket.clone());
        assert_eq!(read(&mut stream, 1), b"a");
        {
            let mut s = socket.0.lock().unwrap();
            s.eof = true;
            s.error = error;
        }
        assert_eq!(read(&mut stream, 5), b"bc");
        let result = poll(&mut stream, 5);
        if error {
            assert!(matches!(result, Poll::Ready(Err(_))));
        } else {
            assert!(matches!(result, Poll::Ready(Ok(b)) if b.is_empty()));
        }
        assert_eq!(stream.retained_read_capacity(), 0);
    }
}

#[tokio::test]
async fn releasing_read_buffer_preserves_pending_writes_and_flush() {
    let socket = Socket::default();
    socket.send(b"read");
    let mut stream = IdleBufStream::with_capacity(SIZE, 1460, socket.clone());
    stream
        .write_all(b"a response longer than seven bytes")
        .await
        .unwrap();
    socket.0.lock().unwrap().writes_blocked = true;
    assert_eq!(read(&mut stream, 4), b"read");
    assert!(poll(&mut stream, 4).is_pending());
    assert_eq!(stream.retained_read_capacity(), 0);
    socket.0.lock().unwrap().writes_blocked = false;
    stream.flush().await.unwrap();
    assert_eq!(
        socket.0.lock().unwrap().writes,
        b"a response longer than seven bytes"
    );
}

#[tokio::test]
async fn actual_waker_resumes_after_releasing_the_buffer() {
    let (left, mut right) = tokio::io::duplex(1024);
    let mut stream = IdleBufStream::with_capacity(SIZE, 1460, left);
    let producer = tokio::spawn(async move {
        tokio::task::yield_now().await;
        right.write_all(b"awake").await.unwrap();
    });
    let mut output = [0; 5];
    stream.read_exact(&mut output).await.unwrap();
    assert_eq!(&output, b"awake");
    producer.await.unwrap();
}

#[test]
fn sustained_small_reads_keep_64k_socket_batches() {
    let socket = Socket::default();
    socket.send(&vec![17; SIZE * 16]);
    let mut stream = IdleBufStream::with_capacity(SIZE, 1460, socket.clone());
    for _ in 0..256 {
        assert_eq!(read(&mut stream, 4096), vec![17; 4096]);
    }
    assert_eq!(socket.0.lock().unwrap().reads, vec![SIZE; 16]);
    assert!(poll(&mut stream, 4096).is_pending());
    assert_eq!(stream.retained_read_capacity(), 0);
}

#[test]
fn unbuffered_mode_remains_unbuffered() {
    let socket = Socket::default();
    socket.send(b"abcdef");
    let mut stream = IdleBufStream::with_capacity(0, 0, socket.clone());
    assert_eq!(read(&mut stream, 3), b"abc");
    assert_eq!(read(&mut stream, 3), b"def");
    assert_eq!(stream.retained_read_capacity(), 0);
    assert_eq!(socket.0.lock().unwrap().reads, vec![3, 3]);
}
