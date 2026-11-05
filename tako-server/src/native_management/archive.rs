use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MAX_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_METADATA_BYTES: u64 = 16 * 1024;

pub(super) fn validate(path: &Path) -> Result<(), ()> {
    // Inspect raw extension headers before tar allocates their declared contents.
    let mut raw = open(path)?;
    let mut metadata_bytes = 0_u64;
    for (count, entry) in raw.entries().map_err(|_| ())?.raw(true).enumerate() {
        let mut entry = entry.map_err(|_| ())?;
        if count >= MAX_ARCHIVE_ENTRIES {
            return Err(());
        }
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() || kind.is_gnu_longlink() {
            metadata_bytes = metadata_bytes.checked_add(entry.size()).ok_or(())?;
            if entry.size() > MAX_METADATA_BYTES || metadata_bytes > 8 * 1024 * 1024 {
                return Err(());
            }
        } else if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            return Err(());
        }
        if entry.size() > MAX_EXPANDED_BYTES {
            return Err(());
        }
        std::io::copy(&mut entry, &mut std::io::sink()).map_err(|_| ())?;
    }
    finish(raw)?;
    let mut archive = open(path)?;
    let mut paths = HashSet::new();
    let mut links = HashMap::new();
    let mut name_bytes = 0;
    for entry in archive.entries().map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            return Err(());
        }
        let path = entry.path().map_err(|_| ())?;
        name_bytes += path.as_os_str().len();
        if name_bytes > 8 * 1024 * 1024
            || path.as_os_str().len() > MAX_METADATA_BYTES as usize
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(());
        }
        let path: PathBuf = path
            .components()
            .filter(|part| *part != Component::CurDir)
            .collect();
        if path.as_os_str().is_empty() {
            if entry.header().entry_type().is_dir() {
                continue;
            }
            return Err(());
        }
        if !paths.insert(path.clone()) {
            return Err(());
        }
        if entry.header().entry_type().is_symlink() {
            let target = entry.link_name().map_err(|_| ())?.ok_or(())?.into_owned();
            name_bytes += target.as_os_str().len();
            if name_bytes > 8 * 1024 * 1024
                || target.as_os_str().is_empty()
                || target.as_os_str().len() > MAX_METADATA_BYTES as usize
            {
                return Err(());
            }
            links.insert(path, target);
        }
    }
    for path in &paths {
        if path
            .ancestors()
            .skip(1)
            .any(|parent| links.contains_key(parent))
        {
            return Err(());
        }
    }
    for (path, target) in &links {
        resolve_link(path.parent().ok_or(())?, target, &links)?;
    }
    Ok(())
}

type Archive =
    tar::Archive<std::io::Take<zstd::Decoder<'static, std::io::BufReader<std::fs::File>>>>;
fn open(path: &Path) -> Result<Archive, ()> {
    let file = std::fs::File::open(path).map_err(|_| ())?;
    let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|_| ())?;
    decoder.window_log_max(27).map_err(|_| ())?;
    Ok(tar::Archive::new(decoder.take(MAX_EXPANDED_BYTES + 1)))
}
fn finish(archive: Archive) -> Result<(), ()> {
    let mut remaining = archive.into_inner();
    std::io::copy(&mut remaining, &mut std::io::sink()).map_err(|_| ())?;
    if remaining.limit() == 0 {
        return Err(());
    }
    Ok(())
}

fn resolve_link(parent: &Path, target: &Path, links: &HashMap<PathBuf, PathBuf>) -> Result<(), ()> {
    let mut pending: VecDeque<_> = parent
        .join(target)
        .components()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let mut resolved = PathBuf::new();
    let mut expansions = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next().ok_or(())? {
            Component::CurDir => (),
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(());
                }
            }
            Component::Normal(_) => {
                resolved.push(part);
                // Extraction replaces root logs with a link outside the release.
                if resolved == Path::new("logs") {
                    return Err(());
                }
                if let Some(target) = links.get(&resolved) {
                    expansions += 1;
                    if expansions > 40 {
                        return Err(());
                    }
                    resolved.pop();
                    for part in target.components().rev() {
                        pending.push_front(part.as_os_str().to_owned());
                    }
                }
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_graph_cannot_escape_or_cross_the_shared_log_link() {
        let links = HashMap::from([(
            PathBuf::from("node_modules/pkg"),
            PathBuf::from(".pnpm/pkg/node_modules/pkg"),
        )]);
        assert!(
            resolve_link(
                Path::new("node_modules/.bin"),
                Path::new("../pkg/bin"),
                &links
            )
            .is_ok()
        );
        for target in ["../../../outside", "/outside", "../../logs/../secret"] {
            assert!(
                resolve_link(Path::new("node_modules/.bin"), Path::new(target), &links).is_err()
            );
        }
        let links = HashMap::from([
            (PathBuf::from("a"), PathBuf::from("b")),
            (PathBuf::from("b"), PathBuf::from("a")),
        ]);
        assert!(resolve_link(Path::new(""), Path::new("a"), &links).is_err());
    }

    #[test]
    fn hostile_links_and_special_files_are_rejected_before_extraction() {
        for (kind, target) in [
            (tar::EntryType::Symlink, "/etc/passwd"),
            (tar::EntryType::Symlink, "../outside"),
            (tar::EntryType::Link, "file"),
            (tar::EntryType::Fifo, ""),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_entry_type(kind);
            header.set_cksum();
            if target.is_empty() {
                tar.append_data(&mut header, "entry", std::io::empty())
                    .unwrap();
            } else {
                tar.append_link(&mut header, "entry", target).unwrap();
            }
            let path = dir.path().join("artifact");
            std::fs::write(
                &path,
                zstd::stream::encode_all(&tar.into_inner().unwrap()[..], 0).unwrap(),
            )
            .unwrap();
            assert!(validate(&path).is_err());
        }
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    fn archive_file(tar: tar::Builder<Vec<u8>>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        std::fs::write(
            &path,
            zstd::stream::encode_all(&tar.into_inner().unwrap()[..], 0).unwrap(),
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn producer_style_gnu_paths_and_confined_relative_links_are_accepted() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        let long = format!("node_modules/.pnpm/{}/index.js", "package".repeat(25));
        tar.append_data(&mut header, &long, &b"hello"[..]).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_cksum();
        tar.append_link(
            &mut header,
            "node_modules/.bin/tool",
            Path::new("..").join(long.strip_prefix("node_modules/").unwrap()),
        )
        .unwrap();
        let (_dir, path) = archive_file(tar);
        assert!(validate(&path).is_ok());
    }

    #[test]
    fn pax_and_oversized_extensions_cannot_hide_a_different_archive() {
        for (kind, size) in [
            (tar::EntryType::XHeader, 20),
            (tar::EntryType::GNULongName, MAX_METADATA_BYTES + 1),
        ] {
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o644);
            header.set_entry_type(kind);
            header.set_cksum();
            tar.append_data(&mut header, "metadata", &vec![b'x'; size as usize][..])
                .unwrap();
            let (_dir, path) = archive_file(tar);
            assert!(validate(&path).is_err());
        }
    }
}
