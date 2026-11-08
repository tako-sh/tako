use super::{IO_TIMEOUT, broker::Command, error, wire};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tako_channels::{ChannelAuthResponse, ChannelMessage, ChannelPublishPayload, PresenceMember};
use tako_network::AppEnv;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use zeroize::Zeroizing;

// Intentionally no Debug: Register can contain a database credential.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    SignInvocation {
        destination: tako_network::DeviceId,
        caller: AppEnv,
        target: AppEnv,
        invocation: tako_core::network::NetworkInvocation,
    },
    Register {
        app: AppEnv,
        postgres_url: Option<Zeroizing<String>>,
    },
    Publish {
        app: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        ephemeral: bool,
    },
    SyncChannel {
        app: AppEnv,
        channel: String,
        #[serde(serialize_with = "serialize_auth")]
        auth: ChannelAuthResponse,
    },
    Subscribe {
        app: AppEnv,
    },
    UpdatePresence {
        app: AppEnv,
        channel: String,
        member: PresenceMember,
    },
    Presence {
        app: AppEnv,
        channel: String,
    },
    RemovePresence {
        app: AppEnv,
        channel: String,
        connection_id: String,
    },
}
fn serialize_auth<S: serde::Serializer>(
    auth: &ChannelAuthResponse,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Auth<'a> {
        ok: bool,
        subject: &'a Option<String>,
        transport: &'a Option<tako_channels::ChannelTransport>,
        #[serde(flatten)]
        lifecycle: tako_channels::ChannelLifecycle,
    }
    Auth {
        ok: auth.ok,
        subject: &auth.subject,
        transport: &auth.transport,
        lifecycle: tako_channels::ChannelLifecycle {
            ephemeral: auth.ephemeral,
            replay_window_ms: auth.replay_window_ms,
            inactivity_ttl_ms: auth.inactivity_ttl_ms,
            keepalive_interval_ms: auth.keepalive_interval_ms,
            max_connection_lifetime_ms: auth.max_connection_lifetime_ms,
        },
    }
    .serialize(serializer)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Notification {
    Committed {
        message: ChannelMessage,
        ephemeral: bool,
    },
    Wake {
        channel: String,
    },
    Gap,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Response {
    SignedInvocation {
        call: crate::native_invocation::SignedInvocation,
    },
    Ok,
    Published {
        message: ChannelMessage,
    },
    Presence {
        members: Vec<PresenceMember>,
    },
    Rejected,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Notice {
    Notification { notification: Notification },
    Keepalive,
}
pub(super) enum Reply {
    Response(Response),
    Subscription(mpsc::Receiver<wire::Packet>),
}

#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
}
impl Client {
    pub(crate) async fn sign_invocation(
        &self,
        destination: tako_network::DeviceId,
        caller: AppEnv,
        target: AppEnv,
        invocation: tako_core::network::NetworkInvocation,
    ) -> io::Result<crate::native_invocation::SignedInvocation> {
        match self
            .request(Request::SignInvocation {
                destination,
                caller,
                target,
                invocation,
            })
            .await?
            .1
        {
            Response::SignedInvocation { call } => Ok(call),
            _ => Err(error("unexpected invocation signer response")),
        }
    }
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
    async fn request(&self, request: Request) -> io::Result<(UnixStream, Response)> {
        tokio::time::timeout(IO_TIMEOUT, async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            if !authorized(&stream)? {
                return Err(error("untrusted channel broker"));
            }
            wire::write(&mut stream, &request).await?;
            let response = wire::read(&mut stream, &wire::Budget::new(wire::MAX_FRAME))
                .await?
                .value;
            if matches!(response, Response::Rejected) {
                return Err(error("channel broker rejected request"));
            }
            Ok((stream, response))
        })
        .await?
    }
    pub async fn register(&self, app: AppEnv, postgres_url: Option<String>) -> io::Result<()> {
        self.request(Request::Register {
            app,
            postgres_url: postgres_url.map(Zeroizing::new),
        })
        .await?;
        Ok(())
    }
    pub async fn sync_channel(
        &self,
        app: AppEnv,
        channel: String,
        auth: ChannelAuthResponse,
    ) -> io::Result<()> {
        self.request(Request::SyncChannel { app, channel, auth })
            .await?;
        Ok(())
    }
    pub async fn publish(
        &self,
        app: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        ephemeral: bool,
    ) -> io::Result<ChannelMessage> {
        match self
            .request(Request::Publish {
                app,
                channel,
                payload,
                ephemeral,
            })
            .await?
            .1
        {
            Response::Published { message } => Ok(message),
            _ => Err(error("unexpected channel broker response")),
        }
    }
    pub async fn update_presence(
        &self,
        app: AppEnv,
        channel: String,
        member: PresenceMember,
    ) -> io::Result<()> {
        match self
            .request(Request::UpdatePresence {
                app,
                channel,
                member,
            })
            .await?
            .1
        {
            Response::Ok => Ok(()),
            _ => Err(error("unexpected presence response")),
        }
    }
    pub async fn presence(&self, app: AppEnv, channel: String) -> io::Result<Vec<PresenceMember>> {
        match self.request(Request::Presence { app, channel }).await?.1 {
            Response::Presence { members } => Ok(members),
            _ => Err(error("unexpected presence response")),
        }
    }
    pub async fn remove_presence(
        &self,
        app: AppEnv,
        channel: String,
        connection_id: String,
    ) -> io::Result<()> {
        match self
            .request(Request::RemovePresence {
                app,
                channel,
                connection_id,
            })
            .await?
            .1
        {
            Response::Ok => Ok(()),
            _ => Err(error("unexpected presence response")),
        }
    }
    pub async fn subscribe(&self, app: AppEnv) -> io::Result<Subscription> {
        let (stream, _) = self.request(Request::Subscribe { app }).await?;
        Ok(Subscription::from_stream(stream))
    }
}
pub struct Subscription {
    stream: Box<dyn AsyncRead + Unpin + Send>,
    budget: wire::Budget,
}
impl Subscription {
    pub(crate) fn from_stream(stream: impl AsyncRead + Unpin + Send + 'static) -> Self {
        Self {
            stream: Box::new(stream),
            budget: wire::Budget::new(wire::MAX_FRAME),
        }
    }
    /// A failed recv is a gap. Drop this subscription and register/subscribe again.
    /// Do not cancel recv and reuse the stream: cancellation may consume a partial frame.
    pub async fn recv(&mut self) -> io::Result<Notification> {
        loop {
            match wire::read(&mut self.stream, &self.budget).await?.value {
                Notice::Notification { notification } => return Ok(notification),
                Notice::Keepalive => (),
            }
        }
    }
}

