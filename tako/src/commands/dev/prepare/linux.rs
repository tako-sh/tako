//! Linux portless dev mode setup
//!
//! Uses iptables redirect rules to transparently forward privileged ports on a
//! dedicated loopback alias (127.77.0.1) to the unprivileged ports used by
//! tako-dev-server. One-time sudo, persisted via a systemd oneshot service.
//!
//! On NixOS, imperative setup would be wiped by `nixos-rebuild`, so we detect
//! it and print a `configuration.nix` snippet instead.

#[cfg(target_os = "linux")]
use super::{DEV_LOOPBACK_ADDR, LOCAL_DNS_PORT};

#[cfg(target_os = "linux")]
const DEV_HTTPS_PORT: u16 = 47831;
#[cfg(target_os = "linux")]
const DEV_HTTP_PORT: u16 = 47830;

#[cfg(target_os = "linux")]
const DEV_PROXY_SERVICE_PATH: &str = "/etc/systemd/system/tako-dev-proxy.service";
#[cfg(target_os = "linux")]
const DEV_PROXY_SOCKET_NAME: &str = "tako-dev-proxy.socket";
#[cfg(target_os = "linux")]
const DEV_PROXY_SOCKET_PATH: &str = "/etc/systemd/system/tako-dev-proxy.socket";

#[cfg(target_os = "linux")]
pub(crate) const SYSTEMD_SERVICE_NAME: &str = "tako-dev-redirect.service";
#[cfg(target_os = "linux")]
const SYSTEMD_SERVICE_PATH: &str = "/etc/systemd/system/tako-dev-redirect.service";
#[cfg(target_os = "linux")]
const RESOLVED_DROP_IN_FILE: &str = "/etc/systemd/resolved.conf.d/tako-dev.conf";

// ─── Status & repair plan ───────────────────────────────────────────────────

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinuxSetupStatus {
    pub loopback_alias: bool,
    pub redirect_443: bool,
    pub redirect_80: bool,
    pub redirect_dns: bool,
    pub dns_configured: bool,
    pub service_installed: bool,
    pub is_nixos: bool,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinuxRepairPlan {
    /// Everything is working.
    None,
    /// First run or NixOS config not applied — full setup needed.
    SetupAll,
    /// Redirect rules are missing (e.g. after reboot without systemd service).
    RepairRedirects,
    /// NixOS detected, setup not done — print config snippet.
    NixOsManual,
}

// ─── Pure parsing functions (testable on any platform) ──────────────────────

/// Parse `ip addr show dev lo` output for the loopback alias.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_loopback_alias(ip_addr_output: &str, ip: &str) -> bool {
    // Match `inet {ip}/` to avoid partial prefix matches like 127.77.0.100
    let needle = format!("inet {ip}/");
    ip_addr_output.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with(&needle)
    })
}

/// Parse `iptables -t nat -L OUTPUT -n` output for a specific redirect rule.
///
/// We look for a line containing `REDIRECT` with `dpt:{dport}` and `redir ports {to_port}`
/// scoped to the destination IP. Port needles are anchored with a trailing space
/// to avoid substring false-positives (e.g. `dpt:80` matching `dpt:8080`).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_iptables_redirect(
    iptables_output: &str,
    dest_ip: &str,
    dport: u16,
    to_port: u16,
) -> bool {
    // Trailing space anchors prevent prefix matches like dpt:80 matching dpt:8080.
    // iptables -n output separates fields with spaces, so each token ends with
    // either a space or is at end-of-line. We check both.
    let dport_needle = format!("dpt:{dport}");
    let redir_needle = format!("redir ports {to_port}");
    iptables_output.lines().any(|line| {
        line.contains("REDIRECT")
            && line.contains(dest_ip)
            && has_word(line, &dport_needle)
            && has_word(line, &redir_needle)
    })
}

