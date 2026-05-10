use crate::app_command::{command_from_manifest, load_release_manifest, safe_subdir};
use crate::instances::AppConfig;
use crate::socket::{AppState, BuildStatus, InstanceState, InstanceStatus};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Command as TokioCommand;

pub(crate) const TAKO_APP_DATA_DIR_ENV: &str = "TAKO_DATA_DIR";

#[derive(Debug, serde::Deserialize)]
struct ReleaseManifestMetadata {
    #[serde(default)]
    commit_message: Option<String>,
    #[serde(default)]
    git_dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppRuntimeDataPaths {
    pub root: PathBuf,
    pub app: PathBuf,
    pub tako: PathBuf,
}

pub(crate) fn collect_running_build_statuses(app: &crate::instances::App) -> Vec<BuildStatus> {
    let mut instances_by_build: HashMap<String, Vec<InstanceStatus>> = HashMap::new();
    for instance in app.get_instances() {
        instances_by_build
            .entry(instance.build_version().to_string())
            .or_default()
            .push(instance.status());
    }

    let mut builds: Vec<BuildStatus> = instances_by_build
        .into_iter()
        .map(|(version, instances)| BuildStatus {
            state: derive_build_state(&instances),
            version,
            instances,
        })
        .collect();

    let current_version = app.version();
    builds.sort_by(|a, b| a.version.cmp(&b.version));
    if let Some(index) = builds.iter().position(|b| b.version == current_version) {
        let current = builds.remove(index);
        builds.insert(0, current);
    }

    builds
}

pub(crate) fn current_release_version(app_root: &Path) -> Option<String> {
    let current_link = app_root.join("current");
    let target = std::fs::read_link(current_link).ok()?;
    target.file_name()?.to_str().map(|value| value.to_string())
}

pub(crate) fn app_root(data_dir: &Path, app_name: &str) -> PathBuf {
    data_dir.join("apps").join(app_name)
}

pub(crate) fn app_runtime_data_paths(data_dir: &Path, app_name: &str) -> AppRuntimeDataPaths {
    let root = app_root(data_dir, app_name).join("data");
    AppRuntimeDataPaths {
        app: root.join("app"),
        tako: root.join("tako"),
        root,
    }
}

pub(crate) fn ensure_app_runtime_data_dirs(
    data_dir: &Path,
    app_name: &str,
) -> Result<AppRuntimeDataPaths, String> {
    let paths = app_runtime_data_paths(data_dir, app_name);
    std::fs::create_dir_all(&paths.app)
        .map_err(|e| format!("create app data dir {}: {e}", paths.app.display()))?;
    std::fs::create_dir_all(&paths.tako)
        .map_err(|e| format!("create tako data dir {}: {e}", paths.tako.display()))?;
    prepare_app_runtime_data_permissions(&paths)
        .map_err(|e| format!("prepare app data permissions {}: {e}", paths.root.display()))?;
    Ok(paths)
}

#[cfg(unix)]
fn prepare_app_runtime_data_permissions(paths: &AppRuntimeDataPaths) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const DATA_ROOT_MODE: u32 = 0o710;
    const APP_DATA_DIR_MODE: u32 = 0o2770;
    const TAKO_DATA_DIR_MODE: u32 = 0o700;

    std::fs::set_permissions(&paths.root, std::fs::Permissions::from_mode(DATA_ROOT_MODE))?;
    std::fs::set_permissions(
        &paths.app,
        std::fs::Permissions::from_mode(APP_DATA_DIR_MODE),
    )?;
    std::fs::set_permissions(
        &paths.tako,
        std::fs::Permissions::from_mode(TAKO_DATA_DIR_MODE),
    )?;
    repair_app_data_tree_permissions(&paths.app)
}

#[cfg(unix)]
fn repair_app_data_tree_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }

    if file_type.is_dir() {
        set_permissions_if_allowed(path, 0o2770)?;
        for entry in std::fs::read_dir(path)? {
            repair_app_data_tree_permissions(&entry?.path())?;
        }
        return Ok(());
    }

    let current_mode = metadata.permissions().mode();
    set_permissions_if_allowed(path, current_mode | 0o060)
}

