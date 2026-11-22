//! Serving stores over the Redis protocol.
//!
//! Apps connect on a loopback port and authenticate with their binding
//! token (`AUTH <token>`, or `HELLO 3 AUTH <user> <token>`). A token whose
//! store lives on another server makes this server open a connection to
//! that server's peer port, identify the app environment with the grant's
//! key, and relay the rest of the connection unchanged.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::command::{self, Class, Cmd, Spec, wrong_arguments};
use crate::glob;
use crate::host::{Forward, HostConfig, KvHost, Resolved, StoreId, TokenHash, hash_token};
use crate::resp::{self, Frame, Parsed, Reply};
use crate::store::{Op, Store, StoreError};

/// The first command on a peer connection: `TAKO-FORWARD <scope> <store>
/// <key> <protocol>`.
pub const FORWARD_COMMAND: &str = "TAKO-FORWARD";

/// Version reported to clients, which some use to pick features.
const REDIS_VERSION: &str = "7.2.0";
/// Bytes each connection buffers without drawing on the shared budget.
const BASE_BUFFER: usize = 64 * 1024;
/// Commands parsed from one read before they run.
const MAX_FRAMES: usize = 1024;
/// Replies buffered before they are written out mid-batch.
const OUT_FLUSH: usize = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a relayed connection checks that its binding still forwards.
const FORWARD_CHECK: Duration = Duration::from_secs(5);
/// Commands a connection may send before it authenticates.
const BEFORE_AUTH: &[&str] = &["auth", "hello", "quit", "reset"];
/// Commands that run at once inside a transaction, rather than queue.
const IMMEDIATE_IN_MULTI: &[&str] = &["exec", "discard", "multi", "watch", "quit", "reset"];

const NOAUTH: &str = "NOAUTH Authentication required.";
const WRONGPASS: &str = "WRONGPASS invalid username-password pair or user is disabled.";
const NOT_SERVED: &str = "ERR this server is not serving the store right now; retry";
const REBOUND: &str = "ERR this connection's binding changed; reconnect";

/// Who a listener accepts.
#[derive(Clone)]
pub enum Access {
    /// Apps on this server, which authenticate with binding tokens.
    Apps,
    /// Other servers forwarding for their apps. `allow` decides which peer
    /// addresses are servers of this network.
    Peers {
        allow: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>,
    },
}

/// Serves `listener` until the task is dropped.
pub async fn serve(host: KvHost, listener: TcpListener, access: Access) {
    let slots = Arc::new(Semaphore::new(host.config().max_connections));
    loop {
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return;
        };
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "KV listener accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let from_peer = match &access {
            Access::Apps => false,
            Access::Peers { allow } => {
                if !allow(peer.ip()) {
                    continue;
                }
                true
            }
        };
        let _ = socket.set_nodelay(true);
        let host = host.clone();
        tokio::spawn(async move {
            let _slot = slot;
            Connection::new(host, socket, peer, from_peer).run().await;
        });
    }
}

enum Auth {
    None,
    /// An app on this server, by its binding token.
    Local {
        token: TokenHash,
        store: StoreId,
    },
    /// An app on another server, which forwarded the connection.
    Remote {
        scope: String,
        store: StoreId,
        key: String,
    },
}

