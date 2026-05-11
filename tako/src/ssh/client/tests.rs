use super::*;
use russh::keys::load_secret_key;

const ENCRYPTED_ED25519_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABCRv2KPnI\n\
IRphE01i7dWiijAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIBS7MYzXocRVMCqK\n\
uxD+2gS1Q9ZtX7zYh74IFWEKRZ4OAAAAkEa8z/fYTNnkt7g2yLcFM8IQFw67+aUeTzC6V2\n\
g+KleH6OSa4Q3cbBSMhWFkNY/IjTKNNg7P2XszrFMJblBkWokMvKgh3oGfJV4Axh3RZUsS\n\
ep5Su4gT/9WhaF3n32sxVB3BhK8IDBQBfsXh+YLhP0bZFdN+jLffuAQlINtoFYY8/4vvsn\n\
l4QMs5cmnWfrM0GQ==\n\
-----END OPENSSH PRIVATE KEY-----\n";

#[cfg(unix)]
fn can_bind_localhost() -> bool {
    std::net::TcpListener::bind(("127.0.0.1", 0)).is_ok()
}

#[cfg(unix)]
fn can_bind_unix_socket() -> bool {
    let Ok(dir) = tempfile::TempDir::new() else {
        return false;
    };
    let socket_path = dir.path().join("agent.sock");
    std::os::unix::net::UnixListener::bind(socket_path).is_ok()
}

#[cfg(unix)]
fn ssh_auth_sock_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[test]
fn test_ssh_config_creation() {
    let config = SshConfig::from_server("example.com", 22);
    assert_eq!(
        (config.host.as_str(), config.user.as_str(), config.port),
        ("example.com", "tako", 22)
    );
    assert_eq!(
        SshConfig::for_user("example.com", 2222, "root").user,
        "root"
    );
}

#[test]
fn test_ssh_config_keys_directory() {
    let config = SshConfig::from_server("example.com", 22);
    let keys_dir = config.keys_directory();
    assert!(keys_dir.ends_with(".ssh"));
}

#[test]
fn test_command_output_success() {
    let output = CommandOutput {
        exit_code: 0,
        stdout: "hello".to_string(),
        stderr: String::new(),
    };
    assert!(output.success());
}

#[test]
fn test_command_output_failure() {
    let output = CommandOutput {
        exit_code: 1,
        stdout: String::new(),
        stderr: "error".to_string(),
    };
    assert!(!output.success());
}

#[test]
fn test_command_output_combined() {
    let output = CommandOutput {
        exit_code: 0,
        stdout: "out".to_string(),
        stderr: "err".to_string(),
    };
    assert_eq!(output.combined(), "out\nerr");
}

#[test]
fn test_ssh_client_creation() {
    let config = SshConfig::from_server("example.com", 22);
    let client = SshClient::new(config);
    assert!(!client.is_connected());
}

#[test]
fn hello_response_interpretation_surfaces_invalid_command_message() {
    let resp = Response::Error {
        message: "Invalid command: unknown variant `hello`, expected one of ...".to_string(),
    };
    let err = SshClient::interpret_hello_response(&resp).unwrap_err();
    assert!(err.contains("tako-server handshake failed"));
    assert!(err.contains("Invalid command"));
}

#[test]
fn hello_response_interpretation_accepts_ok() {
    let resp = Response::Ok {
        data: serde_json::json!({"protocol_version": tako_core::PROTOCOL_VERSION}),
    };
    SshClient::interpret_hello_response(&resp).unwrap();
}

#[test]
fn extract_socket_stdout_returns_stdout() {
    let output = CommandOutput {
        exit_code: 0,
        stdout: "{\"status\":\"ok\"}\n".to_string(),
        stderr: String::new(),
    };
    let value = SshClient::extract_socket_stdout(output).unwrap();
    assert!(value.contains("\"status\":\"ok\""));
}

#[test]
fn extract_socket_stdout_surfaces_stderr_when_stdout_is_empty() {
    let output = CommandOutput {
        exit_code: 0,
        stdout: String::new(),
        stderr: "sh: nc: command not found".to_string(),
    };
    let err = SshClient::extract_socket_stdout(output).unwrap_err();
    assert!(err.to_string().contains("nc: command not found"));
}

#[test]
fn extract_socket_stdout_errors_on_empty_output() {
    let output = CommandOutput {
        exit_code: 0,
        stdout: "\n".to_string(),
        stderr: String::new(),
    };
    let err = SshClient::extract_socket_stdout(output).unwrap_err();
    assert!(err.to_string().contains("empty response"));
}

