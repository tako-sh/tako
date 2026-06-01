use super::proxy_protocol::{ProxyProtocolError, ProxyProtocolResult, read_proxy_protocol_header};
use super::{TakoProxy, TrustedProxyConfig};
use async_trait::async_trait;
use pingora_core::apps::ServerApp;
use pingora_core::listeners::{ServerAddress, TcpSocketOptions, TlsAcceptCallbacks};
use pingora_core::protocols::l4::listener::Listener;
use pingora_core::protocols::l4::socket::SocketAddr as PingoraSocketAddr;
use pingora_core::protocols::l4::stream::Stream as L4Stream;
use pingora_core::protocols::tls::server::{handshake, handshake_with_callback};
use pingora_core::protocols::{GetSocketDigest, SocketDigest, Stream};
use pingora_core::server::ShutdownWatch;
use pingora_core::services::Service as ServiceTrait;
use pingora_core::tls::ssl::{
    AlpnError, SslAcceptor, SslAcceptorBuilder, SslFiletype, SslMethod, SslRef,
    SslSessionCacheMode, select_next_proto,
};
use pingora_core::{Error, ErrorType, OrErr, Result};
use pingora_proxy::{HttpProxy, http_proxy};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::net::TcpSocket;

const H2_H1_ALPN_WIRE_PREFERENCE: &[u8] = b"\x02h2\x08http/1.1";
const LISTENER_BACKLOG: u32 = 65535;
const PROXY_PROTOCOL_HEADER_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct ProxyProtocolService {
    name: String,
    app_logic: Option<HttpProxy<TakoProxy>>,
    endpoints: Vec<ProxyProtocolEndpoint>,
    trusted_proxy: TrustedProxyConfig,
    pub(crate) threads: Option<usize>,
}

#[derive(Clone)]
struct ProxyProtocolEndpoint {
    address: ServerAddress,
    tls: Option<Arc<ProxyProtocolTlsAcceptor>>,
}

pub(crate) struct ProxyProtocolTlsAcceptor {
    acceptor: SslAcceptor,
    callbacks: Option<TlsAcceptCallbacks>,
}

impl ProxyProtocolService {
    pub(crate) fn new(
        name: impl Into<String>,
        conf: &Arc<pingora_core::server::configuration::ServerConf>,
        app: TakoProxy,
        trusted_proxy: TrustedProxyConfig,
    ) -> Self {
        let mut app_logic = http_proxy(conf, app);
        let mut options = pingora_core::apps::HttpServerOptions::default();
        options.keepalive_request_limit = Some(1000);
        app_logic.server_options = Some(options);

        Self {
            name: name.into(),
            app_logic: Some(app_logic),
            endpoints: Vec::new(),
            trusted_proxy,
            threads: None,
        }
    }

    pub(crate) fn add_tcp_with_settings(&mut self, addr: &str, sock_opt: TcpSocketOptions) {
        self.endpoints.push(ProxyProtocolEndpoint {
            address: ServerAddress::Tcp(addr.into(), Some(sock_opt)),
            tls: None,
        });
    }

    pub(crate) fn add_tls_with_settings(
        &mut self,
        addr: &str,
        sock_opt: Option<TcpSocketOptions>,
        tls: Arc<ProxyProtocolTlsAcceptor>,
    ) {
        self.endpoints.push(ProxyProtocolEndpoint {
            address: ServerAddress::Tcp(addr.into(), sock_opt),
            tls: Some(tls),
        });
    }

