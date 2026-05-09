#[cfg(any(target_os = "macos", test))]
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(target_os = "macos")]
use sha2::Digest;

use std::time::Duration;

#[cfg(target_os = "macos")]
use super::super::DEV_LOOPBACK_ADDR;

#[cfg(any(target_os = "macos", test))]
pub(crate) const DEV_PROXY_LABEL: &str = "sh.tako.dev-proxy";
#[cfg(any(target_os = "macos", test))]
pub(crate) const DEV_PROXY_BOOTSTRAP_LABEL: &str = "sh.tako.dev-bootstrap";
#[cfg(target_os = "macos")]
pub(crate) const DEV_PROXY_PLIST_PATH: &str =
    "/Library/Application Support/Tako/launchd/sh.tako.dev-proxy.plist";
#[cfg(target_os = "macos")]
pub(crate) const DEV_PROXY_BOOTSTRAP_PLIST_PATH: &str =
    "/Library/LaunchDaemons/sh.tako.dev-bootstrap.plist";
#[cfg(target_os = "macos")]
pub(crate) const DEV_PROXY_BINARY_PATH: &str =
    "/Library/Application Support/Tako/bin/tako-dev-proxy";
#[cfg(any(target_os = "macos", test))]
pub(crate) const DEV_PROXY_HTTPS_NAME: &str = "https";
#[cfg(any(target_os = "macos", test))]
pub(crate) const DEV_PROXY_HTTP_NAME: &str = "http";
#[cfg(test)]
pub(crate) const DEV_PROXY_IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DevProxyRepairPlan {
    None,
    ReloadService,
    InstallOrUpdate,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevProxyStatus {
    pub installed: bool,
    pub bootstrap_loaded: bool,
    pub alias_ready: bool,
    pub launchd_loaded: bool,
    pub https_ready: bool,
    pub http_ready: bool,
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn launchd_plist(binary_path: &Path) -> String {
    let binary = binary_path.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{DEV_PROXY_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
  </array>
  <key>KeepAlive</key>
  <false/>
  <key>Sockets</key>
  <dict>
    <key>{DEV_PROXY_HTTPS_NAME}</key>
    <dict>
      <key>SockNodeName</key>
      <string>127.77.0.1</string>
      <key>SockServiceName</key>
      <string>443</string>
      <key>SockPassive</key>
      <true/>
      <key>SockType</key>
      <string>stream</string>
    </dict>
    <key>{DEV_PROXY_HTTP_NAME}</key>
    <dict>
      <key>SockNodeName</key>
      <string>127.77.0.1</string>
      <key>SockServiceName</key>
      <string>80</string>
      <key>SockPassive</key>
      <true/>
      <key>SockType</key>
      <string>stream</string>
    </dict>
  </dict>
</dict>
</plist>
"#
    )
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn bootstrap_launchd_plist(binary_path: &Path) -> String {
    let binary = binary_path.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{DEV_PROXY_BOOTSTRAP_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>bootstrap</string>
  </array>
  <key>KeepAlive</key>
  <false/>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#
    )
}

#[cfg(any(target_os = "macos", test))]
fn plists_match_installed_binary(
    installed_binary: &Path,
    proxy_plist_contents: &str,
    bootstrap_plist_contents: &str,
) -> bool {
    proxy_plist_contents == launchd_plist(installed_binary)
        && bootstrap_plist_contents == bootstrap_launchd_plist(installed_binary)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn repair_plan(
    files_current: bool,
    bootstrap_loaded: bool,
    alias_ready: bool,
    launchd_loaded: bool,
    https_ready: bool,
    http_ready: bool,
) -> DevProxyRepairPlan {
    if !files_current || !bootstrap_loaded || !alias_ready {
        DevProxyRepairPlan::InstallOrUpdate
    } else if !launchd_loaded || !https_ready || !http_ready {
        DevProxyRepairPlan::ReloadService
    } else {
        DevProxyRepairPlan::None
    }
}

#[cfg(test)]
pub(crate) fn should_exit_for_idle(
    active_connections: usize,
    idle_for: Duration,
    idle_timeout: Duration,
) -> bool {
    active_connections == 0 && idle_for >= idle_timeout
}

#[cfg(target_os = "macos")]
pub(crate) fn install_binary_path() -> PathBuf {
    PathBuf::from(DEV_PROXY_BINARY_PATH)
}

#[cfg(target_os = "macos")]
pub(crate) fn plist_path() -> PathBuf {
    PathBuf::from(DEV_PROXY_PLIST_PATH)
}

#[cfg(target_os = "macos")]
pub(crate) fn bootstrap_plist_path() -> PathBuf {
    PathBuf::from(DEV_PROXY_BOOTSTRAP_PLIST_PATH)
}

#[cfg(any(target_os = "macos", test))]
fn install_action_line() -> &'static str {
    "Install local dev proxy for 127.77.0.1:80/443"
}

#[cfg(any(target_os = "macos", test))]
fn reload_action_line() -> &'static str {
    "Repair local dev proxy for 127.77.0.1:80/443"
}

#[cfg(target_os = "macos")]
fn locate_proxy_source_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let current_exe = std::env::current_exe()?;
    if let Some(root) = crate::paths::repo_root_from_exe(&current_exe) {
        let candidates = [
            root.join("target").join("debug").join("tako-dev-proxy"),
            root.join("target").join("release").join("tako-dev-proxy"),
        ];
        if candidates.iter().all(|candidate| !candidate.exists()) {
            let _ = std::process::Command::new("cargo")
                .args(["build", "-p", "tako", "--bin", "tako-dev-proxy"])
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        if let Some(found) = candidates.into_iter().find(|candidate| candidate.exists()) {
            return Ok(found);
        }
        return Err(
            "failed to locate 'tako-dev-proxy'. Build it with: cargo build -p tako --bin tako-dev-proxy"
                .into(),
        );
    }

    if let Some(parent) = current_exe.parent() {
        let sibling = parent.join("tako-dev-proxy");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    if let Some(path) = find_on_path("tako-dev-proxy") {
        return Ok(path);
    }

    Err(
        "failed to locate 'tako-dev-proxy'. Reinstall Tako CLI and retry: curl -fsSL https://tako.sh/install.sh | sh"
            .into(),
    )
}

#[cfg(target_os = "macos")]
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|entry| entry.join(name))
        .find(|candidate| candidate.exists())
}

#[cfg(target_os = "macos")]
fn hash_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex::encode(sha2::Sha256::digest(bytes)))
}

