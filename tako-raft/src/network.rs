use crate::{
    LogStore, Replica, SnapshotFile, Types,
    disk::{decode, encode, error},
};
use openraft::async_runtime::WatchReceiver;
use openraft::{
    Config, OptionalSend, Raft,
    alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, VoteOf},
    errors::{NetworkError, RPCError, ReplicationClosed, StreamingError, Unreachable},
    network::{RPCOption, RaftNetworkFactory, v2::RaftNetworkV2},
    raft::{
        AppendEntriesRequest, AppendError, AppendResult, ConflictHint, SnapshotResponse,
        VoteRequest, VoteResponse,
    },
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::timeout,
};

type Node = Raft<Types, Replica>;
type Connections = Arc<parking_lot::Mutex<HashMap<SocketAddr, Vec<TcpStream>>>>;
const FRAME_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct Group {
    raft: Node,
    directory: PathBuf,
    secret: [u8; 32],
    transfer: Arc<parking_lot::Mutex<Option<(u64, u64)>>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Progress {
    pub applied: Option<u64>,
    pub snapshot_bytes: Option<(u64, u64)>,
}

/// Several resource groups share the server's private listener. The caller
/// supplies its WireGuard server directory to authenticate connections.
#[derive(Clone, Default)]
pub struct Peer {
    groups: Arc<RwLock<HashMap<String, Group>>>,
    connections: Connections,
}

impl Peer {
    pub async fn open(
        &self,
        generation: &str,
        secret: [u8; 32],
        id: u128,
        directory: &Path,
        config: Config,
    ) -> io::Result<(Node, Replica)> {
        if generation.is_empty() || generation.len() > 128 {
            return Err(error("invalid resource generation"));
        }
        let path = directory.to_owned();
        let (log, replica) = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&path)?;
            Ok::<_, io::Error>((
                LogStore::open(&path.join("raft.sqlite"))?,
                Replica::open(&path)?,
            ))
        })
        .await
        .map_err(error)??;
        let config = Arc::new(config.validate().map_err(error)?);
        let raft = Node::new(
            id,
            config,
            Network(generation.to_owned(), secret, self.connections.clone()),
            log,
            replica.clone(),
        )
        .await
        .map_err(error)?;
        self.groups.write().insert(
            generation.to_owned(),
            Group {
                raft: raft.clone(),
                directory: directory.to_owned(),
                secret,
                transfer: Default::default(),
            },
        );
        Ok((raft, replica))
    }
    /// Fix one committed position on the leader. An actively written database
    /// can then finish preparing as soon as the target has applied that position.
    pub async fn read_barrier(&self, generation: &str) -> io::Result<u64> {
        let group = self
            .groups
            .read()
            .get(generation)
            .cloned()
            .ok_or_else(|| error("unknown resource group"))?;
        let barrier = timeout(
            Duration::from_secs(10),
            group
                .raft
                .ensure_linearizable(openraft::ReadPolicy::ReadIndex),
        )
        .await
        .map_err(error)?
        .map_err(error)?
        .index();
        Ok(barrier)
    }

    pub async fn progress(&self, generation: &str, target: SocketAddr) -> io::Result<Progress> {
        let group = self
            .groups
            .read()
            .get(generation)
            .cloned()
            .ok_or_else(|| error("unknown resource group"))?;
        let client = Client {
            group: generation.to_owned(),
            target,
            secret: group.secret,
            connections: self.connections.clone(),
        };
        timeout(Duration::from_secs(10), async {
            let mut stream = client.send(&Request::Progress).await?;
            read_frame::<Progress>(&mut stream, 1024).await
        })
        .await
        .map_err(error)?
    }

    pub async fn wait_ready(
        &self,
        generation: &str,
        target: SocketAddr,
        mut report: impl FnMut(Progress),
    ) -> io::Result<()> {
        let barrier = self.read_barrier(generation).await?;
        loop {
            let progress = self.progress(generation, target).await?;
            let ready = progress.applied.is_some_and(|index| index >= barrier);
            report(progress);
            if ready {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    pub async fn close(&self, generation: &str) -> io::Result<()> {
        let group = self.groups.write().remove(generation);
        if let Some(group) = group {
            group.raft.shutdown().await.map_err(error)?;
        }
        Ok(())
    }

    pub async fn serve(
        &self,
        listener: TcpListener,
        allow: impl Fn(IpAddr) -> bool + Send + Sync + 'static,
    ) -> io::Result<()> {
        let allow = Arc::new(allow);
        let slots = Arc::new(Semaphore::new(128));
        loop {
            let (mut stream, peer) = listener.accept().await?;
            if !allow(peer.ip()) {
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let server = self.clone();
            let allow = allow.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if stream.set_nodelay(true).is_err() {
                    return;
                }
                while allow(peer.ip()) && server.handle(&mut stream).await.is_ok() {}
            });
        }
    }

    async fn handle(&self, mut stream: &mut TcpStream) -> io::Result<()> {
        let generation: String = timeout(Duration::from_secs(10), read_frame(&mut stream, 256))
            .await
            .map_err(error)??;
        let group = self
            .groups
            .read()
            .get(&generation)
            .cloned()
            .ok_or_else(|| error("unknown resource generation"))?;
        let mut presented = [0_u8; 32];
        timeout(Duration::from_secs(5), stream.read_exact(&mut presented))
            .await
            .map_err(error)??;
        if !bool::from(subtle::ConstantTimeEq::ct_eq(
            presented.as_slice(),
            group.secret.as_slice(),
        )) {
            return Err(error("invalid resource credential"));
        }
        let req = timeout(
            Duration::from_secs(30),
            read_frame(&mut stream, FRAME_LIMIT),
        )
        .await
        .map_err(error)??;
        match req {
            Request::Progress => {
                let applied = group
                    .raft
                    .metrics()
                    .borrow_watched()
                    .last_applied
                    .map(|id| id.index());
                let snapshot_bytes = *group.transfer.lock();
                write_frame(
                    &mut stream,
                    &Progress {
                        applied,
                        snapshot_bytes,
                    },
                )
                .await
            }
            Request::Append(req) => {
                let reply = match group.raft.append_entries(req).await.map_err(error)? {
                    Ok(id) => AppendReply::Accepted(id),
                    Err(AppendError::HigherVote(vote)) => AppendReply::HigherVote(vote),
                    Err(AppendError::Conflict { log_id, hint }) => {
                        AppendReply::Conflict(log_id, hint.map(|hint| hint.log_ids))
                    }
                };
                write_frame(&mut stream, &reply).await
            }
            Request::Vote(req) => {
                write_frame(&mut stream, &group.raft.vote(req).await.map_err(error)?).await
            }
            Request::PreVote(req) => {
                write_frame(&mut stream, &group.raft.pre_vote(req).await.map_err(error)?).await
            }
            Request::Snapshot { vote, meta, length } => {
                let file = Arc::new(NamedTempFile::new_in(&group.directory)?);
                let mut dest = tokio::fs::File::from_std(file.reopen()?);
                let mut buffer = vec![0; 128 * 1024];
                // The counter is per received snapshot, including a retry that
                // follows a partial transfer. Keep the final size until apply.
                *group.transfer.lock() = Some((0, length));
                let mut remaining = length;
                while remaining != 0 {
                    let len = remaining.min(buffer.len() as u64) as usize;
                    timeout(
                        Duration::from_secs(30),
                        stream.read_exact(&mut buffer[..len]),
                    )
                    .await
                    .map_err(error)??;
                    dest.write_all(&buffer[..len]).await?;
                    remaining -= len as u64;
                    *group.transfer.lock() = Some((length - remaining, length));
                }
                dest.flush().await?;
                let snapshot = openraft::Snapshot {
                    meta,
                    snapshot: file,
                };
                write_frame(
                    &mut stream,
                    &group
                        .raft
                        .install_full_snapshot(vote, snapshot)
                        .await
                        .map_err(error)?,
                )
                .await
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Request {
    Progress,
    Append(AppendEntriesRequest<Types>),
    Vote(VoteRequest<Types>),
    PreVote(VoteRequest<Types>),
    Snapshot {
        vote: VoteOf<Types>,
        meta: SnapshotMetaOf<Types>,
        length: u64,
    },
}

#[derive(Serialize, Deserialize)]
enum AppendReply {
    Accepted(Option<LogIdOf<Types>>),
    HigherVote(VoteOf<Types>),
    Conflict(LogIdOf<Types>, Option<Vec<LogIdOf<Types>>>),
}

async fn read_frame<T: DeserializeOwned>(stream: &mut TcpStream, limit: usize) -> io::Result<T> {
    let len = stream.read_u32().await? as usize;
    if len > limit {
        return Err(error("Raft frame too large"));
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    decode(&bytes)
}
async fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> io::Result<()> {
    let bytes = encode(value)?;
    if bytes.len() > FRAME_LIMIT {
        return Err(error("Raft frame too large"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}

struct Network(String, [u8; 32], Connections);
struct Client {
    group: String,
    target: SocketAddr,
    secret: [u8; 32],
    connections: Connections,
}
impl RaftNetworkFactory<Types> for Network {
    type Network = Client;
    async fn new_client(&mut self, _: u128, target: &SocketAddr) -> Client {
        Client {
            group: self.0.clone(),
            target: *target,
            secret: self.1,
            connections: self.2.clone(),
        }
    }
}

impl Client {
    async fn send(&self, req: &Request) -> io::Result<TcpStream> {
        let cached = self
            .connections
            .lock()
            .get_mut(&self.target)
            .and_then(Vec::pop);
        let mut stream = match cached {
            Some(stream) => stream,
            None => TcpStream::connect(self.target).await?,
        };
        stream.set_nodelay(true)?;
        write_frame(&mut stream, &self.group).await?;
        stream.write_all(&self.secret).await?;
        write_frame(&mut stream, req).await?;
        Ok(stream)
    }
    async fn rpc<T: DeserializeOwned>(
        &self,
        req: Request,
        option: RPCOption,
    ) -> Result<T, RPCError<Types>> {
        timeout(option.hard_ttl(), async {
            let mut stream = self.send(&req).await?;
            let response = read_frame(&mut stream, FRAME_LIMIT).await?;
            // Only a fully consumed reply permits reuse. Cancellation drops
            // the socket so the next request cannot read a previous reply.
            let mut pool = self.connections.lock();
            let slots = pool.entry(self.target).or_default();
            if slots.len() < 2 {
                slots.push(stream);
            }
            Ok::<T, io::Error>(response)
        })
        .await
        .map_err(|e| NetworkError::new(&e))?
        .map_err(|e| Unreachable::new(&e).into())
    }
}

impl RaftNetworkV2<Types> for Client {
    type SnapshotData = SnapshotFile;

    async fn vote(
        &mut self,
        req: VoteRequest<Types>,
        option: RPCOption,
    ) -> Result<VoteResponse<Types>, RPCError<Types>> {
        self.rpc(Request::Vote(req), option).await
    }
    async fn pre_vote(
        &mut self,
        req: VoteRequest<Types>,
        option: RPCOption,
    ) -> Result<VoteResponse<Types>, RPCError<Types>> {
        self.rpc(Request::PreVote(req), option).await
    }
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<Types>,
        option: RPCOption,
    ) -> Result<AppendResult<Types>, RPCError<Types>> {
        Ok(match self.rpc(Request::Append(req), option).await? {
            AppendReply::Accepted(id) => Ok(id),
            AppendReply::HigherVote(vote) => Err(AppendError::HigherVote(vote)),
            AppendReply::Conflict(log_id, hint) => Err(AppendError::Conflict {
                log_id,
                hint: hint.map(ConflictHint::new),
            }),
        })
    }
    async fn full_snapshot(
        &mut self,
        vote: VoteOf<Types>,
        snapshot: SnapshotOf<Types, SnapshotFile>,
        cancel: impl Future<Output = ReplicationClosed> + OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<Types>, StreamingError<Types>> {
        let send = async {
            let mut source = tokio::fs::File::from_std(snapshot.snapshot.reopen()?);
            let length = source.metadata().await?.len();
            let mut stream = self
                .send(&Request::Snapshot {
                    vote,
                    meta: snapshot.meta,
                    length,
                })
                .await?;
            if tokio::io::copy(&mut source, &mut stream).await? != length {
                return Err(error("short Raft snapshot"));
            }
            read_frame(&mut stream, FRAME_LIMIT).await
        };
        tokio::select! {
            closed = cancel => Err(StreamingError::Closed(closed)),
            result = timeout(option.hard_ttl(), send) => Ok(result.map_err(|e| NetworkError::new(&e))?.map_err(|e| NetworkError::new(&e))?),
        }
    }
}
