use std::path::Path;
use std::process::{Command, Stdio};

use crate::output;

pub(super) struct SystemTarget {
    pub(super) description: String,
    pub(super) commands: Vec<Vec<String>>,
}

/// Detect which system-level Tako artifacts exist on this machine.
pub(super) fn gather_system_targets() -> Vec<SystemTarget> {
    let mut targets = Vec::new();

    #[cfg(target_os = "macos")]
    {
        targets.extend(gather_macos_system_targets());
    }

    #[cfg(target_os = "linux")]
    {
        targets.extend(gather_linux_system_targets());
    }

    targets
}

#[cfg(target_os = "macos")]
pub(super) fn gather_macos_system_targets() -> Vec<SystemTarget> {
    use crate::commands::dev::prepare::macos::{
        DEV_PROXY_BINARY_PATH, DEV_PROXY_BOOTSTRAP_LABEL, DEV_PROXY_BOOTSTRAP_PLIST_PATH,
        DEV_PROXY_LABEL, DEV_PROXY_PLIST_PATH,
    };

    let mut targets = Vec::new();

    // Loopback proxy services and files
    if Path::new(DEV_PROXY_BOOTSTRAP_PLIST_PATH).exists()
        || Path::new(DEV_PROXY_PLIST_PATH).exists()
        || Path::new(DEV_PROXY_BINARY_PATH).exists()
    {
        targets.push(SystemTarget {
            description: "Dev proxy (LaunchDaemons, binary)".into(),
            commands: vec![
                vec![
                    "launchctl".into(),
                    "bootout".into(),
                    format!("system/{DEV_PROXY_LABEL}"),
                ],
                vec![
                    "launchctl".into(),
                    "bootout".into(),
                    format!("system/{DEV_PROXY_BOOTSTRAP_LABEL}"),
                ],
                vec!["rm".into(), "-f".into(), DEV_PROXY_PLIST_PATH.into()],
                vec![
                    "rm".into(),
                    "-f".into(),
                    DEV_PROXY_BOOTSTRAP_PLIST_PATH.into(),
                ],
                vec!["rm".into(), "-f".into(), DEV_PROXY_BINARY_PATH.into()],
                vec![
                    "rm".into(),
                    "-rf".into(),
                    "/Library/Application Support/Tako".into(),
                ],
            ],
        });
    }

    // DNS resolver
    if Path::new(crate::commands::dev::TAKO_RESOLVER_FILE).exists() {
        targets.push(SystemTarget {
            description: format!(
                "DNS resolver ({})",
                crate::commands::dev::TAKO_RESOLVER_FILE
            ),
            commands: vec![vec![
                "rm".into(),
                "-f".into(),
                crate::commands::dev::TAKO_RESOLVER_FILE.into(),
            ]],
        });
    }

    // CA certificate(s) in system keychain are handled separately because
    // `delete-certificate -c` fails when multiple certs share the same CN.

    // Loopback alias
    if loopback_alias_exists_macos() {
        targets.push(SystemTarget {
            description: "Loopback alias 127.77.0.1".into(),
            commands: vec![vec![
                "ifconfig".into(),
                "lo0".into(),
                "-alias".into(),
                "127.77.0.1".into(),
            ]],
        });
    }

    targets
}

/// Check whether any Tako CA certificates exist in the system keychain (macOS)
/// or trust store (Linux).
/// Common names of all Tako dev CA certs we've ever shipped. Include
/// legacy names so `tako uninstall` can clean up machines that still have
/// an older cert pinned from a previous Tako version.
#[cfg(target_os = "macos")]
const TAKO_CA_COMMON_NAMES: &[&str] = &[
    "Tako Development CA",
    "Tako Development",
    "Tako Local Development CA",
];