struct Multi {
    queued: Vec<(Vec<Bytes>, &'static Spec)>,
    /// A command failed to queue: `EXEC` aborts.
    failed: bool,
}

/// What a connection does after a command.
enum Flow {
    Continue,
    Close,
    /// Relay the connection to another server, replying `reply` to the
    /// command that authenticated once that server accepts it.
    Forward {
        target: Forward,
        token: TokenHash,
        reply: Reply,
        rest: Vec<Vec<Bytes>>,
        /// The protocol before a `HELLO` that asked to forward, restored
        /// when forwarding fails.
        proto: u8,
    },
}

enum Local {
    Reply(Reply),
    Close(Reply),
    Forward {
        target: Forward,
        token: TokenHash,
        reply: Reply,
        proto: u8,
    },
}

/// Why a connection cannot reach its store.
enum Gone {
    /// For now; the command fails.
    Unavailable(String),
    /// The binding was revoked or now points elsewhere: the client must
    /// reconnect.
    Rebound,
}

struct Connection {
    host: KvHost,
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    peer: SocketAddr,
    from_peer: bool,
    id: u64,
    proto: u8,
    name: Option<Bytes>,
    auth: Auth,
    multi: Option<Multi>,
    watched: Vec<(Bytes, Option<u64>)>,
    buf: BytesMut,
    /// Budget held for `buf` beyond [`BASE_BUFFER`].
    permit: Option<OwnedSemaphorePermit>,
    out: Vec<u8>,
    closings: tokio::sync::watch::Receiver<u64>,
}

struct Connected(KvHost);

impl Drop for Connected {
    fn drop(&mut self) {
        self.0.inner.connected.fetch_sub(1, Ordering::Relaxed);
    }
}

fn is(arg: &[u8], name: &str) -> bool {
    arg.eq_ignore_ascii_case(name.as_bytes())
}

pub(crate) fn time(config: &HostConfig) -> Reply {
    let ms = (config.clock)();
    Reply::Array(vec![
        Reply::bulk((ms / 1000).to_string()),
        Reply::bulk(((ms % 1000) * 1000).to_string()),
    ])
}

impl Connection {
    fn new(host: KvHost, socket: TcpStream, peer: SocketAddr, from_peer: bool) -> Self {
        let (reader, writer) = socket.into_split();
        let closings = host.inner.closings.subscribe();
        let id = host.inner.next_client.fetch_add(1, Ordering::Relaxed);
        Self {
            host,
            reader,
            writer,
            peer,
            from_peer,
            id,
            proto: 2,
            name: None,
            auth: Auth::None,
            multi: None,
            watched: Vec::new(),
            buf: BytesMut::with_capacity(16 * 1024),
            permit: None,
            out: Vec::new(),
            closings,
        }
    }

    fn config(&self) -> &HostConfig {
        &self.host.inner.config
    }

    fn reply(&mut self, reply: Reply) {
        reply.encode(self.proto, &mut self.out);
    }

    async fn flush(&mut self) -> bool {
        if self.out.is_empty() {
            return true;
        }
        let stall = self.config().stall_timeout;
        let written = tokio::time::timeout(stall, self.writer.write_all(&self.out)).await;
        self.out.clear();
        if self.out.capacity() > OUT_FLUSH * 4 {
            self.out = Vec::new();
        }
        matches!(written, Ok(Ok(())))
    }

    /// Makes room for a request of `needed` bytes, drawing on the shared
    /// budget beyond the base buffer. False when none frees up in time.
    async fn reserve(&mut self, needed: usize) -> bool {
        let held = self.permit.as_ref().map_or(0, |p| p.num_permits());
        if needed > BASE_BUFFER + held {
            let more = (needed - BASE_BUFFER - held) as u32;
            let budget = self.host.inner.buffered.clone();
            let stall = self.config().stall_timeout;
            match tokio::time::timeout(stall, budget.acquire_many_owned(more)).await {
                Ok(Ok(permit)) => match &mut self.permit {
                    Some(held) => held.merge(permit),
                    None => self.permit = Some(permit),
                },
                _ => return false,
            }
        }
        if needed > self.buf.capacity() {
            self.buf.reserve(needed - self.buf.len());
        }
        true
    }

    /// Returns budget and memory once a large request is done.
    fn shrink(&mut self) {
        if self.buf.len() <= BASE_BUFFER && self.permit.is_some() {
            self.permit = None;
            if self.buf.capacity() > BASE_BUFFER {
                let rest = self.buf.split();
                self.buf = BytesMut::with_capacity(16 * 1024);
                self.buf.extend_from_slice(&rest);
            }
        }
    }

    fn take(&mut self, frame: Frame) -> Vec<Bytes> {
        match frame {
            Frame::Array { args, consumed } => {
                let data = self.buf.split_to(consumed).freeze();
                args.into_iter().map(|(s, e)| data.slice(s..e)).collect()
            }
            Frame::Inline { args, consumed } => {
                self.buf.advance(consumed);
                args
            }
        }
    }

    /// Reads more bytes. False when the connection ends or the host closes.
    async fn read_more(&mut self) -> bool {
        tokio::select! {
            read = self.reader.read_buf(&mut self.buf) => matches!(read, Ok(n) if n > 0),
            _ = self.closings.changed() => false,
        }
    }