/// Check that `needle` appears in `line` as a complete token — not as a prefix
/// of a longer token. The character after `needle` must be a space, EOL, or
/// absent.
#[cfg(any(target_os = "linux", test))]
fn has_word(line: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = line[start..].find(needle) {
        let abs = start + pos;
        let after = abs + needle.len();
        if after >= line.len() || line.as_bytes()[after] == b' ' {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Compute the repair plan from the current status.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn repair_plan(status: &LinuxSetupStatus) -> LinuxRepairPlan {
    let redirects_ok =
        status.redirect_443 && status.redirect_80 && status.redirect_dns && status.loopback_alias;

    if redirects_ok && status.dns_configured {
        return LinuxRepairPlan::None;
    }

    // On NixOS, imperative changes are wiped by nixos-rebuild. Always direct
    // users to their configuration.nix unless everything is already working.
    if status.is_nixos {
        return LinuxRepairPlan::NixOsManual;
    }

    if status.service_installed && !redirects_ok {
        return LinuxRepairPlan::RepairRedirects;
    }

    LinuxRepairPlan::SetupAll
}

// ─── Content generators ─────────────────────────────────────────────────────

/// systemd oneshot service that restores the loopback alias and iptables rules
/// at boot.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn systemd_service_contents() -> String {
    format!(
        "\
[Unit]
Description=Tako dev port redirect (127.77.0.1)
After=network-pre.target
Before=network.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=-/sbin/ip addr add 127.77.0.1/8 dev lo
ExecStart=/bin/sh -c '/sbin/iptables -t nat -C OUTPUT -d 127.77.0.1 -p tcp --dport 443 -j REDIRECT --to-port {https} 2>/dev/null || /sbin/iptables -t nat -A OUTPUT -d 127.77.0.1 -p tcp --dport 443 -j REDIRECT --to-port {https}'
ExecStart=/bin/sh -c '/sbin/iptables -t nat -C OUTPUT -d 127.77.0.1 -p tcp --dport 80 -j REDIRECT --to-port {http} 2>/dev/null || /sbin/iptables -t nat -A OUTPUT -d 127.77.0.1 -p tcp --dport 80 -j REDIRECT --to-port {http}'
ExecStart=/bin/sh -c '/sbin/iptables -t nat -C OUTPUT -d 127.77.0.1 -p udp --dport 53 -j REDIRECT --to-port {dns} 2>/dev/null || /sbin/iptables -t nat -A OUTPUT -d 127.77.0.1 -p udp --dport 53 -j REDIRECT --to-port {dns}'

[Install]
WantedBy=multi-user.target
",
        https = 47831,
        http = 47830,
        dns = 53535,
    )
}

/// systemd-resolved drop-in that routes `*.test` and `*.tako.test` queries to
/// the loopback alias (which iptables redirects to the dev server DNS on port
/// 53535).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn resolved_drop_in_contents() -> String {
    "\
[Resolve]
DNS=127.77.0.1
Domains=~tako.test ~test
"
    .to_string()
}

/// systemd service for the dev proxy (LAN mode).
/// Runs as the installing user with CAP_NET_BIND_SERVICE so it can bind port 443.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn dev_proxy_service_contents(binary_path: &str, user: &str) -> String {
    format!(
        "\
[Unit]
Description=Tako dev proxy (LAN mode)
After=network.target

[Service]
Type=simple
ExecStart={binary_path}
User={user}
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
Restart=no

[Install]
WantedBy=multi-user.target
"
    )
}

/// systemd socket unit for the dev proxy control socket.
/// Socket-activates the proxy when the dev-server connects.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn dev_proxy_socket_contents() -> String {
    "\
[Unit]
Description=Tako dev proxy control socket

[Socket]
ListenStream=/tmp/tako-dev-proxy.sock
SocketMode=0666

[Install]
WantedBy=sockets.target
"
    .to_string()
}

