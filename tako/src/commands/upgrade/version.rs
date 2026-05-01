use crate::output;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_SHA: Option<&str> = option_env!("TAKO_BUILD_SHA");

const REPO_OWNER: &str = "lilienblum";
const REPO_NAME: &str = "tako";
const LATEST_TAG: &str = "latest";

pub(super) enum UpdateCheck {
    AlreadyCurrent,
    Available { version: String },
}

pub(super) fn current_version() -> String {
    match BUILD_SHA {
        Some(sha) if !sha.trim().is_empty() => {
            let short = &sha.trim()[..sha.trim().len().min(7)];
            format!("{CURRENT_VERSION}-{short}")
        }
        _ => CURRENT_VERSION.to_string(),
    }
}

pub(crate) async fn fetch_latest_version() -> Result<String, Box<dyn std::error::Error>> {
    let url =
        format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/git/ref/tags/{LATEST_TAG}");
    let client = reqwest::Client::new();
    let resp = crate::github::apply_api_headers(client.get(&url).header("User-Agent", "tako-cli"))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| format!("failed to resolve {LATEST_TAG} tag: {e}"))?;
    let body: serde_json::Value = resp.json().await?;
    let sha = body["object"]["sha"]
        .as_str()
        .ok_or("latest tag response missing object.sha")?;
    let short = &sha[..sha.len().min(7)];
    Ok(format!("{CURRENT_VERSION}-{short}"))
}

pub(super) fn tarball_url(os: &str, arch: &str) -> String {
    if let Ok(base) = std::env::var("TAKO_DOWNLOAD_BASE_URL") {
        let base = base.trim().trim_end_matches('/');
        if !base.is_empty() {
            if !base.starts_with("https://") {
                output::warning(&format!(
                    "TAKO_DOWNLOAD_BASE_URL uses non-HTTPS scheme — binary will be downloaded over an insecure connection: {base}"
                ));
            }
            return format!("{base}/tako-{os}-{arch}.tar.gz");
        }
    }
    format!(
        "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{LATEST_TAG}/tako-{os}-{arch}.tar.gz"
    )
}

pub(super) async fn check_for_updates() -> Result<UpdateCheck, String> {
    let remote = fetch_latest_version().await.map_err(|e| e.to_string())?;
    let local = current_version();
    if remote == local {
        Ok(UpdateCheck::AlreadyCurrent)
    } else {
        Ok(UpdateCheck::Available { version: remote })
    }
}
