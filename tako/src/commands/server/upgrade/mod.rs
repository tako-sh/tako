mod compatibility;
mod readiness;
mod task_tree;

use crate::output;
use crate::ssh::SshClient;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::Instrument;

#[cfg(test)]
use compatibility::remote_protocol_compatibility_command;
use compatibility::{
    check_remote_protocol_compatibility, remote_cleanup_upgrade_reload_command,
    remote_prepare_upgrade_reload_command,
};
pub(super) use readiness::wait_for_primary_ready;
use task_tree::{Step, UpgradeTaskTreeController, should_use_upgrade_task_tree};

pub(super) const UPGRADE_SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SERVER_BINARY_PATH: &str = "/usr/local/bin/tako-server";

const REPO_OWNER: &str = "tako-sh";
const REPO_NAME: &str = "tako";
const LATEST_TAG: &str = "latest";
const SERVER_CHECKSUM_MANIFEST_ASSET: &str = "tako-server-sha256s.txt";
const SERVER_CHECKSUM_SIGNATURE_ASSET: &str = "tako-server-sha256s.txt.sig";
const SERVER_RELEASE_SIGNING_PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBojANBgkqhkiG9w0BAQEFAAOCAY8AMIIBigKCAYEAuSti08sNCTG7S1oGDSB3\n\
vThbzAfQQzGq+wQjVkjN1VEPFk21eWqYMEAN2jU3FhTZDrsfl5iEMv1NsE6bimjd\n\
LN3UtdvqnxdF08wlCmbu4tO7thJE4CNY1uY4qHjI1aqBSozJ92x8vkel1DZKUxG0\n\
aK1YdrP0bqbuikK8f5wFgMGPO0sfSH5FKH7N0SseEoMZt1bGh7bL8G2EEDo91uEb\n\
w0OcbZGhZ/G3Kbv9dBQAS16eEgH/d0ssruPjdsQbFD+hnywgiqC8lOro1cmr1bBN\n\
d+Q7l60r6e3Y4kmH3OCqRzmIcKnv+6Piot9YHqMxptd6BuiE6x72w9j2loOLnB5j\n\
ytknLq3YykchWrbwLYqVspjN6FcqPZgI6bIEhsaFLRD6tjTqYBmEHcpLk//26p7a\n\
1/r22DyKdHO3/GS0L2sYVKkD/7R9N5QfnRd3erbx7je0pzDDe/x31h4X7vGgjCTy\n\
xm4tDiIHBg92bd3+ag9qnvulBH1uEb2i+grxFYefUkKpAgMBAAE=\n\
-----END PUBLIC KEY-----\n";

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedReleaseAsset {
    download_url: String,
    expected_sha256: String,
}

fn build_upgrade_owner(server_name: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let raw = format!("upgrade-{server_name}-{now}-{}", std::process::id());
    raw.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

fn server_binary_archive_name(target: &crate::config::ServerTarget) -> String {
    format!("tako-server-linux-{}-{}.tar.zst", target.arch, target.libc)
}

fn default_download_base() -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{LATEST_TAG}")
}

fn parse_sha256_manifest_value(manifest: &str, filename: &str) -> Result<String, String> {
    for line in manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        let normalized_name = name.trim_start_matches('*').trim_start_matches("./");
        if normalized_name == filename {
            if hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Ok(hash.to_ascii_lowercase());
            }
            return Err(format!(
                "checksum manifest entry for '{filename}' contains an invalid SHA-256 value"
            ));
        }
    }
    Err(format!("checksum manifest missing entry for '{filename}'"))
}

