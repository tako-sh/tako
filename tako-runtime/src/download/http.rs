use super::github::apply_github_auth_for_url;

/// Maximum download size for runtime archives (256 MiB).
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum download size for checksum/metadata files (1 MiB).
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// Cap on redirect hops when downloading a runtime archive. Integrity is
/// enforced by the mandatory checksum, not the redirect target, so this exists
/// purely to bound the number of round trips per download.
const MAX_DOWNLOAD_REDIRECTS: usize = 10;

pub(super) async fn download_archive_bytes(url: &str) -> Result<Vec<u8>, String> {
    download_bytes_limited(url, MAX_ARCHIVE_BYTES).await
}

pub(super) async fn download_metadata_bytes(url: &str) -> Result<Vec<u8>, String> {
    download_bytes_limited(url, MAX_METADATA_BYTES).await
}

async fn download_bytes_limited(url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::limited(MAX_DOWNLOAD_REDIRECTS))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let response = client.get(url).header("User-Agent", "tako-server");
    let response = apply_github_auth_for_url(response, url)
        .send()
        .await
        .map_err(|e| format!("download failed for {url}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "download failed: HTTP {} for {url}",
            response.status()
        ));
    }

    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(format!(
            "download too large: {len} bytes exceeds limit of {max_bytes} bytes for {url}"
        ));
    }

    let bytes = read_limited_body(response, max_bytes, url).await?;
    Ok(bytes)
}

async fn read_limited_body(
    mut response: reqwest::Response,
    max_bytes: u64,
    url: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("failed to read response body from {url}: {e}"))?
    {
        append_chunk_limited(&mut bytes, &chunk, max_bytes, url)?;
    }
    Ok(bytes)
}

fn append_chunk_limited(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: u64,
    url: &str,
) -> Result<(), String> {
    let next_len = bytes.len() as u64 + chunk.len() as u64;
    if next_len > max_bytes {
        return Err(format!(
            "download too large: {next_len} bytes exceeds limit of {max_bytes} bytes for {url}"
        ));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::append_chunk_limited;

    #[test]
    fn append_chunk_limited_rejects_chunk_that_crosses_limit() {
        let mut bytes = vec![0; 4];

        let error = append_chunk_limited(&mut bytes, &[1, 2], 5, "https://example.com/runtime")
            .unwrap_err();

        assert!(error.contains("download too large"), "{error}");
        assert_eq!(bytes.len(), 4);
    }
}
