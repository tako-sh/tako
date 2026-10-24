//! Anonymous CLI usage stats.
//!
//! Official installs send one event per command so we can count unique users
//! and see which commands run. The request starts immediately and is waited
//! out on process exit so short commands still deliver. Opt out with
//! `TAKO_TELEMETRY=0`. CI is always off; local Cargo builds require an explicit
//! opt-in. Failures never affect the command.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ENV_TELEMETRY: &str = "TAKO_TELEMETRY";
const EVENT_NAME: &str = "cli_command";
const SEND_TIMEOUT: Duration = Duration::from_millis(800);
const NOTICE: &str = "Anonymous usage stats are on. Set TAKO_TELEMETRY=0 to opt out.";
const DEFAULT_HOST: &str = "https://us.i.posthog.com";

static IN_FLIGHT: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TelemetryState {
    id: String,
    #[serde(default)]
    notice_shown: bool,
}

pub fn maybe_send(command: &str, ci_flag: bool, version: &str) {
    let Some(token) = compiled_token() else {
        return;
    };
    let telemetry_env = std::env::var(ENV_TELEMETRY).ok();
    let ci_env = std::env::var("CI").ok();
    let local_build = std::env::current_exe()
        .ok()
        .is_some_and(|exe| is_local_build_exe(&exe));
    if !telemetry_enabled(
        true,
        telemetry_env.as_deref(),
        ci_env.as_deref(),
        ci_flag,
        local_build,
    ) {
        return;
    }

    let Ok(path) = state_path() else {
        return;
    };
    let Some(mut state) = load_or_create_state(&path) else {
        return;
    };

    if !state.notice_shown && crate::output::is_pretty() {
        crate::output::hint(NOTICE);
        state.notice_shown = true;
        let _ = save_state(&path, &state);
    }

    let url = capture_url(DEFAULT_HOST);
    let body = capture_payload(
        token,
        &state.id,
        command,
        version,
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    if let Ok(handle) = std::thread::Builder::new()
        .name("tako-telemetry".into())
        .spawn(move || {
            let _ = post_capture(&url, &body);
        })
    {
        remember_in_flight(handle);
    }
}

/// Wait for the in-flight capture so short commands still deliver.
pub fn flush() {
    if let Some(handle) = take_in_flight() {
        let _ = handle.join();
    }
}

pub(crate) fn telemetry_enabled(
    has_token: bool,
    telemetry_env: Option<&str>,
    ci_env: Option<&str>,
    ci_flag: bool,
    local_build: bool,
) -> bool {
    if !has_token || ci_flag || is_truthy_env(ci_env) {
        return false;
    }
    match parse_bool_env(telemetry_env) {
        Some(false) => false,
        Some(true) => true,
        None => !local_build,
    }
}

pub(crate) fn capture_url(host: &str) -> String {
    format!("{}/i/v0/e/", host.trim().trim_end_matches('/'))
}

pub(crate) fn capture_payload(
    token: &str,
    distinct_id: &str,
    command: &str,
    version: &str,
    os: &str,
    arch: &str,
) -> Value {
    // PostHog fills the capture-request IP unless $ip is a non-empty string.
    json!({
        "api_key": token,
        "distinct_id": distinct_id,
        "event": EVENT_NAME,
        "properties": {
            "$lib": "tako-cli",
            "$geoip_disable": true,
            "$ip": "0.0.0.0",
            "$process_person_profile": false,
            "command": command,
            "version": version,
            "os": os,
            "arch": arch,
        }
    })
}

pub(crate) fn is_local_build_exe(exe: &Path) -> bool {
    crate::paths::target_dir_from_exe(exe).is_some()
}

pub(crate) fn load_or_create_state(path: &Path) -> Option<TelemetryState> {
    if let Ok(raw) = fs::read_to_string(path)
        && let Ok(state) = serde_json::from_str::<TelemetryState>(&raw)
        && valid_id(&state.id)
    {
        return Some(state);
    }

    let state = TelemetryState {
        id: new_install_id()?,
        notice_shown: false,
    };
    save_state(path, &state).ok()?;
    Some(state)
}

pub(crate) fn save_state(path: &Path, state: &TelemetryState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_vec_pretty(state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    atomic_write(path, &encoded)
}

pub(crate) fn post_capture(url: &str, body: &Value) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    rt.block_on(async {
        let Ok(client) = reqwest::Client::builder().timeout(SEND_TIMEOUT).build() else {
            return false;
        };
        client
            .post(url)
            .json(body)
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
    })
}

fn remember_in_flight(handle: JoinHandle<()>) {
    let previous = lock_in_flight().replace(handle);
    if let Some(previous) = previous {
        let _ = previous.join();
    }
}

fn take_in_flight() -> Option<JoinHandle<()>> {
    lock_in_flight().take()
}

fn lock_in_flight() -> std::sync::MutexGuard<'static, Option<JoinHandle<()>>> {
    IN_FLIGHT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn compiled_token() -> Option<&'static str> {
    option_env!("TAKO_POSTHOG_PROJECT_TOKEN")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn state_path() -> Result<PathBuf, std::io::Error> {
    Ok(crate::paths::tako_config_dir()?.join("telemetry.json"))
}

fn parse_bool_env(value: Option<&str>) -> Option<bool> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    match value.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

fn is_truthy_env(value: Option<&str>) -> bool {
    parse_bool_env(value) == Some(true)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn new_install_id() -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(hex::encode(bytes))
}

fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    write_file(&tmp, contents)?;
    fs::rename(&tmp, path)
}

fn write_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        file.flush()
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests;