fn verify_signed_server_checksum_manifest(manifest: &[u8], signature: &[u8]) -> Result<(), String> {
    let key =
        openssl::pkey::PKey::public_key_from_pem(SERVER_RELEASE_SIGNING_PUBLIC_KEY_PEM.as_bytes())
            .map_err(|e| format!("failed to load embedded server release public key: {e}"))?;
    let mut verifier =
        openssl::sign::Verifier::new(openssl::hash::MessageDigest::sha256(), &key)
            .map_err(|e| format!("failed to initialize server release signature verifier: {e}"))?;
    verifier
        .update(manifest)
        .map_err(|e| format!("failed to hash server release checksum manifest: {e}"))?;
    let verified = verifier
        .verify(signature)
        .map_err(|e| format!("failed to verify server checksum signature: {e}"))?;
    if verified {
        Ok(())
    } else {
        Err("server checksum signature verification failed".to_string())
    }
}

async fn fetch_release_bytes(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::new();
    let response =
        crate::github::apply_auth_for_url(client.get(url).header("User-Agent", "tako-cli"), url)
            .send()
            .await
            .map_err(|e| format!("request failed for {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "download failed for {url}: HTTP {}",
            response.status()
        ));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| format!("failed to read response body from {url}: {e}"))
}

async fn resolve_verified_server_release_asset(
    target: &crate::config::ServerTarget,
) -> Result<VerifiedReleaseAsset, String> {
    let custom_base = std::env::var("TAKO_DOWNLOAD_BASE_URL").ok();
    if custom_base
        .as_deref()
        .is_some_and(|base| !base.trim().is_empty())
    {
        return Err(
            "Server upgrades use official releases. Install custom builds as administrator."
                .to_string(),
        );
    }
    let base = default_download_base();
    let archive_name = server_binary_archive_name(target);
    let download_url = format!("{base}/{archive_name}");
    let manifest_url = format!("{base}/{SERVER_CHECKSUM_MANIFEST_ASSET}");
    let manifest = fetch_release_bytes(&manifest_url).await?;
    let signature_url = format!("{base}/{SERVER_CHECKSUM_SIGNATURE_ASSET}");
    let signature = fetch_release_bytes(&signature_url).await?;
    verify_signed_server_checksum_manifest(&manifest, &signature)?;
    let manifest_text = std::str::from_utf8(&manifest)
        .map_err(|e| format!("signed checksum manifest was not valid UTF-8: {e}"))?;
    let expected_sha256 = parse_sha256_manifest_value(manifest_text, &archive_name)?;
    Ok(VerifiedReleaseAsset {
        download_url,
        expected_sha256,
    })
}

fn remote_binary_replace_command(url: &str, expected_sha256: &str) -> String {
    let archive_name = crate::shell::shell_single_quote(url.rsplit('/').next().unwrap_or(""));
    let expected_sha256 = crate::shell::shell_single_quote(expected_sha256);
    SshClient::run_as_root(&format!(
        "/usr/local/bin/tako-server-install-refresh {archive_name} {expected_sha256}"
    ))
}

fn remote_restore_previous_binary_command() -> String {
    SshClient::run_as_root("/usr/local/bin/tako-server-service rollback")
}

fn remote_cleanup_previous_binary_command() -> String {
    SshClient::run_as_root("/usr/local/bin/tako-server-service cleanup-upgrade")
}

async fn refresh_upgrade_companions(ssh: &SshClient) -> Result<(), String> {
    let command = SshClient::run_as_root("/usr/local/bin/tako-server-service upgrade-companions");
    let output = ssh
        .exec(&command)
        .await
        .map_err(|error| error.to_string())?;
    if !output.success() {
        return Err(format!(
            "Failed to restart upgrade companions: {}",
            output.combined().trim()
        ));
    }
    Ok(())
}

