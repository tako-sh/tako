use std::path::Path;

use tokio::io::AsyncWriteExt;

pub(super) async fn download_url_to_file(url: &str, path: &Path) -> Result<u64, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
    }

    let mut response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download backup: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("download backup returned {}", response.status()));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| format!("create output file {}: {e}", path.display()))?;
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("read backup download: {e}"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write output file {}: {e}", path.display()))?;
        written = written.saturating_add(chunk.len() as u64);
    }
    file.shutdown()
        .await
        .map_err(|e| format!("flush output file {}: {e}", path.display()))?;
    Ok(written)
}
