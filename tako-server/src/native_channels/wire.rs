use super::{IO_TIMEOUT, error};
use serde::{Serialize, de::DeserializeOwned};
use std::{io, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
};
use zeroize::Zeroizing;

pub(super) const MAX_FRAME: usize = 1024 * 1024;
#[derive(Clone)]
pub(super) struct Budget(Arc<Semaphore>, Arc<Semaphore>);
impl Budget {
    pub fn new(bytes: usize) -> Self {
        Self(
            Arc::new(Semaphore::new(bytes)),
            Arc::new(Semaphore::new(bytes)),
        )
    }
    pub async fn wait(&self, bytes: usize) -> io::Result<OwnedSemaphorePermit> {
        self.0
            .clone()
            .acquire_many_owned(bytes as u32)
            .await
            .map_err(|_| error("channel byte budget closed"))
    }
    pub fn take(&self, bytes: usize) -> io::Result<OwnedSemaphorePermit> {
        self.0
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| error("channel byte budget exhausted"))
    }
    fn take_ephemeral(&self, bytes: usize) -> io::Result<OwnedSemaphorePermit> {
        self.1
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| error("ephemeral byte budget exhausted"))
    }
}
pub(super) struct Received<T> {
    pub value: T,
    _bytes: OwnedSemaphorePermit,
}
impl<T> Received<T> {
    pub fn into_ephemeral(mut self, budget: &Budget) -> io::Result<Self> {
        self._bytes = budget.take_ephemeral(self._bytes.num_permits())?;
        Ok(self)
    }
    pub fn into_parts(self) -> (T, OwnedSemaphorePermit) {
        (self.value, self._bytes)
    }
}
pub(super) struct Packet {
    pub bytes: Arc<Zeroizing<Vec<u8>>>,
    _global: OwnedSemaphorePermit,
    _local: OwnedSemaphorePermit,
}
#[derive(Clone)]
pub(super) struct Sender {
    tx: mpsc::Sender<Packet>,
    ephemeral: mpsc::Sender<Packet>,
    global: Budget,
    local: Budget,
    id: Arc<()>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}
pub(super) struct Receiver {
    reliable: mpsc::Receiver<Packet>,
    ephemeral: mpsc::Receiver<Packet>,
}
impl Receiver {
    pub async fn recv(&mut self) -> Option<Packet> {
        // Reliable/control traffic has reserved queue and byte capacity.
        tokio::select! {
            biased;
            packet = self.reliable.recv() => match packet {
                Some(packet) => Some(packet),
                None => self.ephemeral.recv().await,
            },
            packet = self.ephemeral.recv() => match packet {
                Some(packet) => Some(packet),
                None => self.reliable.recv().await,
            },
        }
    }
    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<Packet, mpsc::error::TryRecvError> {
        self.reliable
            .try_recv()
            .or_else(|_| self.ephemeral.try_recv())
    }
}
pub(super) fn queue(global: Budget) -> (Sender, Receiver) {
    let (tx, rx) = mpsc::channel(64);
    let (ephemeral, live_rx) = mpsc::channel(64);
    (
        Sender {
            tx,
            ephemeral,
            global,
            local: Budget::new(2 * MAX_FRAME),
            id: Arc::new(()),
            dropped: Arc::default(),
        },
        Receiver {
            reliable: rx,
            ephemeral: live_rx,
        },
    )
}
impl Sender {
    pub fn session(&self) -> Arc<()> {
        self.id.clone()
    }
    pub fn is_session(&self, session: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.id, session)
    }

    pub fn send_ephemeral_bytes(&self, bytes: Arc<Zeroizing<Vec<u8>>>) {
        let send = || -> io::Result<()> {
            let packet = Packet {
                _global: self.global.take_ephemeral(bytes.len())?,
                _local: self.local.take_ephemeral(bytes.len())?,
                bytes,
            };
            self.ephemeral
                .try_send(packet)
                .map_err(|_| error("ephemeral queue full"))
        };
        if send().is_err() {
            let dropped = self
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .saturating_add(1);
            if dropped.is_power_of_two() {
                tracing::warn!(
                    dropped,
                    "dropping ephemeral events for a slow channel receiver"
                );
            }
        }
    }
    pub fn send<T: Serialize>(&self, value: &T) -> io::Result<()> {
        self.send_bytes(encode(value)?)
    }
    pub fn send_bytes(&self, bytes: Arc<Zeroizing<Vec<u8>>>) -> io::Result<()> {
        let packet = Packet {
            _global: self.global.take(bytes.len())?,
            _local: self.local.take(bytes.len())?,
            bytes,
        };
        self.tx
            .try_send(packet)
            .map_err(|_| error("channel queue unavailable"))
    }
    pub fn closed(&self) -> bool {
        self.tx.is_closed()
    }
}
pub(super) fn encode<T: Serialize>(value: &T) -> io::Result<Arc<Zeroizing<Vec<u8>>>> {
    let bytes =
        Zeroizing::new(serde_json::to_vec(value).map_err(|_| error("invalid channel frame"))?);
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(error("channel frame exceeds limit"));
    }
    Ok(Arc::new(bytes))
}
pub(super) async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
    budget: &Budget,
) -> io::Result<Received<T>> {
    // A single deadline covers the length and payload; partial frames are never resumed.
    tokio::time::timeout(IO_TIMEOUT, async {
        let len = reader.read_u32().await? as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(error("channel frame exceeds limit"));
        }
        let permit = budget.take(len)?;
        let mut bytes = Zeroizing::new(vec![0; len]);
        reader.read_exact(&mut bytes).await?;
        let value = serde_json::from_slice(&bytes).map_err(|_| error("invalid channel frame"))?;
        Ok(Received {
            value,
            _bytes: permit,
        })
    })
    .await?
}
pub(super) async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    write_bytes(writer, &encode(value)?).await
}
pub(super) async fn write_bytes<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> io::Result<()> {
    tokio::time::timeout(IO_TIMEOUT, async {
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(bytes).await?;
        writer.flush().await
    })
    .await?
}