/// NixOS `configuration.nix` snippet for declarative setup.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn nixos_config_snippet() -> String {
    format!(
        r#"# Tako dev mode — portless HTTPS for *.tako.test
{{
  # Loopback alias + iptables redirects (restored at boot)
  systemd.services.tako-dev-redirect = {{
    description = "Tako dev port redirect (127.77.0.1)";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-pre.target" ];
    before = [ "network.target" ];
    serviceConfig = {{
      Type = "oneshot";
      RemainAfterExit = true;
    }};
    script = ''
      /sbin/ip addr add 127.77.0.1/8 dev lo 2>/dev/null || true
      iptables -t nat -C OUTPUT -d 127.77.0.1 -p tcp --dport 443 -j REDIRECT --to-port {https} 2>/dev/null || \
        iptables -t nat -A OUTPUT -d 127.77.0.1 -p tcp --dport 443 -j REDIRECT --to-port {https}
      iptables -t nat -C OUTPUT -d 127.77.0.1 -p tcp --dport 80 -j REDIRECT --to-port {http} 2>/dev/null || \
        iptables -t nat -A OUTPUT -d 127.77.0.1 -p tcp --dport 80 -j REDIRECT --to-port {http}
      iptables -t nat -C OUTPUT -d 127.77.0.1 -p udp --dport 53 -j REDIRECT --to-port {dns} 2>/dev/null || \
        iptables -t nat -A OUTPUT -d 127.77.0.1 -p udp --dport 53 -j REDIRECT --to-port {dns}
    '';
  }};

  # Split DNS: route *.tako.test to the local DNS server
  services.resolved.enable = true;
  networking.resolvconf.extraConfig = "name_servers=127.77.0.1";
  environment.etc."systemd/resolved.conf.d/tako-dev.conf".text = ''
    [Resolve]
    DNS=127.77.0.1
    Domains=~tako.test ~test
  '';
}}"#,
        https = 47831,
        http = 47830,
        dns = 53535,
    )
}

// ─── Sudo action line (for explain_pending_sudo_setup) ──────────────────────

#[cfg(any(target_os = "linux", test))]
pub(crate) fn install_action_line() -> &'static str {
    "Set up iptables port redirect for 127.77.0.1 (443/80/53)"
}

// ─── Imperative setup (Linux runtime only) ──────────────────────────────────

#[cfg(target_os = "linux")]
fn is_nixos() -> bool {
    std::path::Path::new("/etc/NIXOS").exists()
}

#[cfg(target_os = "linux")]
fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

#[cfg(target_os = "linux")]
fn sudo_run(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("sudo").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("sudo {} failed", args.join(" ")).into())
    }
}