pub(super) fn authorized(stream: &UnixStream) -> io::Result<bool> {
    let uid = stream.peer_cred()?.uid();
    // SAFETY: geteuid has no preconditions.
    Ok(uid == 0 || uid == unsafe { libc::geteuid() })
}
#[cfg(test)]
pub(super) async fn serve<S>(
    stream: S,
    commands: mpsc::Sender<Command>,
    budget: wire::Budget,
    replies: wire::Budget,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_with_signer(stream, commands, budget, replies, None).await
}

pub(super) async fn serve_with_signer<S>(
    mut stream: S,
    commands: mpsc::Sender<Command>,
    budget: wire::Budget,
    replies: wire::Budget,
    signer: Option<(
        std::sync::Arc<tako_network::LocalDeviceKeys>,
        tako_network::NetworkId,
    )>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    let request = wire::read(&mut stream, &budget).await?;
    if let Request::SignInvocation {
        destination,
        caller,
        target,
        invocation,
    } = &request.value
    {
        let response = match signer {
            Some((keys, network)) => match crate::native_invocation::SignedInvocation::sign(
                &keys,
                network,
                destination.clone(),
                caller.clone(),
                target.clone(),
                invocation.clone(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ) {
                Ok(call) => Response::SignedInvocation { call },
                Err(_) => Response::Rejected,
            },
            None => Response::Rejected,
        };
        let _admission = tokio::time::timeout_at(deadline, replies.wait(wire::MAX_FRAME)).await??;
        return tokio::time::timeout_at(deadline, wire::write(&mut stream, &response)).await?;
    }
    let (mut reader, mut writer) = tokio::io::split(stream);
    let response = async {
        // Reserve response space before submitting a mutation. Waiting here leaves the
        // broker free to process peer loss and prevents large snapshots exhausting memory.
        let admission = tokio::time::timeout_at(deadline, replies.wait(wire::MAX_FRAME)).await??;
        let (reply, receive) = oneshot::channel();
        tokio::time::timeout_at(
            deadline,
            commands.send(Command::Local {
                request,
                reply,
                deadline,
                admission,
            }),
        )
        .await?
        .map_err(|_| error("channel broker stopped"))?;
        let (reply, admission) = tokio::time::timeout_at(deadline, receive)
            .await?
            .map_err(|_| error("channel broker stopped"))?;
        match reply {
            Reply::Response(response) => {
                let result =
                    tokio::time::timeout_at(deadline, wire::write(&mut writer, &response)).await?;
                drop(admission);
                result
            }
            Reply::Subscription(mut receive) => {
                tokio::time::timeout_at(deadline, wire::write(&mut writer, &Response::Ok))
                    .await??;
                drop(admission);
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    tokio::select! {
                        packet = receive.recv() => match packet {
                            Some(packet) => wire::write_bytes(&mut writer, &packet.bytes).await?,
                            None => return Ok(()),
                        },
                        _ = tick.tick() => wire::write(&mut writer, &Notice::Keepalive).await?,
                    }
                }
            }
        }
    };
    // One request per connection. EOF cancels queued work; extra input is a protocol violation.
    // A mutation already started may still commit, so callers never blindly retry a lost ack.
    tokio::select! {
        result = response => result,
        _ = reader.read_u8() => Err(error("channel IPC connection closed or pipelined")),
    }
}

pub(super) struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    _lock: File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.device
            && meta.ino() == self.inode
            && meta.file_type().is_socket()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub(super) async fn bind(directory: &Path) -> io::Result<(UnixListener, SocketGuard)> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(directory.join("channels.lock"))?;
    let meta = lock.metadata()?;
    let uid = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o7777 != 0o600 {
        return Err(error("unsafe channel lock"));
    }
    // SAFETY: lock owns a valid file descriptor throughout the guard lifetime.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(error("channel broker already running"));
    }
    let path = directory.join("channels.sock");
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() || meta.uid() != uid || meta.mode() & 0o7777 != 0o600 {
                return Err(error("unsafe stale channel socket"));
            }
            match tokio::time::timeout(IO_TIMEOUT, UnixStream::connect(&path)).await {
                Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => (),
                _ => return Err(error("channel socket still active")),
            }
            let current = fs::symlink_metadata(&path)?;
            if current.dev() != meta.dev() || current.ino() != meta.ino() {
                return Err(error("channel socket replaced"));
            }
            fs::remove_file(&path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let meta = fs::symlink_metadata(&path)?;
    Ok((
        listener,
        SocketGuard {
            path,
            device: meta.dev(),
            inode: meta.ino(),
            _lock: lock,
        },
    ))
}