pub(super) async fn upgrade_servers(
    name: Option<&str>,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let servers = crate::commands::helpers::load_management_servers(None, None).await?;
    if servers.is_empty() {
        output::error("No servers configured.");
        output::hint(&format!(
            "Run {} to add a server.",
            output::strong("tako network server add")
        ));
        return Ok(());
    }

    let names: Vec<String> = if let Some(name) = name {
        if !servers.contains(name) {
            return Err(format!("Server '{}' not found.", name).into());
        }
        vec![name.to_string()]
    } else {
        let mut names: Vec<String> = servers.names().iter().map(|s| s.to_string()).collect();
        names.sort_unstable();
        names
    };

    // Resolve the real latest version from GitHub. The CLI's own version is
    // only authoritative on release builds; dev builds report bare "0.0.0".
    let latest_version = crate::commands::upgrade::version::fetch_latest_version()
        .await
        .map_err(|e| format!("Failed to resolve latest version: {e}"))?;
    tracing::info!("Upgrading to {latest_version}");
    if output::is_pretty() {
        output::line(&format!("Latest version: {latest_version}"));
        eprintln!();
    }

    let task_tree = should_use_upgrade_task_tree().then(|| UpgradeTaskTreeController::new(&names));

    let mut handles = Vec::new();
    for server_name in &names {
        let server = servers
            .get(server_name)
            .ok_or_else(|| format!("Server '{}' not found.", server_name))?
            .clone();
        let name = server_name.clone();
        let latest = latest_version.clone();
        let tree = task_tree.clone();
        let span = output::scope(&name);
        handles.push(tokio::spawn(
            async move {
                let result =
                    upgrade_one_server(&name, &server, &latest, force, tree.as_ref()).await;
                (name, result)
            }
            .instrument(span),
        ));
    }

    let mut results: Vec<(String, Result<(), String>)> = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(pair) => results.push(pair),
            Err(e) => return Err(format!("Upgrade task panicked: {e}").into()),
        }
    }

    let total = results.len();
    let failures = results.iter().filter(|(_, r)| r.is_err()).count();

    if failures > 0 {
        let succeeded = total - failures;
        if let Some(tree) = &task_tree {
            tree.set_error_summary(format!("Upgraded {succeeded}/{total} servers"));
            tree.finalize();
        }
        if output::is_pretty() {
            return Err(output::silent_exit_error().into());
        }
        return Err(format!("Upgraded {succeeded}/{total} servers").into());
    }

    if let Some(tree) = &task_tree {
        tree.finalize();
    }
    Ok(())
}

async fn upgrade_one_server(
    name: &str,
    server: &crate::config::ServerEntry,
    latest_version: &str,
    force: bool,
    task_tree: Option<&UpgradeTaskTreeController>,
) -> Result<(), String> {
    if let Some(tree) = task_tree {
        tree.mark_server_running(name);
        tree.mark_step_running(name, Step::VersionCheck);
    }

    let mut ssh = match SshClient::connect_to(server).await {
        Ok(ssh) => ssh,
        Err(e) => {
            let msg = e.to_string();
            if let Some(tree) = task_tree {
                tree.fail_step(name, Step::VersionCheck, &msg);
                tree.fail_server(name);
            }
            return Err(msg);
        }
    };

    let current_version = {
        let _t = output::timed(&format!("[{name}] Check current version"));
        ssh.tako_version().await.ok().flatten()
    };
    let current_label = current_version.clone().unwrap_or_else(|| "unknown".into());

    if let Some(tree) = task_tree {
        tree.rename_step(
            name,
            Step::VersionCheck,
            format!("Current version: {current_label}"),
        );
        tree.succeed_step(name, Step::VersionCheck, None);
    }

    if current_version.as_deref() == Some(latest_version) {
        tracing::debug!("[{name}] already on latest ({current_label})");
        if let Some(tree) = task_tree {
            tree.rename_step(name, Step::Upgrade, "Already on latest");
            tree.succeed_step(name, Step::Upgrade, None);
            tree.succeed_server(name, None);
        }
        let _ = ssh.disconnect().await;
        return Ok(());
    }

    if let Some(tree) = task_tree {
        tree.mark_step_running(name, Step::Upgrade);
    }

    let target = match super::wizard::detect_server_target(&ssh).await {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("Could not detect server target: {e}");
            if let Some(tree) = task_tree {
                tree.fail_step(name, Step::Upgrade, &msg);
                tree.fail_server(name);
            }
            let _ = ssh.disconnect().await;
            return Err(msg);
        }
    };

    let result =
        run_server_upgrade(name, &mut ssh, current_version.as_deref(), &target, force).await;
    let _ = ssh.disconnect().await;

    match result {
        Ok(version_after) => {
            let new_version = version_after.as_deref().unwrap_or("unknown").to_string();
            let new_label = if new_version == current_label {
                "Already on latest"
            } else {
                "Upgraded"
            };
            if let Some(tree) = task_tree {
                tree.rename_step(name, Step::Upgrade, new_label);
                tree.succeed_step(name, Step::Upgrade, None);
                tree.succeed_server(name, None);
            }
            Ok(())
        }
        Err(e) => {
            let clean_err = if let Some(pos) = e.find(" (owner:") {
                e[..pos].to_string()
            } else {
                e
            };
            if let Some(tree) = task_tree {
                tree.fail_step(name, Step::Upgrade, &clean_err);
                tree.fail_server(name);
            }
            Err(clean_err)
        }
    }
}

