use crate::control::State;
use crate::paths;
use std::fs::File;
use std::io::{Read as _, Seek, Write as _};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const TAKO_DEV_DOMAIN: &str = "tako.test";
pub(super) const SHORT_DEV_DOMAIN: &str = "test";
pub(super) const LOCAL_DNS_LISTEN_ADDR: &str = "127.0.0.1:53535";
pub(super) const DEV_LOOPBACK_ADDR: &str = "127.77.0.1";
pub(super) const HTTP_REDIRECT_LISTEN_ADDR: &str = "127.0.0.1:47830";

#[derive(Debug, Clone)]
pub(super) struct Args {
    pub(super) listen_addr: String,
    pub(super) dns_ip: Option<String>,
}

pub(super) fn parse_args() -> Args {
    let mut listen_addr = "127.0.0.1:47831".to_string();
    let mut dns_ip = Some(DEV_LOOPBACK_ADDR.to_string());

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => {
                if let Some(v) = it.next()
                    && !v.trim().is_empty()
                {
                    listen_addr = v;
                }
            }
            "--dns-ip" => {
                if let Some(v) = it.next()
                    && !v.trim().is_empty()
                {
                    dns_ip = Some(v);
                }
            }
            "--tunnel-only" => dns_ip = None,
            _ => {}
        }
    }

    Args {
        listen_addr,
        dns_ip,
    }
}

pub(super) fn acquire_pid_lock(pid_path: &Path) -> Result<File, Box<dyn std::error::Error>> {
    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(pid_path)?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        write_pid(&mut file)?;
        return Ok(file);
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
        return Err(format!("flock({}) failed: {}", pid_path.display(), err).into());
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    if let Ok(old_pid) = contents.trim().parse::<i32>()
        && old_pid > 0
    {
        unsafe {
            libc::kill(old_pid, libc::SIGTERM);
        }
    }

    const MAX_RETRIES: u32 = 20;
    for _ in 0..MAX_RETRIES {
        std::thread::sleep(Duration::from_millis(100));
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            write_pid(&mut file)?;
            return Ok(file);
        }
    }

    Err(format!(
        "could not acquire dev-server lock at {} after sending SIGTERM (another instance may be stuck)",
        pid_path.display()
    )
    .into())
}

fn write_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn app_short_host(app_name: &str) -> String {
    format!("{}.{}", app_name, SHORT_DEV_DOMAIN)
}

fn app_host(app_name: &str) -> String {
    format!("{}.{}", app_name, TAKO_DEV_DOMAIN)
}

pub(crate) fn default_hosts(app_name: &str) -> Vec<String> {
    vec![app_short_host(app_name), app_host(app_name)]
}

pub(crate) fn advertised_https_port(state: &State) -> u16 {
    if state.advertised_ip == DEV_LOOPBACK_ADDR {
        443
    } else {
        state.listen_port
    }
}

pub(super) fn default_socket_path() -> PathBuf {
    paths::tako_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("dev-server.sock")
}

pub(super) fn port_from_listen(listen: &str, default_port: u16) -> u16 {
    listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(default_port)
}

pub(super) fn listen_port_from_addr(listen: &str) -> u16 {
    port_from_listen(listen, 47831)
}

pub(crate) fn ensure_tcp_listener_can_bind(
    listen_addr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match std::net::TcpListener::bind(listen_addr) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(e) => Err(format!("dev proxy could not bind on {}: {}", listen_addr, e).into()),
    }
}