    async fn run_endpoint(
        app_logic: Arc<HttpProxy<TakoProxy>>,
        endpoint: BuiltProxyProtocolEndpoint,
        trusted_proxy: TrustedProxyConfig,
        mut shutdown: ShutdownWatch,
    ) {
        loop {
            let new_io = tokio::select! {
                new_io = endpoint.listener.accept() => new_io,
                shutdown_signal = shutdown.changed() => {
                    match shutdown_signal {
                        Ok(()) if !*shutdown.borrow() => continue,
                        Ok(()) => {
                            tracing::info!("Shutting down {}", endpoint.address);
                            break;
                        }
                        Err(error) => {
                            tracing::error!("shutdown_signal error {error}");
                            break;
                        }
                    }
                }
            };

            match new_io {
                Ok(mut io) => {
                    if let Err(error) = io.set_nodelay() {
                        tracing::warn!("Failed to set TCP_NODELAY: {error}");
                    }
                    let Some(proxy_ip) = stream_peer_ip(&io) else {
                        tracing::warn!("Rejected PROXY protocol connection with no peer address");
                        continue;
                    };
                    if !trusted_proxy.trusts_proxy_ip(&proxy_ip) {
                        tracing::warn!(
                            %proxy_ip,
                            "Rejected PROXY protocol connection from untrusted peer"
                        );
                        continue;
                    }

                    let app = app_logic.clone();
                    let shutdown = shutdown.clone();
                    let tls = endpoint.tls.clone();
                    tokio::spawn(async move {
                        let peer_addr = stream_peer_addr(&io);
                        match handle_proxy_protocol_stream(io, tls, app, shutdown).await {
                            Ok(()) => {}
                            Err(error) => {
                                if let Some(addr) = peer_addr {
                                    tracing::error!(
                                        "Downstream PROXY protocol error from {addr}: {error}"
                                    );
                                } else {
                                    tracing::error!("Downstream PROXY protocol error: {error}");
                                }
                            }
                        }
                    });
                }
                Err(error) => {
                    tracing::error!("Accept() failed {error}");
                }
            }
        }
    }
}

#[derive(Clone)]
struct BuiltProxyProtocolEndpoint {
    address: String,
    listener: Arc<Listener>,
    tls: Option<Arc<ProxyProtocolTlsAcceptor>>,
}