async fn run_server_upgrade(
    name: &str,
    ssh: &mut SshClient,
    running_version: Option<&str>,
    target: &crate::config::ServerTarget,
    force: bool,
) -> Result<Option<String>, String> {
    let owner = build_upgrade_owner(name);
    let mut upgrade_mode_entered = false;
    let mut binary_replaced = false;
    let mut reload_handoff_data_dir = None;
    let mut rollback_from_pid = None;

    let result: Result<Option<String>, String> = async {
        let status = ssh
            .tako_status()
            .await
            .map_err(|e| format!("Failed to query status: {e}"))?;
        if status != "active" {
            return Err(format!("tako-server not active (status: {status})"));
        }

        let verified_release = resolve_verified_server_release_asset(target)
            .await
            .map_err(|e| format!("Failed to verify release metadata: {e}"))?;

        let _t = output::timed("Enter upgrade mode");
        ssh.tako_enter_upgrading_allow_incompatible(&owner, force)
            .await
            .map_err(|e| match &e {
                crate::ssh::SshError::CommandFailed(m) => m.clone(),
                other => other.to_string(),
            })?;
        drop(_t);
        upgrade_mode_entered = true;

        let old_info = ssh
            .tako_server_info_allow_incompatible(force)
            .await
            .map_err(|e| format!("Failed to read runtime config: {e}"))?;
        let old_pid = old_info.pid;
        rollback_from_pid = Some(old_pid);

        let _t = output::timed("Download latest tako-server binary");
        let install_output = ssh
            .exec(&remote_binary_replace_command(
                &verified_release.download_url,
                &verified_release.expected_sha256,
            ))
            .await
            .map_err(|e| format!("Binary download failed: {e}"))?;
        drop(_t);
        if !install_output.success() {
            tracing::debug!("Binary replace failed: {}", install_output.stderr.trim());
            let combined = install_output.combined();
            let message =
                first_non_empty_line(combined.trim()).unwrap_or("binary download/install failed");
            return Err(message.to_string());
        }
        binary_replaced = true;

        let version_after_install = ssh.tako_version().await.ok().flatten();
        if version_after_install.as_deref() == running_version {
            tracing::debug!("Binary unchanged, skipping reload");
            // An earlier upgrade may have left these on an older binary.
            refresh_upgrade_companions(ssh).await?;
            ssh.tako_exit_upgrading_allow_incompatible(&owner, force)
                .await
                .map_err(|error| format!("Failed to exit upgrading mode: {error}"))?;
            upgrade_mode_entered = false;
            if let Err(error) = ssh.exec(&remote_cleanup_previous_binary_command()).await {
                tracing::warn!("Failed to remove previous tako-server binary: {error}");
            }
            return Ok(version_after_install);
        }

        check_remote_protocol_compatibility(ssh, &old_info.data_dir, force).await?;

        let handoff = ssh
            .exec(&remote_prepare_upgrade_reload_command(
                &old_info.data_dir,
                &owner,
            ))
            .await
            .map_err(|error| format!("Failed to prepare upgrade reload: {error}"))?;
        if !handoff.success() {
            return Err(format!(
                "Failed to prepare upgrade reload: {}",
                handoff.combined().trim()
            ));
        }
        reload_handoff_data_dir = Some(old_info.data_dir.clone());

        let _t = output::timed(&format!(
            "Reload server (pid: {old_pid}) + wait for new process"
        ));
        ssh.tako_reload()
            .await
            .map_err(|e| format!("Reload failed: {e}"))?;
        let info =
            wait_for_primary_ready(ssh, UPGRADE_SOCKET_WAIT_TIMEOUT, old_pid, name, force).await?;
        rollback_from_pid = Some(info.pid);
        tracing::debug!("New server process ready (pid: {})", info.pid);

        check_remote_protocol_compatibility(ssh, &info.data_dir, force).await?;

        refresh_upgrade_companions(ssh).await?;

        ssh.tako_exit_upgrading_allow_incompatible(&owner, force)
            .await
            .map_err(|error| format!("Failed to exit upgrading mode: {error}"))?;
        upgrade_mode_entered = false;

        let version = ssh.tako_version().await.ok().flatten();
        tracing::debug!("Upgraded (version: {version:?})");

        if let Err(e) = ssh.exec(&remote_cleanup_previous_binary_command()).await {
            tracing::warn!("Failed to remove previous tako-server binary: {e}");
        }
        Ok(version)
    }
    .await;

    let mut rollback_error = None;
    if result.is_err() && binary_replaced {
        match ssh.exec(&remote_restore_previous_binary_command()).await {
            Ok(output) if output.success() => {
                tracing::warn!("Restored previous tako-server binary after failed upgrade");
                let reload_result = async {
                    ssh.tako_restart()
                        .await
                        .map_err(|error| format!("restart previous binary: {error}"))?;
                    refresh_upgrade_companions(ssh).await?;
                    if let Some(pid) = rollback_from_pid {
                        wait_for_primary_ready(ssh, UPGRADE_SOCKET_WAIT_TIMEOUT, pid, name, force)
                            .await
                            .map(|_| ())
                            .map_err(|error| format!("wait for previous binary: {error}"))?;
                    }
                    Ok::<(), String>(())
                }
                .await;
                match reload_result {
                    Ok(()) => upgrade_mode_entered = false,
                    Err(error) => {
                        tracing::warn!("Failed to restart previous tako-server binary: {error}");
                        rollback_error = Some(error);
                    }
                }
            }
            Ok(output) => {
                let error = format!("restore previous binary: {}", output.combined().trim());
                tracing::warn!("Failed to restore previous tako-server binary: {error}");
                rollback_error = Some(error);
            }
            Err(e) => {
                tracing::warn!("Failed to restore previous tako-server binary: {e}");
                rollback_error = Some(format!("restore previous binary: {e}"));
            }
        }
    }

    if result.is_err() && upgrade_mode_entered {
        tracing::debug!("Upgrade failed, attempting to release upgrade lock (owner: {owner})");
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            match ssh
                .tako_exit_upgrading_allow_incompatible(&owner, force)
                .await
            {
                Ok(()) => {
                    tracing::debug!("Upgrade lock released (attempt {attempt})");
                    break;
                }
                Err(e) => {
                    tracing::debug!(
                        "Failed to release upgrade lock, retrying (attempt {attempt}): {e}"
                    );
                }
            }
        }
    }

    if let Some(data_dir) = reload_handoff_data_dir {
        match ssh
            .exec(&remote_cleanup_upgrade_reload_command(&data_dir))
            .await
        {
            Ok(output) if output.success() => {}
            Ok(output) => tracing::warn!(
                "Failed to remove upgrade reload marker: {}",
                output.combined().trim()
            ),
            Err(error) => tracing::warn!("Failed to remove upgrade reload marker: {error}"),
        }
    }

    match (result, rollback_error) {
        (Err(error), Some(rollback_error)) => {
            Err(format!("{error}; rollback failed: {rollback_error}"))
        }
        (result, _) => result,
    }
}

#[cfg(test)]
mod tests;