    async fn run(mut self) {
        self.host.inner.connected.fetch_add(1, Ordering::Relaxed);
        let _connected = Connected(self.host.clone());
        if self.from_peer && !self.handshake().await {
            return;
        }
        let max_request = self.config().max_request_bytes;
        loop {
            let mut frames = Vec::new();
            let mut fatal = None;
            while frames.len() < MAX_FRAMES {
                match resp::parse(&self.buf, max_request) {
                    Ok(Parsed::Frame(frame)) => {
                        let args = self.take(frame);
                        frames.push(args);
                    }
                    Ok(Parsed::Incomplete { needed }) => {
                        if frames.is_empty() && !self.reserve(needed).await {
                            fatal = Some(
                                "ERR the server is short of request buffer space; retry".into(),
                            );
                        }
                        break;
                    }
                    Err(error) => {
                        fatal = Some(error.0);
                        break;
                    }
                }
            }
            if !frames.is_empty() {
                let mut flow = self.process(frames).await;
                while let Flow::Forward {
                    target,
                    token,
                    reply,
                    rest,
                    proto,
                } = flow
                {
                    if !self.flush().await {
                        return;
                    }
                    match self.open_upstream(&target).await {
                        Ok((upstream, early)) => {
                            // Bytes sent after accepting would answer nothing.
                            if !early.is_empty() {
                                return;
                            }
                            self.reply(reply);
                            if !self.flush().await {
                                return;
                            }
                            let mut pending = Vec::new();
                            for args in &rest {
                                resp::encode_request(args, &mut pending);
                            }
                            pending.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.relay(upstream, pending, target, token).await;
                            return;
                        }
                        Err(error) => {
                            // The command that authenticated fails; the
                            // connection carries on as it was.
                            self.proto = proto;
                            self.reply(Reply::Error(error));
                            flow = self.process(rest).await;
                        }
                    }
                }
                if matches!(flow, Flow::Close) || !self.flush().await {
                    let _ = self.flush().await;
                    return;
                }
                self.shrink();
                if fatal.is_none() {
                    continue;
                }
            }
            if let Some(error) = fatal {
                self.reply(Reply::Error(error));
                let _ = self.flush().await;
                return;
            }
            if !self.read_more().await {
                return;
            }
        }
    }