#[async_trait]
impl ServiceTrait for ProxyProtocolService {
    async fn start_service(
        &mut self,
        #[cfg(unix)] fds: Option<pingora_core::server::ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
    ) {
        let mut endpoints = Vec::with_capacity(self.endpoints.len());
        for endpoint in &self.endpoints {
            #[cfg(unix)]
            let listener = bind_listener(&endpoint.address, fds.clone())
                .await
                .expect("Failed to build PROXY protocol listener");
            #[cfg(windows)]
            let listener = bind_listener(&endpoint.address)
                .await
                .expect("Failed to build PROXY protocol listener");
            endpoints.push(BuiltProxyProtocolEndpoint {
                address: endpoint.address.as_ref().to_string(),
                listener: Arc::new(listener),
                tls: endpoint.tls.clone(),
            });
        }

        let app_logic = self
            .app_logic
            .take()
            .expect("can only start_service() once");
        let app_logic = Arc::new(app_logic);

        let mut handlers = Vec::new();
        for endpoint in endpoints {
            for _ in 0..listeners_per_fd {
                let endpoint = endpoint.clone();
                let app_logic = app_logic.clone();
                let shutdown = shutdown.clone();
                let trusted_proxy = self.trusted_proxy.clone();
                handlers.push(tokio::spawn(async move {
                    Self::run_endpoint(app_logic, endpoint, trusted_proxy, shutdown).await;
                }));
            }
        }

        for handler in handlers {
            let _ = handler.await;
        }
        app_logic.cleanup().await;
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn threads(&self) -> Option<usize> {
        self.threads
    }
}

#[cfg(unix)]
async fn bind_listener(
    address: &ServerAddress,
    fds: Option<pingora_core::server::ListenFds>,
) -> Result<Listener> {
    let addr = address.as_ref();
    if let Some(fds_table) = fds {
        let mut table = fds_table.lock().await;
        if let Some(fd) = table.get(addr).copied() {
            return listener_from_raw_fd(fd);
        }
        let listener = bind_new_listener(address).await?;
        table.add(addr.to_string(), listener.as_raw_fd());
        return Ok(listener);
    }

    bind_new_listener(address).await
}

#[cfg(windows)]
async fn bind_listener(address: &ServerAddress) -> Result<Listener> {
    bind_new_listener(address).await
}

#[cfg(unix)]
fn listener_from_raw_fd(fd: std::os::unix::io::RawFd) -> Result<Listener> {
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    std_listener
        .set_nonblocking(true)
        .or_err(ErrorType::BindError, "failed to set listener nonblocking")?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .or_err(ErrorType::BindError, "failed to convert inherited listener")?;
    Ok(listener.into())
}

#[cfg(windows)]
fn listener_from_raw_socket(socket: std::os::windows::io::RawSocket) -> Result<Listener> {
    let std_listener = unsafe { std::net::TcpListener::from_raw_socket(socket) };
    std_listener
        .set_nonblocking(true)
        .or_err(ErrorType::BindError, "failed to set listener nonblocking")?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .or_err(ErrorType::BindError, "failed to convert inherited listener")?;
    Ok(listener.into())
}

async fn bind_new_listener(address: &ServerAddress) -> Result<Listener> {
    let ServerAddress::Tcp(addr, options) = address else {
        return Err(Error::explain(
            ErrorType::BindError,
            "PROXY protocol listener only supports TCP",
        ));
    };

    let sock_addr = addr
        .to_socket_addrs()
        .or_err_with(ErrorType::BindError, || {
            format!("Invalid PROXY protocol listen address {addr}")
        })?
        .next()
        .ok_or_else(|| {
            Error::explain(
                ErrorType::BindError,
                format!("Invalid PROXY protocol listen address {addr}"),
            )
        })?;

    let socket = match sock_addr {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .or_err_with(ErrorType::BindError, || {
        format!("failed to create listener socket for {sock_addr}")
    })?;
    socket
        .set_reuseaddr(true)
        .or_err(ErrorType::BindError, "failed to set SO_REUSEADDR")?;
    #[cfg(unix)]
    if options
        .as_ref()
        .and_then(|options| options.so_reuseport)
        .unwrap_or(false)
    {
        socket
            .set_reuseport(true)
            .or_err(ErrorType::BindError, "failed to set SO_REUSEPORT")?;
    }
    socket
        .bind(sock_addr)
        .or_err_with(ErrorType::BindError, || format!("bind() failed on {addr}"))?;
    Ok(socket
        .listen(LISTENER_BACKLOG)
        .or_err_with(ErrorType::BindError, || {
            format!("listen() failed on {addr}")
        })?
        .into())
}

impl ProxyProtocolTlsAcceptor {
    pub(crate) fn with_cert_files(cert_path: &str, key_path: &str) -> Result<Self> {
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).or_err(
            ErrorType::Custom("TLSConfigError"),
            "fail to create mozilla_intermediate_v5 Acceptor",
        )?;
        builder
            .set_private_key_file(key_path, SslFiletype::PEM)
            .or_err_with(ErrorType::Custom("TLSConfigError"), || {
                format!("fail to read key file {key_path}")
            })?;
        builder
            .set_certificate_chain_file(cert_path)
            .or_err_with(ErrorType::Custom("TLSConfigError"), || {
                format!("fail to read cert file {cert_path}")
            })?;
        Ok(Self::from_builder(builder, None))
    }

    pub(crate) fn with_callbacks(callbacks: TlsAcceptCallbacks) -> Result<Self> {
        let builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).or_err(
            ErrorType::Custom("TLSConfigError"),
            "fail to create mozilla_intermediate_v5 Acceptor",
        )?;
        Ok(Self::from_builder(builder, Some(callbacks)))
    }

    fn from_builder(
        mut builder: SslAcceptorBuilder,
        callbacks: Option<TlsAcceptCallbacks>,
    ) -> Self {
        configure_tls_acceptor_builder(&mut builder);
        Self {
            acceptor: builder.build(),
            callbacks,
        }
    }

    async fn tls_handshake(&self, stream: L4Stream) -> Result<Stream> {
        let stream = if let Some(callbacks) = self.callbacks.as_ref() {
            handshake_with_callback(&self.acceptor, stream, callbacks).await?
        } else {
            handshake(&self.acceptor, stream).await?
        };
        Ok(Box::new(stream))
    }
}