#[test]
fn socket_request_command_reads_one_line() {
    let command = SshClient::socket_request_command();
    assert!(command.contains("| head -n 1"));
    assert!(command.contains("nc -U '/var/run/tako/tako.sock'"));
}

#[test]
fn socket_request_command_on_path_uses_custom_socket() {
    let command = SshClient::socket_request_command_on_path("/tmp/tako-next.sock");
    assert!(command.contains("nc -U '/tmp/tako-next.sock'"));
    assert!(command.contains("| head -n 1"));
}

#[test]
fn tako_restart_command_uses_service_helper_with_root_or_sudo() {
    let command = SshClient::tako_restart_command();
    assert!(command.contains("if [ \"$(id -u)\" -eq 0 ]"));
    assert!(command.contains("command -v sudo"));
    assert!(command.contains("/usr/local/bin/tako-server-service restart"));
}

#[test]
fn tako_reload_command_uses_direct_sudo() {
    let command = SshClient::tako_reload_command();
    assert!(command.contains("if [ \"$(id -u)\" -eq 0 ]"));
    assert!(command.contains("command -v sudo"));
    assert!(command.contains("/usr/local/bin/tako-server-service reload"));
    // run_as_root uses direct sudo, not sh -c wrapping
    assert!(
        !command.contains("sudo sh -c"),
        "reload should use direct sudo for restricted sudoers compatibility"
    );
    assert!(command.contains("then sudo /usr/local/bin/tako-server-service reload"));
}

#[test]
fn run_as_root_uses_direct_sudo() {
    let cmd = SshClient::run_as_root("systemctl restart foo");
    // Direct sudo: `sudo systemctl restart foo`
    assert!(cmd.contains("sudo systemctl restart foo"));
    // No sh -c wrapping
    assert!(!cmd.contains("sudo sh -c"));
}

#[test]
fn run_with_root_or_sudo_uses_sh_c() {
    let cmd = SshClient::run_with_root_or_sudo("cat /etc/foo && echo ok");
    // sh -c wrapping for complex shell constructs
    assert!(cmd.contains("sudo sh -c 'cat /etc/foo && echo ok'"));
}

#[test]
fn run_with_root_or_sudo_preserves_github_token_env_when_allowed() {
    let cmd = SshClient::run_with_root_or_sudo("echo ok");
    assert!(cmd.contains("--preserve-env=GH_TOKEN,GITHUB_TOKEN"));
    assert!(cmd.contains("sudo sh -c 'echo ok'"));
}

#[test]
fn run_with_root_or_sudo_escapes_inner_single_quotes() {
    let cmd = SshClient::run_with_root_or_sudo("printf '%s' 'TOKEN=abc' > /etc/creds");
    // Inner single quotes must be escaped for the outer sh -c wrapper
    assert!(cmd.contains("sudo sh -c 'printf '\\''%s'\\'' '\\''TOKEN=abc'\\'' > /etc/creds'"));
}

#[test]
fn run_as_root_when_already_root() {
    let cmd = SshClient::run_as_root("systemctl restart foo");
    // When running as root (id -u == 0), execute directly without sudo
    assert!(cmd.contains("then systemctl restart foo"));
}

#[test]
fn tako_service_status_command_supports_openrc() {
    let command = SshClient::tako_service_status_command();
    assert!(command.contains("systemctl is-active tako-server"));
    assert!(command.contains("rc-service tako-server status"));
    assert!(command.contains("echo active"));
    assert!(command.contains("echo inactive"));
}

#[test]
fn install_server_script_installs_and_verifies_runtime_dependencies() {
    let script = super::tako::INSTALL_SERVER_SCRIPT;
    assert!(script.contains("install_libvips_runtime"));
    assert!(script.contains("libvips42t64"));
    assert!(script.contains("libheif-plugin-aomenc"));
    assert!(script.contains("apt-get install -y \"$apt_vips_pkg\" $apt_avif_encoder_pkg"));
    assert!(script.contains("install_missing_tako_server_runtime_deps"));
    assert!(script.contains("install_missing_tako_server_runtime_deps /usr/local/bin/tako-server"));
    assert!(script.contains("verify_tako_server_runtime_deps"));
    assert!(script.contains("missing_runtime_libraries /usr/local/bin/tako-server"));
    assert!(script.contains("not found"));
    assert!(script.contains("Error loading shared library"));

    let install_index = script
        .find("install -m 0755 \"$tmp_bin\" /usr/local/bin/tako-server")
        .unwrap();
    let runtime_deps_index = script
        .find("install_missing_tako_server_runtime_deps /usr/local/bin/tako-server")
        .unwrap();
    assert!(install_index < runtime_deps_index);
}

