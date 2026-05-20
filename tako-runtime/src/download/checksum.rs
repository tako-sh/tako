use sha2::{Digest, Sha256};

use super::http::download_metadata_bytes;

pub(super) async fn verify_checksum(
    data: &[u8],
    checksum_url: &str,
    checksum_format: &str,
    archive_url: &str,
) -> Result<(), String> {
    let checksum_text = download_metadata_bytes(checksum_url)
        .await
        .map_err(|e| format!("failed to fetch checksum from {checksum_url}: {e}"))?;
    let checksum_text = String::from_utf8_lossy(&checksum_text);

    let mut hasher = Sha256::new();
    hasher.update(data);
    let actual_hash = hex::encode(hasher.finalize());

    match checksum_format {
        "shasums" => {
            // SHASUMS256.txt format: "<hash>  <filename>"
            let archive_filename = archive_url
                .rsplit_once('/')
                .map_or(archive_url, |(_, file_name)| file_name);
            for line in checksum_text.lines() {
                let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
                if parts.len() == 2 {
                    let expected_hash = parts[0].trim();
                    let filename = parts[1].trim().trim_start_matches('*');
                    if filename == archive_filename && expected_hash == actual_hash {
                        return Ok(());
                    }
                }
            }
            Err(format!(
                "checksum mismatch: no matching entry for '{archive_filename}' in {checksum_url}"
            ))
        }
        "sha256" => {
            // Single hash or "<hash>  <filename>" format
            let expected = checksum_text
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim();
            if expected == actual_hash {
                Ok(())
            } else {
                Err(format!(
                    "checksum mismatch: expected {expected}, got {actual_hash}"
                ))
            }
        }
        other => Err(format!("unsupported checksum format: {other}")),
    }
}