    /// Accepts a connection another server forwards: its first command
    /// names the app environment, the store, and the grant's key.
    async fn handshake(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
        let args = loop {
            match resp::parse(&self.buf, 4096) {
                Ok(Parsed::Frame(frame)) => break self.take(frame),
                Ok(Parsed::Incomplete { .. }) => {}
                Err(_) => return false,
            }
            match tokio::time::timeout_at(deadline, self.reader.read_buf(&mut self.buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return false,
            }
        };
        let text = |i: usize| args.get(i).and_then(|a| std::str::from_utf8(a).ok());
        let accepted = (|| {
            if args.len() != 5 || !is(&args[0], FORWARD_COMMAND) {
                return None;
            }
            let (scope, store, key) = (text(1)?, StoreId::parse(text(2)?).ok()?, text(3)?);
            let proto = match text(4)? {
                "2" => 2,
                "3" => 3,
                _ => return None,
            };
            self.host
                .granted(scope, &store, key)
                .then(|| (scope.to_owned(), store, key.to_owned(), proto))
        })();
        let Some((scope, store, key, proto)) = accepted else {
            self.reply(Reply::error("ERR forwarding refused"));
            let _ = self.flush().await;
            return false;
        };
        self.auth = Auth::Remote { scope, store, key };
        self.proto = proto;
        self.reply(Reply::Ok);
        self.flush().await
    }

    fn current_store(&self) -> Result<Arc<Store>, Gone> {
        let id = match &self.auth {
            Auth::Local { token, store } => match self.host.resolve_token(token) {
                Some(Resolved::Serve(id)) if id == *store => id,
                Some(Resolved::Unavailable(reason)) => {
                    return Err(Gone::Unavailable(format!("ERR {reason}")));
                }
                _ => return Err(Gone::Rebound),
            },
            Auth::Remote { scope, store, key } => {
                if !self.host.granted(scope, store, key) {
                    return Err(Gone::Rebound);
                }
                store.clone()
            }
            Auth::None => return Err(Gone::Unavailable(NOAUTH.into())),
        };
        self.host
            .store(&id)
            .filter(|store| !store.is_closed())
            .ok_or_else(|| Gone::Unavailable(NOT_SERVED.into()))
    }

    /// Runs queued store commands. False when the connection must close.
    async fn run_pending(&mut self, pending: &mut Vec<Cmd>) -> bool {
        while !pending.is_empty() {
            let store = match self.current_store() {
                Ok(store) => store,
                Err(Gone::Unavailable(message)) => {
                    for _ in pending.drain(..) {
                        self.reply(Reply::Error(message.clone()));
                    }
                    return true;
                }
                Err(Gone::Rebound) => {
                    self.reply(Reply::error(REBOUND));
                    pending.clear();
                    return false;
                }
            };
            match store.run(pending.clone()).await {
                Ok(replies) => {
                    let done = replies.len().max(1).min(pending.len());
                    for reply in replies {
                        self.reply(reply);
                    }
                    pending.drain(..done);
                    if !pending.is_empty() && !self.flush().await {
                        return false;
                    }
                }
                Err(error) => {
                    let message = store_error(&error);
                    for _ in pending.drain(..) {
                        self.reply(Reply::Error(message.clone()));
                    }
                }
            }
        }
        true
    }

    async fn process(&mut self, frames: Vec<Vec<Bytes>>) -> Flow {
        let limits = self.config().limits();
        let mut pending = Vec::new();
        let mut frames = frames.into_iter();
        while let Some(args) = frames.next() {
            if args.is_empty() {
                continue;
            }
            let spec = match command::lookup(&args) {
                Ok(spec) => spec,
                Err(reply) => {
                    if let Some(multi) = &mut self.multi {
                        multi.failed = true;
                    }
                    if !self.run_pending(&mut pending).await {
                        return Flow::Close;
                    }
                    self.reply(reply);
                    continue;
                }
            };
            if matches!(self.auth, Auth::None) && !BEFORE_AUTH.contains(&spec.name) {
                if !self.run_pending(&mut pending).await {
                    return Flow::Close;
                }
                self.reply(Reply::error(NOAUTH));
                continue;
            }
            if let Some(multi) = &mut self.multi
                && !IMMEDIATE_IN_MULTI.contains(&spec.name)
            {
                let reply = if matches!(spec.name, "auth" | "hello") {
                    multi.failed = true;
                    Reply::error("ERR Command not allowed inside a transaction")
                } else {
                    multi.queued.push((args, spec));
                    Reply::Simple("QUEUED".into())
                };
                self.reply(reply);
                continue;
            }
            match spec.class {
                Class::Read | Class::Write { .. } => match command::parse(spec, &args, limits) {
                    Ok(cmd) => pending.push(cmd),
                    Err(reply) => {
                        if !self.run_pending(&mut pending).await {
                            return Flow::Close;
                        }
                        self.reply(reply);
                    }
                },
                Class::Connection => {
                    if !self.run_pending(&mut pending).await {
                        return Flow::Close;
                    }
                    match self.local(spec, args).await {
                        Local::Reply(reply) => self.reply(reply),
                        Local::Close(reply) => {
                            self.reply(reply);
                            return Flow::Close;
                        }
                        Local::Forward {
                            target,
                            token,
                            reply,
                            proto,
                        } => {
                            return Flow::Forward {
                                target,
                                token,
                                reply,
                                rest: frames.collect(),
                                proto,
                            };
                        }
                    }
                }
            }
            if self.out.len() > OUT_FLUSH && !self.flush().await {
                return Flow::Close;
            }
        }
        if !self.run_pending(&mut pending).await {
            return Flow::Close;
        }
        Flow::Continue
    }

    /// Checks a token. Peer connections are authenticated by their
    /// handshake, which `AUTH` cannot change.
    fn authenticate(&mut self, password: &[u8], reply: Reply) -> Local {
        if self.from_peer {
            return Local::Reply(reply);
        }
        let Ok(token) = std::str::from_utf8(password) else {
            return Local::Reply(Reply::error(WRONGPASS));
        };
        let token = hash_token(token);
        match self.host.resolve_token(&token) {
            Some(Resolved::Serve(store)) => {
                self.auth = Auth::Local { token, store };
                Local::Reply(reply)
            }
            Some(Resolved::Forward(target)) => Local::Forward {
                target,
                token,
                reply,
                proto: self.proto,
            },
            Some(Resolved::Unavailable(reason)) => {
                Local::Reply(Reply::Error(format!("ERR {reason}")))
            }
            None => Local::Reply(Reply::error(WRONGPASS)),
        }
    }

    async fn local(&mut self, spec: &'static Spec, args: Vec<Bytes>) -> Local {
        Local::Reply(match spec.name {
            "quit" => return Local::Close(Reply::Ok),
            "auth" => {
                let password = match args.len() {
                    2 => &args[1],
                    3 => &args[2],
                    _ => return Local::Reply(Reply::error(command::SYNTAX)),
                };
                let password = password.clone();
                return self.authenticate(&password, Reply::Ok);
            }
            "hello" => return self.hello(&args),
            "reset" => {
                self.multi = None;
                self.watched.clear();
                self.proto = 2;
                self.name = None;
                if !self.from_peer {
                    self.auth = Auth::None;
                }
                Reply::Simple("RESET".into())
            }
            "multi" => {
                if self.multi.is_some() {
                    Reply::error("ERR MULTI calls can not be nested")
                } else {
                    self.multi = Some(Multi {
                        queued: Vec::new(),
                        failed: false,
                    });
                    Reply::Ok
                }
            }
            "exec" => self.exec().await,
            "discard" => {
                if self.multi.take().is_some() {
                    self.watched.clear();
                    Reply::Ok
                } else {
                    Reply::error("ERR DISCARD without MULTI")
                }
            }
            "watch" => {
                if self.multi.is_some() {
                    Reply::error("ERR WATCH inside MULTI is not allowed")
                } else {
                    self.watch(args[1..].to_vec()).await
                }
            }
            "info" => self.info(&args).await,
            _ => self.simple(spec, &args),
        })
    }

    fn hello(&mut self, args: &[Bytes]) -> Local {
        let mut proto = self.proto;
        let mut i = 1;
        if let Some(version) = args.get(1) {
            proto = match command::int(version) {
                Some(2) => 2,
                Some(3) => 3,
                Some(_) => {
                    return Local::Reply(Reply::error("NOPROTO unsupported protocol version"));
                }
                None => {
                    return Local::Reply(Reply::error(
                        "ERR Protocol version is not an integer or out of range",
                    ));
                }
            };
            i = 2;
        }
        let mut password = None;
        let mut name = None;
        while i < args.len() {
            if is(&args[i], "auth") && i + 2 < args.len() {
                password = Some(args[i + 2].clone());
                i += 3;
            } else if is(&args[i], "setname") && i + 1 < args.len() {
                name = Some(args[i + 1].clone());
                i += 2;
            } else {
                return Local::Reply(Reply::Error(format!(
                    "ERR Syntax error in HELLO option '{}'",
                    String::from_utf8_lossy(&args[i])
                )));
            }
        }
        if password.is_none() && matches!(self.auth, Auth::None) {
            return Local::Reply(Reply::error(
                "NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time",
            ));
        }
        let previous = self.proto;
        self.proto = proto;
        let mut result = match password {
            Some(password) => {
                let reply = self.hello_reply();
                self.authenticate(&password, reply)
            }
            None => Local::Reply(self.hello_reply()),
        };
        match &mut result {
            Local::Reply(Reply::Error(_)) => self.proto = previous,
            Local::Forward { proto, .. } => *proto = previous,
            _ => {}
        }
        if !matches!(result, Local::Reply(Reply::Error(_)))
            && let Some(name) = name
        {
            self.name = (!name.is_empty()).then_some(name);
        }
        result
    }

    fn hello_reply(&self) -> Reply {
        Reply::Map(vec![
            (Reply::bulk_str("server"), Reply::bulk_str("redis")),
            (Reply::bulk_str("version"), Reply::bulk_str(REDIS_VERSION)),
            (Reply::bulk_str("proto"), Reply::Int(i64::from(self.proto))),
            (Reply::bulk_str("id"), Reply::Int(self.id as i64)),
            (Reply::bulk_str("mode"), Reply::bulk_str("standalone")),
            (Reply::bulk_str("role"), Reply::bulk_str("master")),
            (Reply::bulk_str("modules"), Reply::Array(Vec::new())),
        ])
    }

    async fn watch(&mut self, keys: Vec<Bytes>) -> Reply {
        let store = match self.current_store() {
            Ok(store) => store,
            Err(Gone::Unavailable(message)) => return Reply::Error(message),
            Err(Gone::Rebound) => return Reply::error(REBOUND),
        };
        match store.versions(keys.clone()).await {
            Ok(versions) => {
                self.watched.extend(keys.into_iter().zip(versions));
                Reply::Ok
            }
            Err(error) => Reply::Error(store_error(&error)),
        }
    }

    async fn exec(&mut self) -> Reply {
        let Some(multi) = self.multi.take() else {
            return Reply::error("ERR EXEC without MULTI");
        };
        let watched = std::mem::take(&mut self.watched);
        if multi.failed {
            return Reply::error("EXECABORT Transaction discarded because of previous errors.");
        }
        let limits = self.config().limits();
        let ops: Vec<Op> = multi
            .queued
            .iter()
            .filter(|(_, spec)| spec.class != Class::Connection)
            .map(|(args, spec)| match command::parse(spec, args, limits) {
                Ok(cmd) => Op::Cmd(cmd),
                Err(reply) => Op::Failed(reply),
            })
            .collect();
        let replies = if ops.is_empty() && watched.is_empty() {
            Vec::new()
        } else {
            let store = match self.current_store() {
                Ok(store) => store,
                Err(Gone::Unavailable(message)) => return Reply::Error(message),
                Err(Gone::Rebound) => return Reply::error(REBOUND),
            };
            match store.exec(ops, watched).await {
                Ok(Reply::Array(replies)) => replies,
                Ok(other) => return other,
                Err(error) => return Reply::Error(store_error(&error)),
            }
        };
        let mut replies = replies.into_iter();
        let mut results = Vec::with_capacity(multi.queued.len());
        for (args, spec) in &multi.queued {
            results.push(if spec.class == Class::Connection {
                match spec.name {
                    "info" => self.info(args).await,
                    _ => self.simple(spec, args),
                }
            } else {
                replies.next().unwrap_or(Reply::Nil)
            });
        }
        Reply::Array(results)
    }

    async fn info(&self, args: &[Bytes]) -> Reply {
        let stats = match self.current_store() {
            Ok(store) => store.stats().await.ok(),
            Err(_) => None,
        }
        .unwrap_or_default();
        let config = self.config();
        let uptime = self.host.inner.started.elapsed().as_secs();
        let sections: [(&str, String); 7] = [
            (
                "server",
                format!(
                    "redis_version:{REDIS_VERSION}\r\ntako_kv:1\r\nredis_mode:standalone\r\narch_bits:64\r\nprocess_id:{}\r\nuptime_in_seconds:{uptime}\r\nuptime_in_days:{}\r\n",
                    std::process::id(),
                    uptime / 86_400
                ),
            ),
            (
                "clients",
                format!(
                    "connected_clients:{}\r\n",
                    self.host.inner.connected.load(Ordering::Relaxed)
                ),
            ),
            (
                "memory",
                format!(
                    "used_memory:{}\r\nmaxmemory:{}\r\nmaxmemory_policy:noeviction\r\n",
                    stats.bytes, config.max_store_bytes
                ),
            ),
            (
                "persistence",
                "loading:0\r\nasync_loading:0\r\nrdb_bgsave_in_progress:0\r\naof_enabled:0\r\n"
                    .into(),
            ),
            ("stats", String::new()),
            (
                "replication",
                "role:master\r\nconnected_slaves:0\r\n".into(),
            ),
            (
                "keyspace",
                if stats.keys > 0 {
                    format!(
                        "db0:keys={},expires={},avg_ttl=0\r\n",
                        stats.keys, stats.expires
                    )
                } else {
                    String::new()
                },
            ),
        ];
        let wanted: Vec<String> = args[1..]
            .iter()
            .map(|a| String::from_utf8_lossy(a).to_lowercase())
            .collect();
        let all = wanted.is_empty()
            || wanted
                .iter()
                .any(|w| w == "all" || w == "everything" || w == "default");
        let mut text = String::new();
        for (name, body) in sections {
            if all || wanted.iter().any(|w| w == name) {
                if !text.is_empty() {
                    text.push_str("\r\n");
                }
                let mut title = name.to_owned();
                title[..1].make_ascii_uppercase();
                text.push_str(&format!("# {title}\r\n{body}"));
            }
        }
        Reply::bulk(text)
    }

    /// Connection commands that need no store and do not change who the
    /// connection is.
    fn simple(&mut self, spec: &'static Spec, args: &[Bytes]) -> Reply {
        match spec.name {
            "ping" => match args.len() {
                1 => Reply::Simple("PONG".into()),
                2 => Reply::Bulk(args[1].clone()),
                _ => wrong_arguments("ping"),
            },
            "echo" => Reply::Bulk(args[1].clone()),
            "select" => match command::int(&args[1]) {
                Some(0) => Reply::Ok,
                Some(_) => Reply::error("ERR DB index is out of range"),
                None => Reply::error(command::NOT_INTEGER),
            },
            "time" => time(self.config()),
            "wait" => match (command::int(&args[1]), command::int(&args[2])) {
                (Some(_), Some(timeout)) if timeout >= 0 => Reply::Int(0),
                (Some(_), Some(_)) => Reply::error("ERR timeout is negative"),
                _ => Reply::error(command::NOT_INTEGER),
            },
            "unwatch" => {
                self.watched.clear();
                Reply::Ok
            }
            "client" => self.client(args),
            "command" => command_command(args),
            "config" => self.config_command(args),
            other => Reply::Error(format!("ERR {} cannot be used here", other.to_uppercase())),
        }
    }

    fn client_info(&self) -> String {
        format!(
            "id={} addr={} laddr= fd=0 name={} db=0 multi={} resp={} lib-name= lib-ver=",
            self.id,
            self.peer,
            self.name
                .as_ref()
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default(),
            self.multi.as_ref().map_or(-1, |m| m.queued.len() as i64),
            self.proto
        )
    }

    fn client(&mut self, args: &[Bytes]) -> Reply {
        let sub = String::from_utf8_lossy(&args[1]).to_lowercase();
        let arity = |n: usize| {
            if args.len() == n {
                None
            } else {
                Some(wrong_arguments(&format!("client|{sub}")))
            }
        };
        match sub.as_str() {
            "setname" => {
                if let Some(error) = arity(3) {
                    return error;
                }
                let name = &args[2];
                if name.iter().any(|b| !(b'!'..=b'~').contains(b)) {
                    return Reply::error(
                        "ERR Client names cannot contain spaces, newlines or special characters.",
                    );
                }
                self.name = (!name.is_empty()).then(|| name.clone());
                Reply::Ok
            }
            "getname" => {
                arity(2).unwrap_or_else(|| self.name.clone().map_or(Reply::Nil, Reply::Bulk))
            }
            "id" => arity(2).unwrap_or(Reply::Int(self.id as i64)),
            "setinfo" => {
                if let Some(error) = arity(4) {
                    return error;
                }
                if is(&args[2], "lib-name") || is(&args[2], "lib-ver") {
                    Reply::Ok
                } else {
                    Reply::Error(format!(
                        "ERR Unrecognized option '{}'",
                        String::from_utf8_lossy(&args[2])
                    ))
                }
            }
            "info" => arity(2).unwrap_or_else(|| Reply::bulk(format!("{}\n", self.client_info()))),
            "list" => Reply::bulk(format!("{}\n", self.client_info())),
            "no-evict" | "no-touch" => {
                if let Some(error) = arity(3) {
                    return error;
                }
                if is(&args[2], "on") || is(&args[2], "off") {
                    Reply::Ok
                } else {
                    Reply::error(command::SYNTAX)
                }
            }
            "help" => Reply::Array(
                [
                    "CLIENT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
                    "GETNAME",
                    "ID",
                    "INFO",
                    "LIST",
                    "SETINFO <option> <value>",
                    "SETNAME <name>",
                    "NO-EVICT (ON|OFF)",
                    "NO-TOUCH (ON|OFF)",
                    "HELP",
                ]
                .iter()
                .map(|line| Reply::Simple((*line).into()))
                .collect(),
            ),
            _ => Reply::Error(format!(
                "ERR unknown subcommand '{}'. Try CLIENT HELP.",
                String::from_utf8_lossy(&args[1])
            )),
        }
    }

    fn config_command(&self, args: &[Bytes]) -> Reply {
        let sub = String::from_utf8_lossy(&args[1]).to_lowercase();
        match sub.as_str() {
            "get" if args.len() >= 3 => {
                let max = self.config().max_store_bytes.to_string();
                let known = [
                    ("maxmemory", max.as_str()),
                    ("maxmemory-policy", "noeviction"),
                    ("databases", "1"),
                    ("timeout", "0"),
                    ("save", ""),
                    ("appendonly", "no"),
                ];
                let mut pairs = Vec::new();
                for (name, value) in known {
                    if args[2..]
                        .iter()
                        .any(|p| glob::matches(&p.to_ascii_lowercase(), name.as_bytes()))
                    {
                        pairs.push((Reply::bulk_str(name), Reply::bulk_str(value)));
                    }
                }
                Reply::Map(pairs)
            }
            "get" => wrong_arguments("config|get"),
            "resetstat" => Reply::Ok,
            "set" | "rewrite" => Reply::Error(format!(
                "ERR CONFIG {} is not supported; Tako manages this store's configuration",
                sub.to_uppercase()
            )),
            _ => Reply::Error(format!(
                "ERR unknown subcommand '{}'. Try CONFIG HELP.",
                String::from_utf8_lossy(&args[1])
            )),
        }
    }

    async fn open_upstream(&self, target: &Forward) -> Result<(TcpStream, Vec<u8>), String> {
        let unreachable = "ERR the server hosting this store is not reachable right now; retry";
        let connect = tokio::time::timeout(
            self.config().connect_timeout,
            TcpStream::connect(target.upstream),
        )
        .await;
        let mut upstream = match connect {
            Ok(Ok(stream)) => stream,
            _ => return Err(unreachable.into()),
        };
        let _ = upstream.set_nodelay(true);
        let mut handshake = Vec::new();
        resp::encode_request(
            &[
                FORWARD_COMMAND,
                target.scope.as_str(),
                target.store.as_str(),
                target.key.as_str(),
                if self.proto >= 3 { "3" } else { "2" },
            ],
            &mut handshake,
        );
        if upstream.write_all(&handshake).await.is_err() {
            return Err(unreachable.into());
        }
        let mut answer = Vec::new();
        let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            if let Some((line, used)) = resp::first_line(&answer) {
                if line == b"+OK" {
                    return Ok((upstream, answer[used..].to_vec()));
                }
                return Err(match line.strip_prefix(b"-") {
                    Some(error) => format!(
                        "ERR the store's server refused this app: {}",
                        String::from_utf8_lossy(error).trim_start_matches("ERR ")
                    ),
                    None => unreachable.into(),
                });
            }
            if answer.len() > 4096 {
                return Err(unreachable.into());
            }
            let mut chunk = [0u8; 1024];
            match tokio::time::timeout_at(deadline, upstream.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => answer.extend_from_slice(&chunk[..n]),
                _ => return Err(unreachable.into()),
            }
        }
    }

