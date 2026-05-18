use std::path::Path;

pub(crate) fn extract_zstd_archive(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("create extraction dir {}: {}", dest_dir.display(), e))?;
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("open archive {}: {}", archive_path.display(), e))?;
    let decoder = zstd::stream::read::Decoder::new(file).map_err(|e| {
        format!(
            "initialize zstd decoder for {}: {}",
            archive_path.display(),
            e
        )
    })?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest_dir).map_err(|e| {
        format!(
            "extract archive {} into {}: {}",
            archive_path.display(),
            dest_dir.display(),
            e
        )
    })?;
    Ok(())
}