#[cfg(target_os = "macos")]
fn files_current(desired_binary: &Path) -> bool {
    let installed_binary = install_binary_path();
    let plist = plist_path();
    let bootstrap_plist = bootstrap_plist_path();
    if !installed_binary.is_file() || !plist.is_file() || !bootstrap_plist.is_file() {
        return false;
    }

    let installed_hash = hash_file(&installed_binary);
    let desired_hash = hash_file(desired_binary);
    if installed_hash.is_none() || installed_hash != desired_hash {
        return false;
    }

    let Some(proxy_plist_contents) = std::fs::read_to_string(&plist).ok() else {
        return false;
    };
    let Some(bootstrap_plist_contents) = std::fs::read_to_string(&bootstrap_plist).ok() else {
        return false;
    };
    plists_match_installed_binary(
        &installed_binary,
        &proxy_plist_contents,
        &bootstrap_plist_contents,
    )
}

#[cfg(target_os = "macos")]
fn files_installed() -> bool {
    install_binary_path().is_file() && plist_path().is_file() && bootstrap_plist_path().is_file()
}

#[cfg(target_os = "macos")]
fn launchd_loaded(label: &str) -> bool {
    let label = format!("system/{label}");
    std::process::Command::new("launchctl")
        .args(["print", &label])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn loopback_alias_present(ifconfig_output: &str, ip: &str) -> bool {
    ifconfig_output.lines().any(|line| {
        let mut parts = line.split_whitespace();
        matches!(parts.next(), Some("inet")) && parts.next() == Some(ip)
    })
}

#[cfg(target_os = "macos")]
fn loopback_alias_ready() -> bool {
    let Ok(output) = std::process::Command::new("ifconfig").arg("lo0").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    loopback_alias_present(&String::from_utf8_lossy(&output.stdout), DEV_LOOPBACK_ADDR)
}

#[cfg(target_os = "macos")]
pub(crate) fn status() -> DevProxyStatus {
    DevProxyStatus {
        installed: files_installed(),
        bootstrap_loaded: launchd_loaded(DEV_PROXY_BOOTSTRAP_LABEL),
        alias_ready: loopback_alias_ready(),
        launchd_loaded: launchd_loaded(DEV_PROXY_LABEL),
        https_ready: tcp_port_open(DEV_LOOPBACK_ADDR, 443, 150),
        http_ready: tcp_port_open(DEV_LOOPBACK_ADDR, 80, 150),
    }
}

#[cfg(target_os = "macos")]
fn install_binary_with_sudo(
    src: &Path,
    dest: &Path,
    mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(parent) = dest.parent() else {
        return Err(format!("invalid destination {}", dest.display()).into());
    };
    let parent_str = parent.to_string_lossy().to_string();
    let src_str = src.to_string_lossy().to_string();
    let dest_str = dest.to_string_lossy().to_string();
    sudo_run_checked(
        &["install", "-d", "-m", "755", &parent_str],
        &format!("creating {}", parent.display()),
    )?;
    sudo_run_checked(
        &["install", "-m", mode, &src_str, &dest_str],
        &format!("installing {}", dest.display()),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_parent_dir_with_sudo(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let Some(parent) = path.parent() else {
        return Err(format!("invalid path {}", path.display()).into());
    };
    let parent_str = parent.to_string_lossy().to_string();
    sudo_run_checked(
        &["install", "-d", "-m", "755", &parent_str],
        &format!("creating {}", parent.display()),
    )
}

#[cfg(target_os = "macos")]
fn bootout_launchd_service(label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let label = format!("system/{label}");
    let status = std::process::Command::new("sudo")
        .args(["launchctl", "bootout", &label])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() || status.code() == Some(3) {
        return Ok(());
    }
    Err(format!("booting out {label} failed").into())
}

#[cfg(target_os = "macos")]
fn bootstrap_launchd_service(
    label: &str,
    plist_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let launchd_label = format!("system/{label}");
    sudo_run_checked(
        &["launchctl", "bootstrap", "system", plist_path],
        &format!("bootstrapping {label} launchd service"),
    )?;
    sudo_run_checked(
        &["launchctl", "enable", &launchd_label],
        &format!("enabling {label} launchd service"),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_bootstrap_helper_with_sudo() -> Result<(), Box<dyn std::error::Error>> {
    let binary = install_binary_path();
    let binary_str = binary.to_string_lossy().to_string();
    sudo_run_checked(
        &[&binary_str, "bootstrap"],
        "running dev proxy bootstrap helper",
    )
}

/// Best-effort cleanup of the old `sh.tako.loopback-proxy` launchd service and
/// files from before the rename to `tako-dev-proxy`.
#[cfg(target_os = "macos")]
fn cleanup_old_loopback_proxy() {
    const OLD_LABEL: &str = "sh.tako.loopback-proxy";
    const OLD_BOOTSTRAP_LABEL: &str = "sh.tako.loopback-bootstrap";
    const OLD_PLIST: &str =
        "/Library/Application Support/Tako/launchd/sh.tako.loopback-proxy.plist";
    const OLD_BOOTSTRAP_PLIST: &str = "/Library/LaunchDaemons/sh.tako.loopback-bootstrap.plist";
    const OLD_BINARY: &str = "/Library/Application Support/Tako/bin/tako-loopback-proxy";

    let _ = std::process::Command::new("sudo")
        .args(["launchctl", "bootout", &format!("system/{OLD_LABEL}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new("sudo")
        .args([
            "launchctl",
            "bootout",
            &format!("system/{OLD_BOOTSTRAP_LABEL}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    for path in [OLD_PLIST, OLD_BOOTSTRAP_PLIST, OLD_BINARY] {
        if std::path::Path::new(path).exists() {
            let _ = std::process::Command::new("sudo")
                .args(["rm", "-f", path])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[cfg(target_os = "macos")]
fn install_or_update(desired_binary: &Path) -> Result<(), Box<dyn std::error::Error>> {
    cleanup_old_loopback_proxy();
    install_binary_with_sudo(desired_binary, &install_binary_path(), "755")?;
    ensure_parent_dir_with_sudo(&plist_path())?;
    write_system_file_with_sudo(DEV_PROXY_PLIST_PATH, &launchd_plist(&install_binary_path()))?;
    write_system_file_with_sudo(
        DEV_PROXY_BOOTSTRAP_PLIST_PATH,
        &bootstrap_launchd_plist(&install_binary_path()),
    )?;
    // Run the bootstrap helper synchronously first (sets up loopback alias +
    // proxy launchd service). Only then register the bootstrap plist for future
    // boots — otherwise RunAtLoad fires concurrently and races with this call.
    run_bootstrap_helper_with_sudo()?;
    bootout_launchd_service(DEV_PROXY_BOOTSTRAP_LABEL)?;
    bootstrap_launchd_service(DEV_PROXY_BOOTSTRAP_LABEL, DEV_PROXY_BOOTSTRAP_PLIST_PATH)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn reload_service() -> Result<(), Box<dyn std::error::Error>> {
    bootout_launchd_service(DEV_PROXY_LABEL)?;
    run_bootstrap_helper_with_sudo()?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn current_repair_plan() -> Result<DevProxyRepairPlan, Box<dyn std::error::Error>> {
    let desired_binary = locate_proxy_source_binary()?;
    let status = status();

    // If the service is fully operational, skip the binary hash freshness check.
    // A new binary will be installed next time repair is actually needed.
    if status.installed
        && status.bootstrap_loaded
        && status.alias_ready
        && status.launchd_loaded
        && status.https_ready
        && status.http_ready
    {
        return Ok(DevProxyRepairPlan::None);
    }

    let files_current = files_current(&desired_binary);
    Ok(repair_plan(
        files_current,
        status.bootstrap_loaded,
        status.alias_ready,
        status.launchd_loaded,
        status.https_ready,
        status.http_ready,
    ))
}

#[cfg(target_os = "macos")]
pub(crate) fn pending_sudo_action() -> Result<Option<&'static str>, Box<dyn std::error::Error>> {
    Ok(match current_repair_plan()? {
        DevProxyRepairPlan::InstallOrUpdate => Some(install_action_line()),
        DevProxyRepairPlan::ReloadService => Some(reload_action_line()),
        DevProxyRepairPlan::None => None,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_installed() -> Result<(), Box<dyn std::error::Error>> {
    let current_plan = current_repair_plan()?;

    if current_plan == DevProxyRepairPlan::None {
        return Ok(());
    }

    if !crate::output::is_interactive() && !crate::output::is_root() {
        return Err(
            "local dev proxy is not configured; run `tako dev` interactively once to install it"
                .into(),
        );
    }

    let loading = match current_plan {
        DevProxyRepairPlan::InstallOrUpdate => "Setting up",
        DevProxyRepairPlan::ReloadService => "Starting",
        DevProxyRepairPlan::None => unreachable!(),
    };
    let success = match current_plan {
        DevProxyRepairPlan::InstallOrUpdate => "Set up",
        DevProxyRepairPlan::ReloadService => "Ready",
        DevProxyRepairPlan::None => unreachable!(),
    };

    crate::output::with_spinner(
        loading,
        success,
        || -> Result<(), Box<dyn std::error::Error>> {
            match current_plan {
                DevProxyRepairPlan::InstallOrUpdate => {
                    let desired_binary = locate_proxy_source_binary()?;
                    install_or_update(&desired_binary)?;
                }
                DevProxyRepairPlan::ReloadService => {
                    reload_service()?;
                }
                DevProxyRepairPlan::None => unreachable!(),
            }

            // Check non-network state once (files, launchd, alias won't change by waiting).
            let verified = status();
            if !(verified.installed
                && verified.bootstrap_loaded
                && verified.alias_ready
                && verified.launchd_loaded)
            {
                return Err("local dev proxy setup verification failed".into());
            }

            // The service was just (re)started — give it time to bind its ports.
            let (mut https_ok, mut http_ok) = (verified.https_ready, verified.http_ready);
            if !(https_ok && http_ok) {
                for _ in 0..20 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    https_ok = https_ok || tcp_port_open(DEV_LOOPBACK_ADDR, 443, 150);
                    http_ok = http_ok || tcp_port_open(DEV_LOOPBACK_ADDR, 80, 150);
                    if https_ok && http_ok {
                        break;
                    }
                }
            }
            if !(https_ok && http_ok) {
                return Err("local dev proxy setup verification failed".into());
            }

            Ok(())
        },
    )?;
    Ok(())
}

// ── Sudo helpers ─────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub(crate) fn sudo_run_checked(
    args: &[&str],
    context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("sudo").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{context} failed").into())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn write_system_file_with_sudo(
    path: &str,
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "tako-dns-resolver-{}-{}",
        std::process::id(),
        unique
    ));
    std::fs::write(&tmp, content)?;
    let tmp_str = tmp.to_string_lossy().to_string();
    let install_args = ["install", "-m", "644", tmp_str.as_str(), path];
    let result = sudo_run_checked(&install_args, &format!("installing {path}"));
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(target_os = "macos")]
pub(crate) fn tcp_port_open(ip: &str, port: u16, timeout_ms: u64) -> bool {
    use std::net::{Ipv4Addr, SocketAddr};
    let Ok(ipv4) = ip.parse::<Ipv4Addr>() else {
        return false;
    };
    let addr = SocketAddr::from((ipv4, port));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).is_ok()
}

// ── DNS resolver ─────────────────────────────────────────────────────────────

#[cfg(any(target_os = "macos", test))]
pub(crate) fn local_dns_resolver_contents(port: u16) -> String {
    format!("nameserver 127.0.0.1\nport {port}\n")
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_local_dns_resolver(contents: &str) -> (Option<String>, Option<u16>) {
    let mut nameserver = None;
    let mut port = None;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        match key {
            "nameserver" if !value.is_empty() => nameserver = Some(value.to_string()),
            "port" => {
                if let Ok(v) = value.parse::<u16>() {
                    port = Some(v);
                }
            }
            _ => {}
        }
    }

    (nameserver, port)
}

#[cfg(target_os = "macos")]
fn resolver_file_matches(path: &str, port: u16) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let (nameserver, configured_port) = parse_local_dns_resolver(&contents);
    nameserver.as_deref() == Some("127.0.0.1") && configured_port == Some(port)
}

#[cfg(target_os = "macos")]
fn local_dns_resolver_configured(port: u16) -> bool {
    resolver_file_matches(super::super::TAKO_RESOLVER_FILE, port)
}

#[cfg(target_os = "macos")]
fn short_dns_resolver_configured(port: u16) -> bool {
    resolver_file_matches(super::super::SHORT_RESOLVER_FILE, port)
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_local_dns_resolver_configured(
    port: u16,
) -> Result<bool, Box<dyn std::error::Error>> {
    let tako_ok = local_dns_resolver_configured(port);
    let short_ok = short_dns_resolver_configured(port);

    if tako_ok && short_ok {
        return Ok(true);
    }

    if !tako_ok && !crate::output::is_interactive() && !crate::output::is_root() {
        return Err(format!(
            "local DNS resolver is not configured at {}; run `tako dev` interactively once to install it",
            super::super::TAKO_RESOLVER_FILE
        )
        .into());
    }

    sudo_run_checked(
        &["install", "-d", "-m", "755", super::super::RESOLVER_DIR],
        "creating /etc/resolver",
    )?;

    if !tako_ok {
        write_system_file_with_sudo(
            super::super::TAKO_RESOLVER_FILE,
            &local_dns_resolver_contents(port),
        )?;

        if !local_dns_resolver_configured(port) {
            return Err("local DNS resolver setup verification failed".into());
        }
    }

    let short_active = if short_ok {
        true
    } else if !Path::new(super::super::SHORT_RESOLVER_FILE).exists() {
        write_system_file_with_sudo(
            super::super::SHORT_RESOLVER_FILE,
            &local_dns_resolver_contents(port),
        )?;
        short_dns_resolver_configured(port)
    } else if crate::output::is_interactive() {
        crate::output::warning(
            "Another tool owns /etc/resolver/test. Override it for shorter *.test URLs?",
        );
        if crate::output::confirm("Override /etc/resolver/test?", false).unwrap_or(false) {
            write_system_file_with_sudo(
                super::super::SHORT_RESOLVER_FILE,
                &local_dns_resolver_contents(port),
            )?;
            short_dns_resolver_configured(port)
        } else {
            crate::output::muted("Skipped — using *.tako.test URLs instead.");
            false
        }
    } else {
        false
    };

    Ok(short_active)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn local_dns_sudo_action_line() -> &'static str {
    "Configure DNS"
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn sudo_setup_action_items(
    ca_action: Option<&str>,
    local_dns_needed: bool,
    dev_proxy_action: Option<&str>,
) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(action) = ca_action {
        items.push(action.to_string());
    }
    if local_dns_needed {
        items.push(local_dns_sudo_action_line().to_string());
    }
    if let Some(action) = dev_proxy_action {
        items.push(action.to_string());
    }
    items
}

#[cfg(target_os = "macos")]
pub(crate) fn explain_pending_sudo_setup(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    if !crate::output::is_interactive() {
        return Ok(());
    }

    let dns_needed = !local_dns_resolver_configured(port) || !short_dns_resolver_configured(port);
    let items = sudo_setup_action_items(
        super::tls::pending_sudo_action()?,
        dns_needed,
        pending_sudo_action()?,
    );
    if items.is_empty() {
        return Ok(());
    }

    crate::output::warning("sudo access required");
    if crate::output::is_pretty() {
        eprintln!("Tako needs this to set up your development environment:");
        for item in items {
            eprintln!("- {item}");
        }
    }
    eprintln!();

    let status = std::process::Command::new("sudo")
        .arg("-v")
        .status()
        .map_err(|e| -> Box<dyn std::error::Error> { format!("failed to run sudo: {e}").into() })?;
    if !status.success() {
        return Err(crate::output::silent_exit_error().into());
    }

    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn local_dns_resolver_values() -> Option<(String, u16)> {
    let contents = std::fs::read_to_string(super::super::TAKO_RESOLVER_FILE).ok()?;
    let (nameserver, port) = parse_local_dns_resolver(&contents);
    Some((nameserver?, port?))
}

#[cfg(test)]
mod tests;