#[cfg(unix)]
fn set_permissions_if_allowed(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // App-created files may be owned by `tako-app`; the service user cannot
    // chmod those, and they are already writable by their owner.
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn prepare_app_runtime_data_permissions(_paths: &AppRuntimeDataPaths) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn inject_app_data_dir_env(
    env: &mut HashMap<String, String>,
    paths: &AppRuntimeDataPaths,
) {
    env.insert(
        TAKO_APP_DATA_DIR_ENV.to_string(),
        paths.app.display().to_string(),
    );
}

pub(crate) fn app_release_root(data_dir: &Path, app_name: &str, version: &str) -> PathBuf {
    app_root(data_dir, app_name).join("releases").join(version)
}

pub(crate) fn release_app_path(data_dir: &Path, config: &AppConfig) -> PathBuf {
    app_root(data_dir, &config.deployment_id())
        .join("releases")
        .join(&config.version)
}

pub(crate) fn apply_release_runtime_to_config(
    config: &mut AppConfig,
    release_path: PathBuf,
    runtime_bin: Option<&str>,
) -> Result<(), String> {
    let manifest = load_release_manifest(&release_path)?;
    config.command = command_from_manifest(&manifest, &release_path, runtime_bin)?;
    config.env_vars = manifest.env_vars;
    config.idle_timeout = Duration::from_secs(u64::from(manifest.idle_timeout));
    config.path = safe_subdir(&release_path, &manifest.app_dir)
        .map_err(|e| format!("Invalid app_dir in manifest: {e}"))?;
    Ok(())
}

pub(crate) fn read_release_manifest_metadata(path: &Path) -> (Option<String>, Option<bool>) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let Ok(parsed) = serde_json::from_str::<ReleaseManifestMetadata>(&raw) else {
        return (None, None);
    };
    (parsed.commit_message, parsed.git_dirty)
}

pub(crate) fn directory_modified_unix_secs(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let unix = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(unix.as_secs()).ok()
}

fn derive_build_state(instances: &[InstanceStatus]) -> AppState {
    if instances
        .iter()
        .any(|i| i.state == InstanceState::Healthy || i.state == InstanceState::Ready)
    {
        return AppState::Running;
    }
    if instances
        .iter()
        .any(|i| i.state == InstanceState::Starting || i.state == InstanceState::Draining)
    {
        return AppState::Deploying;
    }
    if instances
        .iter()
        .any(|i| i.state == InstanceState::Unhealthy)
    {
        return AppState::Error;
    }
    AppState::Stopped
}