#[cfg(target_os = "macos")]
pub(super) fn has_ca_certs_in_keychain() -> bool {
    TAKO_CA_COMMON_NAMES.iter().any(|cn| {
        Command::new("security")
            .args([
                "find-certificate",
                "-c",
                cn,
                "/Library/Keychains/System.keychain",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(target_os = "linux")]
pub(super) fn has_ca_certs_in_keychain() -> bool {
    Path::new("/usr/local/share/ca-certificates/tako-ca.crt").exists()
        || Path::new("/etc/pki/ca-trust/source/anchors/tako-ca.crt").exists()
}

/// Remove all Tako CA certificates from the macOS System keychain by SHA-1 hash.
/// `delete-certificate -c` fails when multiple certs share the same CN, so we
/// find each cert's hash individually and delete by `-Z <hash>` in a loop.
#[cfg(target_os = "macos")]
pub(super) fn remove_ca_certs_from_keychain() {
    let mut removed = 0u32;
    loop {
        // Find the SHA-1 hash of the first matching certificate under any
        // of our known CA names. `security find-certificate -c` is an
        // exact-match on common name, so we have to try each one.
        let hash = TAKO_CA_COMMON_NAMES.iter().find_map(|cn| {
            let output = Command::new("security")
                .args([
                    "find-certificate",
                    "-c",
                    cn,
                    "-Z",
                    "/Library/Keychains/System.keychain",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    line.strip_prefix("SHA-1 hash:")
                        .or_else(|| line.strip_prefix("      SHA-1 hash:"))
                        .map(|h| h.trim().to_string())
                })
        });

        let Some(hash) = hash else {
            break;
        };

        let del = Command::new("sudo")
            .args([
                "security",
                "delete-certificate",
                "-Z",
                &hash,
                "/Library/Keychains/System.keychain",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match del {
            Ok(s) if s.success() => {
                removed += 1;
            }
            _ => {
                output::warning("Could not fully remove: CA certificate(s) in system keychain");
                return;
            }
        }
    }

    if removed > 0 {
        output::success(&format!(
            "Removed {} CA certificate{} from system keychain",
            removed,
            if removed == 1 { "" } else { "s" }
        ));
    }
}

/// On Linux the CA cert is removed as a regular SystemTarget (file delete + update-ca-certificates).
#[cfg(target_os = "linux")]
pub(super) fn remove_ca_certs_from_keychain() {
    // Handled by SystemTarget commands in gather_linux_system_targets.
}

#[cfg(target_os = "macos")]
fn loopback_alias_exists_macos() -> bool {
    Command::new("ifconfig")
        .arg("lo0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            text.contains("127.77.0.1")
        })
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub(super) fn gather_linux_system_targets() -> Vec<SystemTarget> {
    let mut targets = Vec::new();

    // systemd service
    if Path::new("/etc/systemd/system/tako-dev-redirect.service").exists() {
        targets.push(SystemTarget {
            description: "systemd service (tako-dev-redirect)".into(),
            commands: vec![
                vec![
                    "systemctl".into(),
                    "disable".into(),
                    "--now".into(),
                    "tako-dev-redirect.service".into(),
                ],
                vec![
                    "rm".into(),
                    "-f".into(),
                    "/etc/systemd/system/tako-dev-redirect.service".into(),
                ],
                vec!["systemctl".into(), "daemon-reload".into()],
            ],
        });
    }

    // Dev proxy service (LAN mode)
    if Path::new("/etc/systemd/system/tako-dev-proxy.service").exists()
        || Path::new("/etc/systemd/system/tako-dev-proxy.socket").exists()
    {
        targets.push(SystemTarget {
            description: "Dev proxy service (tako-dev-proxy)".into(),
            commands: vec![
                vec![
                    "systemctl".into(),
                    "disable".into(),
                    "--now".into(),
                    "tako-dev-proxy.socket".into(),
                ],
                vec![
                    "systemctl".into(),
                    "disable".into(),
                    "--now".into(),
                    "tako-dev-proxy.service".into(),
                ],
                vec![
                    "rm".into(),
                    "-f".into(),
                    "/etc/systemd/system/tako-dev-proxy.service".into(),
                ],
                vec![
                    "rm".into(),
                    "-f".into(),
                    "/etc/systemd/system/tako-dev-proxy.socket".into(),
                ],
                vec!["systemctl".into(), "daemon-reload".into()],
            ],
        });
    }

    // systemd-resolved drop-in
    if Path::new("/etc/systemd/resolved.conf.d/tako-dev.conf").exists() {
        targets.push(SystemTarget {
            description: "systemd-resolved config (tako-dev.conf)".into(),
            commands: vec![
                vec![
                    "rm".into(),
                    "-f".into(),
                    "/etc/systemd/resolved.conf.d/tako-dev.conf".into(),
                ],
                vec![
                    "systemctl".into(),
                    "restart".into(),
                    "systemd-resolved".into(),
                ],
            ],
        });
    }

    // CA certificate (Debian/Ubuntu)
    if Path::new("/usr/local/share/ca-certificates/tako-ca.crt").exists() {
        targets.push(SystemTarget {
            description: "CA certificate (Debian/Ubuntu trust store)".into(),
            commands: vec![
                vec![
                    "rm".into(),
                    "-f".into(),
                    "/usr/local/share/ca-certificates/tako-ca.crt".into(),
                ],
                vec!["update-ca-certificates".into()],
            ],
        });
    }

    // CA certificate (Fedora/RHEL/SUSE)
    if Path::new("/etc/pki/ca-trust/source/anchors/tako-ca.crt").exists() {
        targets.push(SystemTarget {
            description: "CA certificate (Fedora/RHEL trust store)".into(),
            commands: vec![
                vec![
                    "rm".into(),
                    "-f".into(),
                    "/etc/pki/ca-trust/source/anchors/tako-ca.crt".into(),
                ],
                vec!["update-ca-trust".into()],
            ],
        });
    }

    // iptables rules and loopback alias (ephemeral, but clean up if present)
    if loopback_alias_exists_linux() {
        targets.push(SystemTarget {
            description: "Loopback alias 127.77.0.1 and iptables rules".into(),
            commands: vec![
                vec![
                    "iptables".into(),
                    "-t".into(),
                    "nat".into(),
                    "-D".into(),
                    "OUTPUT".into(),
                    "-d".into(),
                    "127.77.0.1".into(),
                    "-p".into(),
                    "tcp".into(),
                    "--dport".into(),
                    "443".into(),
                    "-j".into(),
                    "REDIRECT".into(),
                    "--to-port".into(),
                    "47831".into(),
                ],
                vec![
                    "iptables".into(),
                    "-t".into(),
                    "nat".into(),
                    "-D".into(),
                    "OUTPUT".into(),
                    "-d".into(),
                    "127.77.0.1".into(),
                    "-p".into(),
                    "tcp".into(),
                    "--dport".into(),
                    "80".into(),
                    "-j".into(),
                    "REDIRECT".into(),
                    "--to-port".into(),
                    "47830".into(),
                ],
                vec![
                    "iptables".into(),
                    "-t".into(),
                    "nat".into(),
                    "-D".into(),
                    "OUTPUT".into(),
                    "-d".into(),
                    "127.77.0.1".into(),
                    "-p".into(),
                    "udp".into(),
                    "--dport".into(),
                    "53".into(),
                    "-j".into(),
                    "REDIRECT".into(),
                    "--to-port".into(),
                    "53535".into(),
                ],
                vec![
                    "ip".into(),
                    "addr".into(),
                    "del".into(),
                    "127.77.0.1/8".into(),
                    "dev".into(),
                    "lo".into(),
                ],
            ],
        });
    }

    targets
}

#[cfg(target_os = "linux")]
fn loopback_alias_exists_linux() -> bool {
    Command::new("ip")
        .args(["addr", "show", "dev", "lo"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            text.contains("inet 127.77.0.1/")
        })
        .unwrap_or(false)
}

/// Run each system target's commands with sudo, best-effort.
/// Sudo credential cache should already be warm from a prior `sudo -v` call.
pub(super) fn remove_system_targets(targets: &[SystemTarget]) {
    for target in targets {
        let mut any_failed = false;
        for cmd_args in &target.commands {
            let result = Command::new("sudo")
                .args(cmd_args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match result {
                Err(e) => {
                    tracing::debug!("sudo {:?} spawn failed: {e}", cmd_args);
                    any_failed = true;
                }
                Ok(s) if !s.success() => {
                    tracing::debug!("sudo {:?} exited {}", cmd_args, s);
                    any_failed = true;
                }
                Ok(_) => {}
            }
        }
        if any_failed {
            output::warning(&format!("Could not fully remove: {}", target.description));
        } else {
            output::success(&format!("Removed {}", target.description));
        }
    }
}