    /// Copies bytes both ways until either side closes, this host closes,
    /// or the binding stops pointing at `target`.
    async fn relay(
        &mut self,
        mut upstream: TcpStream,
        pending: Vec<u8>,
        target: Forward,
        token: TokenHash,
    ) {
        if !pending.is_empty() && upstream.write_all(&pending).await.is_err() {
            return;
        }
        self.permit = None;
        let host = self.host.clone();
        let (mut from_upstream, mut to_upstream) = upstream.split();
        let closings = &mut self.closings;
        let still_bound = async {
            let mut tick = tokio::time::interval(FORWARD_CHECK);
            tick.tick().await;
            loop {
                tick.tick().await;
                if host.resolve_token(&token) != Some(Resolved::Forward(target.clone())) {
                    break;
                }
            }
        };
        tokio::select! {
            _ = tokio::io::copy(&mut self.reader, &mut to_upstream) => {}
            _ = tokio::io::copy(&mut from_upstream, &mut self.writer) => {}
            _ = closings.changed() => {}
            _ = still_bound => {}
        }
    }
}

fn store_error(error: &StoreError) -> String {
    match error {
        StoreError::Closed => NOT_SERVED.into(),
        StoreError::Storage(message) => format!("ERR storage error: {message}"),
    }
}

fn command_command(args: &[Bytes]) -> Reply {
    let Some(sub) = args.get(1) else {
        return Reply::Array(Vec::new());
    };
    let sub = String::from_utf8_lossy(sub).to_lowercase();
    match sub.as_str() {
        "count" => Reply::Int(command::count() as i64),
        "docs" => Reply::Map(Vec::new()),
        "info" => Reply::Array(args[2..].iter().map(|_| Reply::Nil).collect()),
        _ => Reply::Error(format!(
            "ERR unknown subcommand '{}'. Try COMMAND HELP.",
            String::from_utf8_lossy(&args[1])
        )),
    }
}
