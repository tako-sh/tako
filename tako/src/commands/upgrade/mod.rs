mod install;
pub(crate) mod version;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::output;

use install::{detect_platform, download_and_install, resolve_install_dir};
use version::{UpdateCheck, check_for_updates, current_version, tarball_url};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliUpgradeMethod {
    Installer,
    Homebrew,
}

#[derive(Debug, Clone)]
struct CliUpgradeDetectionContext {
    current_exe: PathBuf,
    has_brew: bool,
    brew_formula_installed: bool,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let ver = current_version();
    output::info(&format!("Current version: {}", output::strong(&ver)));
    tracing::info!("Current version: {ver}");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_upgrade())
}

async fn run_upgrade() -> Result<(), Box<dyn std::error::Error>> {
    match detect_cli_upgrade_method_runtime() {
        CliUpgradeMethod::Installer => run_installer_upgrade().await,
        CliUpgradeMethod::Homebrew => run_brew_upgrade().await,
    }
}

async fn run_installer_upgrade() -> Result<(), Box<dyn std::error::Error>> {
    let (os, arch) = detect_platform()?;
    let install_dir = resolve_install_dir();
    let url = tarball_url(os, arch);

    match output::with_spinner_async_simple("Upgrading", check_and_install(&url, &install_dir))
        .await?
    {
        UpdateCheck::AlreadyCurrent => output::info("Already on the latest version"),
        UpdateCheck::Available { version } => {
            output::info(&format!("Upgraded to {}", output::strong(&version)))
        }
    }
    Ok(())
}

async fn check_and_install(
    url: &str,
    install_dir: &Path,
) -> Result<UpdateCheck, Box<dyn std::error::Error>> {
    match check_for_updates().await? {
        UpdateCheck::AlreadyCurrent => Ok(UpdateCheck::AlreadyCurrent),
        UpdateCheck::Available { version } => {
            download_and_install(url, install_dir).await?;
            Ok(UpdateCheck::Available { version })
        }
    }
}

async fn run_brew_upgrade() -> Result<(), Box<dyn std::error::Error>> {
    output::with_spinner_async_simple("Upgrading via Homebrew", async {
        run_local_upgrade_command("brew", &["upgrade", "tako"])
    })
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    output::info("Upgraded via Homebrew");
    Ok(())
}

fn run_local_upgrade_command(binary: &str, args: &[&str]) -> Result<(), String> {
    let result = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to start {}: {}", binary, e))?;

    if result.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&result.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        Err(format!("{binary} exited with a non-zero status"))
    } else {
        Err(format!("{binary}: {detail}"))
    }
}

fn build_cli_upgrade_detection_context() -> CliUpgradeDetectionContext {
    let has_brew = command_exists("brew");

    CliUpgradeDetectionContext {
        current_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tako")),
        has_brew,
        brew_formula_installed: if has_brew {
            homebrew_formula_installed("tako")
        } else {
            false
        },
    }
}

fn detect_cli_upgrade_method_runtime() -> CliUpgradeMethod {
    let ctx = build_cli_upgrade_detection_context();
    detect_cli_upgrade_method(&ctx)
}

fn detect_cli_upgrade_method(ctx: &CliUpgradeDetectionContext) -> CliUpgradeMethod {
    if ctx.has_brew && is_homebrew_path(&ctx.current_exe) {
        return CliUpgradeMethod::Homebrew;
    }

    if ctx.has_brew && ctx.brew_formula_installed {
        return CliUpgradeMethod::Homebrew;
    }

    CliUpgradeMethod::Installer
}

fn is_homebrew_path(path: &Path) -> bool {
    let value = path.to_string_lossy();
    value.starts_with("/opt/homebrew/")
        || value.starts_with("/usr/local/Homebrew/")
        || value.starts_with("/home/linuxbrew/.linuxbrew/")
        || value.contains("/Cellar/tako/")
}

fn homebrew_formula_installed(formula: &str) -> bool {
    let output = Command::new("brew")
        .args(["list", "--formula", "--versions", formula])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => {
            !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        }
        _ => false,
    }
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarball_url_constructs_github_url() {
        let url = tarball_url("darwin", "aarch64");
        assert_eq!(
            url,
            "https://github.com/tako-sh/tako/releases/download/latest/tako-darwin-aarch64.tar.gz"
        );
    }

    #[test]
    fn detect_cli_upgrade_method_prefers_homebrew_path() {
        let ctx = CliUpgradeDetectionContext {
            current_exe: PathBuf::from("/opt/homebrew/bin/tako"),
            has_brew: true,
            brew_formula_installed: true,
        };
        assert_eq!(detect_cli_upgrade_method(&ctx), CliUpgradeMethod::Homebrew);
    }

    #[test]
    fn detect_cli_upgrade_method_uses_formula_presence_when_path_is_generic() {
        let ctx = CliUpgradeDetectionContext {
            current_exe: PathBuf::from("/usr/local/bin/tako"),
            has_brew: true,
            brew_formula_installed: true,
        };
        assert_eq!(detect_cli_upgrade_method(&ctx), CliUpgradeMethod::Homebrew);
    }

    #[test]
    fn detect_cli_upgrade_method_falls_back_to_installer() {
        let ctx = CliUpgradeDetectionContext {
            current_exe: PathBuf::from("/usr/local/bin/tako"),
            has_brew: false,
            brew_formula_installed: false,
        };
        assert_eq!(detect_cli_upgrade_method(&ctx), CliUpgradeMethod::Installer);
    }
}