#[tokio::test]
async fn connect_to_unreachable_host_fails_quickly() {
    let mut cfg = SshConfig::from_server("10.255.255.1", 22);
    cfg.timeout = Duration::from_millis(200);
    let mut client = SshClient::new(cfg);

    let start = std::time::Instant::now();
    let err = client.connect().await.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(2));

    // Depending on platform/network, this can be a timeout or immediate connect failure.
    match err {
        SshError::Timeout(_) | SshError::Connection(_) => {}
        other => panic!("unexpected error: {}", other),
    }
}

#[tokio::test]
#[cfg(unix)]
async fn encrypted_keyfile_authenticates_with_configured_passphrase() {
    use russh::Channel;
    use russh::keys::{Algorithm, PrivateKey};
    use russh::server::{Server as _, Session};
    use std::sync::Arc;
    use tempfile::TempDir;

    if !can_bind_localhost() {
        return;
    }

    let keys_dir = TempDir::new().expect("temp keys dir");
    let key_path = keys_dir.path().join("id_ed25519");
    std::fs::write(&key_path, ENCRYPTED_ED25519_KEY).expect("write key file");
    // This should be private to satisfy OpenSSH conventions (and some parsers).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod key file");
    }

    let client_key = load_secret_key(&key_path, Some("testpass")).expect("load encrypted key");
    let allowed_key = client_key.public_key().clone();

    #[derive(Clone)]
    struct TestServer {
        allowed_key: russh::keys::PublicKey,
    }

    impl russh::server::Server for TestServer {
        type Handler = Self;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            self.clone()
        }
    }

    impl russh::server::Handler for TestServer {
        type Error = russh::Error;

        fn auth_publickey(
            &mut self,
            _user: &str,
            key: &russh::keys::PublicKey,
        ) -> impl Future<Output = Result<russh::server::Auth, Self::Error>> + Send {
            let accepted = key.key_data() == self.allowed_key.key_data();
            async move {
                if accepted {
                    Ok(russh::server::Auth::Accept)
                } else {
                    Ok(russh::server::Auth::reject())
                }
            }
        }

        fn channel_open_session(
            &mut self,
            channel: Channel<russh::server::Msg>,
            _session: &mut Session,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            let _ = channel.id();
            async { Ok(true) }
        }
    }

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let host_public_key = host_key.public_key().clone();

    let server_config = russh::server::Config {
        auth_rejection_time: Duration::from_millis(0),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        inactivity_timeout: Some(Duration::from_secs(5)),
        keys: vec![host_key],
        ..Default::default()
    };
    let server_config = Arc::new(server_config);

    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("local addr").port();
    let known_hosts_path = keys_dir.path().join("known_hosts");
    russh::keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        port,
        &host_public_key,
        &known_hosts_path,
    )
    .expect("write known_hosts entry");

    let mut server = TestServer { allowed_key };
    let server_task = tokio::spawn(async move {
        server
            .run_on_socket(server_config, &listener)
            .await
            .expect("server failed");
    });

    let _ssh_auth_sock_guard = ssh_auth_sock_test_lock().lock().await;
    let prev_sock = std::env::var("SSH_AUTH_SOCK").ok();
    // Ensure we don't accidentally use an agent in this test.
    unsafe { std::env::remove_var("SSH_AUTH_SOCK") };

    let mut ssh_config = SshConfig::from_server("127.0.0.1", port);
    ssh_config.timeout = Duration::from_secs(5);
    ssh_config.keys_dir = Some(keys_dir.path().to_path_buf());
    ssh_config.key_passphrase = Some("testpass".to_string());

    let mut ssh = SshClient::new(ssh_config);
    tokio::time::timeout(Duration::from_secs(10), ssh.connect())
        .await
        .expect("connect timed out")
        .expect("encrypted key auth should work");
    ssh.disconnect().await.expect("disconnect");

    // Cleanup.
    server_task.abort();
    match prev_sock {
        Some(v) => unsafe { std::env::set_var("SSH_AUTH_SOCK", v) },
        None => unsafe { std::env::remove_var("SSH_AUTH_SOCK") },
    }
}

