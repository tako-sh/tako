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
use tako_channels::{ChannelAuthResponse, ChannelMessage, ChannelPublishPayload};
use tako_network::AppEnv;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use zeroize::Zeroizing;

// Intentionally no Debug: Register can contain a database credential.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
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
    Ok,
    Published { message: ChannelMessage },
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
    pub async fn subscribe(&self, app: AppEnv) -> io::Result<Subscription> {
        let (stream, _) = self.request(Request::Subscribe { app }).await?;
        Ok(Subscription {
            stream,
            budget: wire::Budget::new(wire::MAX_FRAME),
        })
    }
}
pub struct Subscription {
    stream: UnixStream,
    budget: wire::Budget,
}
impl Subscription {
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
pub(super) async fn serve<S>(
    mut stream: S,
    commands: mpsc::Sender<Command>,
    budget: wire::Budget,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = wire::read(&mut stream, &budget).await?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(IO_TIMEOUT, commands.send(Command::Local { request, reply }))
        .await?
        .map_err(|_| error("channel broker stopped"))?;
    let reply = tokio::time::timeout(IO_TIMEOUT, receive)
        .await?
        .map_err(|_| error("channel broker stopped"))?;
    match reply {
        Reply::Response(response) => wire::write(&mut stream, &response).await,
        Reply::Subscription(mut receive) => {
            wire::write(&mut stream, &Response::Ok).await?;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    packet = receive.recv() => match packet {
                        Some(packet) => wire::write_bytes(&mut stream, &packet.bytes).await?,
                        None => return Ok(()),
                    },
                    _ = tick.tick() => wire::write(&mut stream, &Notice::Keepalive).await?,
                }
            }
        }
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