#[cfg(unix)]
fn resolve_app_user_for_install() -> Option<(u32, u32)> {
    use std::ffi::CString;
    let name = CString::new("tako-app").ok()?;
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    Some(unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

#[cfg(unix)]
fn drop_privileges_if_root(cmd: &mut TokioCommand) {
    if unsafe { libc::geteuid() } == 0
        && let Some((uid, gid)) = resolve_app_user_for_install()
    {
        cmd.uid(uid);
        cmd.gid(gid);
    }
}

pub(crate) async fn prepare_release_runtime(
    release_dir: &Path,
    env: &HashMap<String, String>,
    data_dir: &Path,
) -> Result<Option<String>, String> {
    let manifest = load_release_manifest(release_dir)?;
    let runtime = &manifest.runtime;
    if runtime.trim().is_empty() {
        return Err(format!(
            "deploy manifest {} has empty runtime field",
            release_dir.join("app.json").display()
        ));
    }

    if manifest.runtime_version.is_none() {
        tracing::warn!(runtime = %runtime, "Could not detect runtime version; using latest. Pin a version with runtime_version in tako.toml");
    }
    let runtime_bin = crate::version_manager::install_and_resolve(
        runtime,
        manifest.runtime_version.as_deref(),
        data_dir,
    )
    .await;

    let mut install_env = env.clone();
    let mut path_dirs: Vec<String> = Vec::new();
    if let Some(ref bin) = runtime_bin
        && let Some(bin_dir) = Path::new(bin).parent()
    {
        path_dirs.push(bin_dir.display().to_string());
    }

    if let Some(ref pm) = manifest.package_manager
        && pm != runtime
    {
        let pm_bin = crate::version_manager::install_and_resolve(
            pm,
            manifest.package_manager_version.as_deref(),
            data_dir,
        )
        .await;
        if let Some(ref bin) = pm_bin
            && let Some(bin_dir) = Path::new(bin).parent()
        {
            path_dirs.push(bin_dir.display().to_string());
        }
    }

    if !path_dirs.is_empty() {
        let current_path =
            std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
        path_dirs.push(current_path);
        install_env.insert("PATH".to_string(), path_dirs.join(":"));
    }

    let app_dir = safe_subdir(release_dir, &manifest.app_dir)
        .map_err(|e| format!("Invalid app_dir in manifest: {e}"))?;
    let install_dir = safe_subdir(release_dir, &manifest.install_dir)
        .map_err(|e| format!("Invalid install_dir in manifest: {e}"))?;
    let ctx = tako_runtime::PluginContext {
        project_dir: &app_dir,
        package_manager: manifest.package_manager.as_deref(),
    };
    if let Some(def) = tako_runtime::runtime_def_for(runtime, Some(&ctx))
        && let Some(install_cmd) = &def.package_manager.install
    {
        tracing::info!(runtime = %runtime, install_dir = %install_dir.display(), "Running production install: {}", install_cmd);
        let mut cmd = TokioCommand::new("sh");
        cmd.args(["-c", install_cmd.as_str()])
            .current_dir(&install_dir)
            .envs(&install_env);
        #[cfg(unix)]
        drop_privileges_if_root(&mut cmd);
        let output = cmd
            .output()
            .await
            .map_err(|e| format!("Failed to run production install: {e}"))?;
        if !output.status.success() {
            return Err(format_process_failure(
                "production install",
                output.status,
                &output.stdout,
                &output.stderr,
            ));
        }
    }

    Ok(runtime_bin)
}

pub(crate) async fn resolve_release_runtime_bin(
    release_dir: &Path,
    data_dir: &Path,
) -> Result<Option<String>, String> {
    let manifest = load_release_manifest(release_dir)?;
    let runtime = &manifest.runtime;
    if runtime.trim().is_empty() {
        return Err(format!(
            "deploy manifest {} has empty runtime field",
            release_dir.join("app.json").display()
        ));
    }

    let runtime_bin = crate::version_manager::install_and_resolve(
        runtime,
        manifest.runtime_version.as_deref(),
        data_dir,
    )
    .await;

    Ok(runtime_bin)
}

fn format_process_failure(
    context: &str,
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let status_text = match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    };

    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
    let detail = if !stderr_text.is_empty() {
        stderr_text
    } else {
        stdout_text
    };

    if detail.is_empty() {
        return format!("{context} ({status_text})");
    }

    let preview: String = detail.chars().take(400).collect();
    if detail.chars().count() > 400 {
        format!("{context} ({status_text}): {preview}...")
    } else {
        format!("{context} ({status_text}): {preview}")
    }
}

pub(crate) fn is_private_local_hostname(domain: &str) -> bool {
    let host = domain
        .split(':')
        .next()
        .unwrap_or(domain)
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if host.is_empty() {
        return false;
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if !host.contains('.') {
        return true;
    }

    host.ends_with(".local")
        || host.ends_with(".test")
        || host.ends_with(".invalid")
        || host.ends_with(".example")
        || host.ends_with(".home.arpa")
}

pub(crate) fn should_use_self_signed_route_cert(domain: &str) -> bool {
    is_private_local_hostname(domain)
}

fn validate_app_name_segment(app_name: &str) -> Result<(), String> {
    if app_name.is_empty() {
        return Err("Invalid app name: must not be empty".to_string());
    }
    if app_name.len() > 63 {
        return Err("Invalid app name: must be 63 characters or fewer".to_string());
    }
    if !app_name
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase())
        .unwrap_or(false)
    {
        return Err("Invalid app name: must start with a lowercase letter".to_string());
    }
    if app_name.ends_with('-') {
        return Err("Invalid app name: must not end with '-'".to_string());
    }
    if !app_name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "Invalid app name: only lowercase letters, digits, and '-' are allowed".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn validate_app_name(app_name: &str) -> Result<(), String> {
    if let Some((app, env)) = tako_core::split_deployment_app_id(app_name) {
        validate_app_name_segment(app)?;
        validate_app_name_segment(env)?;
        return Ok(());
    }

    validate_app_name_segment(app_name)
}

pub(crate) fn requested_deployment_identity(app_id: &str) -> (String, String) {
    if let Some((name, environment)) = tako_core::split_deployment_app_id(app_id) {
        return (name.to_string(), environment.to_string());
    }
    (app_id.to_string(), "production".to_string())
}

pub(crate) fn validate_release_version(version: &str) -> Result<(), String> {
    if version.is_empty() {
        return Err("Invalid release version: must not be empty".to_string());
    }
    if version.len() > 128 {
        return Err("Invalid release version: must be 128 characters or fewer".to_string());
    }
    if version == "." || version == ".." {
        return Err("Invalid release version: '.' and '..' are not allowed".to_string());
    }
    if version.contains('/') || version.contains('\\') {
        return Err("Invalid release version: path separators are not allowed".to_string());
    }
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(
            "Invalid release version: only letters, digits, '.', '_' and '-' are allowed"
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn validate_release_path_for_app(
    data_dir: &Path,
    app_name: &str,
    path: &str,
) -> Result<PathBuf, String> {
    let release_path = std::fs::canonicalize(Path::new(path))
        .map_err(|e| format!("Invalid release path: {} ({})", path, e))?;
    if !release_path.is_dir() {
        return Err(format!(
            "Invalid release path: '{}' must be an existing directory",
            release_path.display()
        ));
    }

    let expected_root = data_dir.join("apps").join(app_name).join("releases");
    let expected_root = std::fs::canonicalize(&expected_root).unwrap_or(expected_root);
    if !release_path.starts_with(&expected_root) {
        return Err(format!(
            "Invalid release path: '{}' must stay under '{}'",
            release_path.display(),
            expected_root.display()
        ));
    }

    Ok(release_path)
}

pub(crate) fn validate_deploy_routes(routes: &[String]) -> Result<(), String> {
    if routes.is_empty() {
        return Err("Deploy rejected: app must define at least one route".to_string());
    }
    if routes.iter().any(|r| r.trim().is_empty()) {
        return Err("Deploy rejected: routes must be non-empty values".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn app_runtime_data_paths_use_nested_app_and_tako_dirs() {
        let data_dir = Path::new("/opt/tako");
        let paths = app_runtime_data_paths(data_dir, "my-app/production");
        assert_eq!(
            paths.root,
            Path::new("/opt/tako/apps/my-app/production/data")
        );
        assert_eq!(
            paths.app,
            Path::new("/opt/tako/apps/my-app/production/data/app")
        );
        assert_eq!(
            paths.tako,
            Path::new("/opt/tako/apps/my-app/production/data/tako")
        );
    }

    #[test]
    fn ensure_app_runtime_data_dirs_creates_both_directories() {
        let temp = TempDir::new().unwrap();
        let paths = ensure_app_runtime_data_dirs(temp.path(), "my-app").unwrap();
        assert!(paths.app.is_dir());
        assert!(paths.tako.is_dir());
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    #[cfg(unix)]
    fn ensure_app_runtime_data_dirs_makes_app_data_group_writable() {
        let temp = TempDir::new().unwrap();
        let paths = ensure_app_runtime_data_dirs(temp.path(), "my-app").unwrap();

        assert_eq!(mode(&paths.root), 0o710);
        assert_eq!(mode(&paths.app), 0o2770);
        assert_eq!(mode(&paths.tako), 0o700);
    }

    #[test]
    #[cfg(unix)]
    fn ensure_app_runtime_data_dirs_repairs_existing_app_data_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let paths = app_runtime_data_paths(temp.path(), "my-app");
        std::fs::create_dir_all(&paths.app).unwrap();
        std::fs::create_dir_all(&paths.tako).unwrap();
        let db_path = paths.app.join("mission.sqlite3");
        let wal_path = paths.app.join("mission.sqlite3-wal");
        std::fs::write(&db_path, "db").unwrap();
        std::fs::write(&wal_path, "wal").unwrap();
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&wal_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        ensure_app_runtime_data_dirs(temp.path(), "my-app").unwrap();

        assert_eq!(mode(&db_path) & 0o660, 0o660);
        assert_eq!(mode(&wal_path) & 0o660, 0o660);
    }

    #[test]
    fn inject_app_data_dir_env_sets_tako_data_dir() {
        let mut env = HashMap::new();
        let paths = AppRuntimeDataPaths {
            root: PathBuf::from("/tmp/app/data"),
            app: PathBuf::from("/tmp/app/data/app"),
            tako: PathBuf::from("/tmp/app/data/tako"),
        };
        inject_app_data_dir_env(&mut env, &paths);
        assert_eq!(
            env.get(TAKO_APP_DATA_DIR_ENV).map(String::as_str),
            Some("/tmp/app/data/app")
        );
    }
}