#[tokio::test]
#[cfg(unix)]
async fn ssh_agent_authenticates_when_no_key_files_exist() {
    use russh::Channel;
    use russh::keys::agent::client::AgentClient;
    use russh::keys::{Algorithm, PrivateKey};
    use russh::server::{Server as _, Session};
    use std::process::Stdio;
    use std::sync::Arc;
    use tempfile::TempDir;

    if !can_bind_localhost() || !can_bind_unix_socket() {
        return;
    }

    #[derive(Clone)]
    struct TestServer {
        allowed_key: russh::keys::PublicKey,
    }

    impl russh::server::Server for TestServer {
        type Handler = Self;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            self.clone()
        }
    }

    impl russh::server::Handler for TestServer {
        type Error = russh::Error;

        fn auth_publickey(
            &mut self,
            _user: &str,
            key: &russh::keys::PublicKey,
        ) -> impl Future<Output = Result<russh::server::Auth, Self::Error>> + Send {
            let accepted = key.key_data() == self.allowed_key.key_data();
            async move {
                if accepted {
                    Ok(russh::server::Auth::Accept)
                } else {
                    Ok(russh::server::Auth::reject())
                }
            }
        }

        fn channel_open_session(
            &mut self,
            channel: Channel<russh::server::Msg>,
            _session: &mut Session,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            // We don't need to run any commands for this test; just allow opening.
            let _ = channel.id();
            async { Ok(true) }
        }
    }

    // Start a private ssh-agent with a temporary socket (daemonized by ssh-agent).
    let agent_dir = TempDir::new().expect("tempdir");
    let agent_path = agent_dir.path().join("agent.sock");
    let agent_out = tokio::process::Command::new("ssh-agent")
        .arg("-a")
        .arg(&agent_path)
        .arg("-s")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("start ssh-agent");
    if !agent_out.status.success() {
        return;
    }
    let agent_stdout = String::from_utf8_lossy(&agent_out.stdout);
    let Some(pid) = agent_stdout
        .split(';')
        .find_map(|part| part.trim().strip_prefix("SSH_AGENT_PID="))
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return;
    };

    // Generate a client key and load it into the agent.
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("client key");
    let client_pub = client_key.public_key().clone();

    let stream = tokio::net::UnixStream::connect(&agent_path)
        .await
        .expect("connect to agent");
    let mut agent = AgentClient::connect(stream);
    agent
        .add_identity(&client_key, &[])
        .await
        .expect("add identity");

    let _ssh_auth_sock_guard = ssh_auth_sock_test_lock().lock().await;
    // Point SSH_AUTH_SOCK at the test agent so SshClient can find it.
    let prev_sock = std::env::var("SSH_AUTH_SOCK").ok();
    // SAFETY: tests in this crate are not expected to rely on concurrent env var mutation.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &agent_path) };

    // Start an SSH server that accepts only the agent-loaded public key.
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let host_public_key = host_key.public_key().clone();

    let server_config = russh::server::Config {
        auth_rejection_time: Duration::from_millis(0),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        inactivity_timeout: Some(Duration::from_secs(5)),
        keys: vec![host_key],
        ..Default::default()
    };
    let server_config = Arc::new(server_config);

    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        let _ = tokio::process::Command::new("kill")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        return;
    };
    let port = listener.local_addr().expect("local addr").port();

    let mut server = TestServer {
        allowed_key: client_pub,
    };

    let server_task = tokio::spawn(async move {
        server
            .run_on_socket(server_config, &listener)
            .await
            .expect("server failed");
    });

    // Ensure we don't find any key files on disk.
    let keys_dir = TempDir::new().expect("temp keys dir");
    let known_hosts_path = keys_dir.path().join("known_hosts");
    russh::keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        port,
        &host_public_key,
        &known_hosts_path,
    )
    .expect("write known_hosts entry");
    let mut ssh_config = SshConfig::from_server("127.0.0.1", port);
    ssh_config.timeout = Duration::from_secs(5);
    ssh_config.keys_dir = Some(keys_dir.path().to_path_buf());

    let mut ssh = SshClient::new(ssh_config);
    tokio::time::timeout(Duration::from_secs(10), ssh.connect())
        .await
        .expect("connect timed out")
        .expect("agent auth should work");
    ssh.disconnect().await.expect("disconnect");

    // Cleanup.
    server_task.abort();
    let _ = tokio::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    if let Some(prev) = prev_sock {
        // SAFETY: see note above.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", prev) };
    } else {
        // SAFETY: see note above.
        unsafe { std::env::remove_var("SSH_AUTH_SOCK") };
    }
}