#[cfg(target_os = "linux")]
fn write_system_file_with_sudo(
    path: &str,
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "tako-linux-setup-{}-{}",
        std::process::id(),
        unique
    ));

    // Create exclusively (O_CREAT|O_EXCL) with restrictive permissions (0600)
    // to prevent symlink attacks and limit read access on multi-user systems.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(content.as_bytes())?;
    drop(file);

    let tmp_str = tmp.to_string_lossy().to_string();

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(path).parent() {
        let parent_str = parent.to_string_lossy().to_string();
        let _ = sudo_run(&["mkdir", "-p", &parent_str]);
    }

    let result = sudo_run(&["install", "-m", "644", &tmp_str, path]);
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(target_os = "linux")]
pub(crate) fn status() -> LinuxSetupStatus {
    let ip_output = run_command("ip", &["addr", "show", "dev", "lo"]).unwrap_or_default();
    let iptables_output =
        run_command("iptables", &["-t", "nat", "-L", "OUTPUT", "-n"]).unwrap_or_default();

    let loopback_alias = parse_loopback_alias(&ip_output, DEV_LOOPBACK_ADDR);
    let redirect_443 =
        parse_iptables_redirect(&iptables_output, DEV_LOOPBACK_ADDR, 443, DEV_HTTPS_PORT);
    let redirect_80 =
        parse_iptables_redirect(&iptables_output, DEV_LOOPBACK_ADDR, 80, DEV_HTTP_PORT);
    let redirect_dns =
        parse_iptables_redirect(&iptables_output, DEV_LOOPBACK_ADDR, 53, LOCAL_DNS_PORT);

    let dns_configured = std::path::Path::new(RESOLVED_DROP_IN_FILE).exists();
    let service_installed = std::path::Path::new(SYSTEMD_SERVICE_PATH).exists();

    LinuxSetupStatus {
        loopback_alias,
        redirect_443,
        redirect_80,
        redirect_dns,
        dns_configured,
        service_installed,
        is_nixos: is_nixos(),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn explain_pending_sudo_setup() -> Result<(), Box<dyn std::error::Error>> {
    if !crate::output::is_interactive() {
        return Ok(());
    }

    let ca_action = super::tls::pending_sudo_action()?;
    let redirect_action = pending_sudo_action()?;

    let mut items = Vec::new();
    if let Some(action) = ca_action {
        items.push(action.to_string());
    }
    if let Some(action) = redirect_action {
        items.push(action.to_string());
    }
    if items.is_empty() {
        return Ok(());
    }

    crate::output::warning("sudo access required");
    if crate::output::is_pretty() {
        eprintln!("Tako needs this to set up your development environment:");
        for item in &items {
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

#[cfg(target_os = "linux")]
pub(crate) fn pending_sudo_action() -> Result<Option<&'static str>, Box<dyn std::error::Error>> {
    let s = status();
    let plan = repair_plan(&s);
    match plan {
        LinuxRepairPlan::None => Ok(None),
        LinuxRepairPlan::NixOsManual => Ok(None), // handled separately
        _ => Ok(Some(install_action_line())),
    }
}

#[cfg(target_os = "linux")]
fn setup_loopback_alias() -> Result<(), Box<dyn std::error::Error>> {
    // ip addr add is idempotent when the address already exists (returns error
    // EEXIST which we ignore).
    let _ = sudo_run(&["ip", "addr", "add", "127.77.0.1/8", "dev", "lo"]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup_iptables_redirects() -> Result<(), Box<dyn std::error::Error>> {
    let rules: &[(&str, u16, u16)] = &[
        ("tcp", 443, DEV_HTTPS_PORT),
        ("tcp", 80, DEV_HTTP_PORT),
        ("udp", 53, LOCAL_DNS_PORT),
    ];
    for (proto, dport, to_port) in rules {
        let dport_str = dport.to_string();
        let to_port_str = to_port.to_string();
        // Check first (-C), add only if missing (-A)
        let check = std::process::Command::new("sudo")
            .args([
                "iptables",
                "-t",
                "nat",
                "-C",
                "OUTPUT",
                "-d",
                DEV_LOOPBACK_ADDR,
                "-p",
                proto,
                "--dport",
                &dport_str,
                "-j",
                "REDIRECT",
                "--to-port",
                &to_port_str,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !check.is_ok_and(|s| s.success()) {
            sudo_run(&[
                "iptables",
                "-t",
                "nat",
                "-A",
                "OUTPUT",
                "-d",
                DEV_LOOPBACK_ADDR,
                "-p",
                proto,
                "--dport",
                &dport_str,
                "-j",
                "REDIRECT",
                "--to-port",
                &to_port_str,
            ])?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd_service() -> Result<(), Box<dyn std::error::Error>> {
    write_system_file_with_sudo(SYSTEMD_SERVICE_PATH, &systemd_service_contents())?;
    sudo_run(&["systemctl", "daemon-reload"])?;
    sudo_run(&["systemctl", "enable", SYSTEMD_SERVICE_NAME])?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_dev_proxy_service() -> Result<(), Box<dyn std::error::Error>> {
    let binary_path = locate_dev_proxy_binary()?;
    let binary_str = binary_path.to_string_lossy().to_string();
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());

    write_system_file_with_sudo(
        DEV_PROXY_SERVICE_PATH,
        &dev_proxy_service_contents(&binary_str, &user),
    )?;
    write_system_file_with_sudo(DEV_PROXY_SOCKET_PATH, &dev_proxy_socket_contents())?;
    sudo_run(&["systemctl", "daemon-reload"])?;
    sudo_run(&["systemctl", "enable", "--now", DEV_PROXY_SOCKET_NAME])?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn locate_dev_proxy_binary() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let current_exe = std::env::current_exe()?;
    if let Some(parent) = current_exe.parent() {
        let sibling = parent.join("tako-dev-proxy");
        if sibling.exists() {
            return Ok(sibling);
        }
    }
    if let Some(root) = crate::paths::repo_root_from_exe(&current_exe) {
        let candidates = [
            root.join("target").join("release").join("tako-dev-proxy"),
            root.join("target").join("debug").join("tako-dev-proxy"),
        ];
        for candidate in &candidates {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }
    }
    Err("could not locate tako-dev-proxy binary".into())
}

#[cfg(target_os = "linux")]
fn setup_dns_resolved() -> Result<(), Box<dyn std::error::Error>> {
    write_system_file_with_sudo(RESOLVED_DROP_IN_FILE, &resolved_drop_in_contents())?;
    // Restart resolved to pick up the new drop-in.
    let _ = sudo_run(&["systemctl", "restart", "systemd-resolved"]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn has_systemd_resolved() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "systemd-resolved"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
fn explain_nixos_setup() -> Result<(), Box<dyn std::error::Error>> {
    crate::output::warning_full(
        "NixOS detected — imperative iptables setup would be lost on rebuild.",
    );
    eprintln!();
    eprintln!("Add the following to your configuration.nix:\n");
    eprintln!("{}", nixos_config_snippet());
    eprintln!();
    crate::output::info("After applying, run `tako dev` again.");
    Err("NixOS requires declarative configuration — see snippet above".into())
}

/// Main entry point. Checks current status and runs setup if needed.
#[cfg(target_os = "linux")]
pub(crate) fn ensure_installed() -> Result<(), Box<dyn std::error::Error>> {
    let s = status();
    let plan = repair_plan(&s);

    match plan {
        LinuxRepairPlan::None => return Ok(()),
        LinuxRepairPlan::NixOsManual => return explain_nixos_setup(),
        _ => {}
    }

    if !crate::output::is_interactive() && !crate::output::is_root() {
        return Err(
            "Linux port redirect is not configured; run `tako dev` interactively once to set it up"
                .into(),
        );
    }

    match plan {
        LinuxRepairPlan::SetupAll => {
            crate::output::info("Setting up loopback alias and port redirects (sudo)…");
            setup_loopback_alias()?;
            setup_iptables_redirects()?;

            if has_systemd_resolved() {
                setup_dns_resolved()?;
            } else {
                crate::output::warning(
                    "systemd-resolved not found. DNS for *.tako.test may not work automatically.",
                );
                crate::output::muted(
                    "You may need to add '127.77.0.1' as a DNS server manually or use /etc/hosts.",
                );
            }

            // Persist via systemd if available
            if run_command("systemctl", &["--version"]).is_some() {
                install_systemd_service()?;
                if let Err(e) = install_dev_proxy_service() {
                    crate::output::warning(&format!(
                        "Dev proxy service not installed (LAN mode unavailable): {e}"
                    ));
                }
                crate::output::success(
                    "Port redirect installed and persisted (tako-dev-redirect.service).",
                );
            } else {
                crate::output::success("Port redirect installed.");
                crate::output::muted(
                    "No systemd found — redirect rules will need to be re-applied after reboot.",
                );
            }
        }
        LinuxRepairPlan::RepairRedirects => {
            crate::output::info("Restoring port redirect rules (sudo)…");
            setup_loopback_alias()?;
            setup_iptables_redirects()?;
            crate::output::success("Port redirect rules restored.");
        }
        LinuxRepairPlan::None | LinuxRepairPlan::NixOsManual => unreachable!(),
    }

    // Verify
    let s = status();
    if !s.loopback_alias || !s.redirect_443 || !s.redirect_80 || !s.redirect_dns {
        return Err(
            "Port redirect setup verification failed. Check iptables and try again.".into(),
        );
    }

    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