async fn handle_proxy_protocol_stream(
    mut stream: L4Stream,
    tls: Option<Arc<ProxyProtocolTlsAcceptor>>,
    app_logic: Arc<HttpProxy<TakoProxy>>,
    shutdown: ShutdownWatch,
) -> Result<()> {
    let parsed =
        read_proxy_protocol_header_with_timeout(&mut stream, PROXY_PROTOCOL_HEADER_TIMEOUT).await?;
    if let Some(source_addr) = parsed.source_addr {
        set_stream_peer_addr(&mut stream, source_addr);
    }

    let stream: Stream = if let Some(tls) = tls {
        tls.tls_handshake(stream).await?
    } else {
        Box::new(stream)
    };

    handle_event(stream, app_logic, shutdown).await;
    Ok(())
}

async fn read_proxy_protocol_header_with_timeout<R>(
    reader: &mut R,
    timeout: Duration,
) -> Result<ProxyProtocolResult>
where
    R: AsyncRead + Unpin,
{
    match tokio::time::timeout(timeout, read_proxy_protocol_header(reader)).await {
        Ok(Ok(parsed)) => Ok(parsed),
        Ok(Err(error)) => Err(proxy_protocol_error(error)),
        Err(_) => Err(Error::explain(
            ErrorType::ReadTimedout,
            "Timed out waiting for PROXY protocol header",
        )),
    }
}

async fn handle_event(
    event: Stream,
    app_logic: Arc<HttpProxy<TakoProxy>>,
    shutdown: ShutdownWatch,
) {
    let mut reuse_event = app_logic.process_new(event, &shutdown).await;
    while let Some(event) = reuse_event {
        reuse_event = app_logic.process_new(event, &shutdown).await;
    }
}

fn proxy_protocol_error(error: ProxyProtocolError) -> Box<Error> {
    Error::explain(
        ErrorType::ReadError,
        format!("Invalid PROXY protocol header: {error}"),
    )
}

fn stream_peer_ip(stream: &L4Stream) -> Option<IpAddr> {
    stream_peer_addr(stream).map(|addr| addr.ip())
}

fn stream_peer_addr(stream: &L4Stream) -> Option<SocketAddr> {
    stream
        .get_socket_digest()
        .and_then(|digest| digest.peer_addr().and_then(|addr| addr.as_inet()).copied())
}

fn set_stream_peer_addr(stream: &mut L4Stream, source_addr: SocketAddr) {
    #[cfg(unix)]
    let digest = SocketDigest::from_raw_fd(stream.as_raw_fd());
    #[cfg(windows)]
    let digest = SocketDigest::from_raw_socket(stream.as_raw_socket());
    let _ = digest
        .peer_addr
        .set(Some(PingoraSocketAddr::Inet(source_addr)));
    stream.set_socket_digest(digest);
}

fn enable_h2(builder: &mut SslAcceptorBuilder) {
    builder.set_alpn_select_callback(prefer_h2);
}

fn configure_tls_acceptor_builder(builder: &mut SslAcceptorBuilder) {
    enable_h2(builder);
    disable_tls_session_cache(builder);
}

fn disable_tls_session_cache(builder: &mut SslAcceptorBuilder) {
    builder.set_session_cache_mode(SslSessionCacheMode::OFF);
}

fn prefer_h2<'a>(_ssl: &mut SslRef, alpn_in: &'a [u8]) -> Result<&'a [u8], AlpnError> {
    if alpn_in.is_empty() {
        return Err(AlpnError::NOACK);
    }

    select_next_proto(H2_H1_ALPN_WIRE_PREFERENCE, alpn_in).ok_or(AlpnError::NOACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_protocol_timeout_applies_to_header_read() {
        let (_client, mut server) = tokio::io::duplex(64);

        let error = read_proxy_protocol_header_with_timeout(&mut server, Duration::from_millis(1))
            .await
            .expect_err("idle downstream should time out waiting for PROXY header");

        assert_eq!(error.etype, ErrorType::ReadTimedout);
    }

    #[test]
    fn tls_acceptor_disables_internal_session_cache() {
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())
            .expect("TLS builder should initialize");

        disable_tls_session_cache(&mut builder);

        let previous_mode = builder.set_session_cache_mode(SslSessionCacheMode::SERVER);
        assert_eq!(previous_mode, SslSessionCacheMode::OFF);
    }
}
