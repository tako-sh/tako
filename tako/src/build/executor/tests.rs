use super::*;
use std::fs;
use std::io::Read;
use std::path::Path;
use tempfile::TempDir;

fn assert_zstd_magic(path: &Path) {
    let mut file = fs::File::open(path).unwrap();
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).unwrap();
    assert_eq!(magic, [0x28, 0xB5, 0x2F, 0xFD], "archive should be zstd");
}

#[test]
fn test_run_build_echo() {
    let temp = TempDir::new().unwrap();
    let executor = BuildExecutor::new(temp.path());

    let result = executor.run_build("echo hello").unwrap();
    assert!(result.success);
    assert!(result.stdout.contains("hello"));
}

#[test]
fn test_run_build_failure() {
    let temp = TempDir::new().unwrap();
    let executor = BuildExecutor::new(temp.path());

    let result = executor.run_build("false").unwrap();
    assert!(!result.success);
}

#[test]
fn test_run_build_not_found() {
    let temp = TempDir::new().unwrap();
    let executor = BuildExecutor::new(temp.path());

    let result = executor.run_build("nonexistent_command_12345").unwrap();
    assert!(!result.success);
    assert_eq!(result.exit_code, Some(127));
}

#[test]
fn test_create_and_extract_archive() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let archive_path = temp.path().join("test.tar.zst");
    let dest = temp.path().join("dest");

    // Create source files
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("file1.txt"), "content1").unwrap();
    fs::create_dir_all(source.join("subdir")).unwrap();
    fs::write(source.join("subdir/file2.txt"), "content2").unwrap();

    // Create archive
    let executor = BuildExecutor::new(&source);
    let size = executor
        .create_archive(&source, &archive_path, &[])
        .unwrap();
    assert!(size > 0);
    assert!(archive_path.exists());
    assert_zstd_magic(&archive_path);

    // Extract archive
    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    assert!(dest.join("file1.txt").exists());
    assert!(dest.join("subdir/file2.txt").exists());

    // Verify contents
    assert_eq!(
        fs::read_to_string(dest.join("file1.txt")).unwrap(),
        "content1"
    );
    assert_eq!(
        fs::read_to_string(dest.join("subdir/file2.txt")).unwrap(),
        "content2"
    );
}

#[test]
fn test_create_archive_with_extra_files_includes_virtual_manifest() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let archive_path = temp.path().join("test.tar.zst");
    let dest = temp.path().join("dest");

    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("index.ts"), "export default {};").unwrap();

    let executor = BuildExecutor::new(&source);
    executor
        .create_archive_with_extra_files(
            &source,
            &archive_path,
            &[],
            &[("app.json", br#"{"main":"index.ts"}"#)],
        )
        .unwrap();

    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    assert!(dest.join("index.ts").exists());
    assert_eq!(
        fs::read_to_string(dest.join("app.json")).unwrap(),
        r#"{"main":"index.ts"}"#
    );
}

#[test]
fn test_archive_excludes_node_modules() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let archive_path = temp.path().join("test.tar.zst");
    let dest = temp.path().join("dest");

    // Create source with node_modules
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("index.js"), "console.log('hello')").unwrap();
    fs::create_dir_all(source.join("node_modules/dep")).unwrap();
    fs::write(source.join("node_modules/dep/index.js"), "module").unwrap();

    // Create archive
    let executor = BuildExecutor::new(&source);
    executor
        .create_archive(&source, &archive_path, &[])
        .unwrap();

    // Extract and verify node_modules excluded
    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    assert!(dest.join("index.js").exists());
    assert!(!dest.join("node_modules").exists());
}

#[test]
fn test_create_source_archive_respects_gitignore() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let archive_path = temp.path().join("source.tar.zst");
    let dest = temp.path().join("dest");
    fs::create_dir_all(source.join("dist")).unwrap();
    fs::create_dir_all(source.join("src")).unwrap();

    fs::write(source.join(".gitignore"), "dist/\n").unwrap();
    fs::write(source.join("src/main.ts"), "export default 1;\n").unwrap();
    fs::write(source.join("dist/out.txt"), "out").unwrap();

    let executor = BuildExecutor::new(&source);
    executor
        .create_source_archive_with_extra_files(&source, &archive_path, &[])
        .unwrap();
    assert_zstd_magic(&archive_path);

    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    assert!(dest.join("src/main.ts").exists());
    assert!(!dest.join("dist/out.txt").exists());
}

#[test]
fn test_create_source_archive_keeps_default_excludes_non_overridable() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let archive_path = temp.path().join("source.tar.zst");
    let dest = temp.path().join("dest");

    fs::create_dir_all(source.join("src")).unwrap();
    fs::create_dir_all(source.join(".git")).unwrap();
    fs::create_dir_all(source.join(".tako/cache")).unwrap();
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::create_dir_all(source.join("target/debug")).unwrap();

    fs::write(source.join("src/main.ts"), "export default 1;\n").unwrap();
    fs::write(source.join(".git/config"), "git").unwrap();
    fs::write(source.join(".tako/cache/x"), "cache").unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();
    fs::write(source.join("target/debug/out.txt"), "out").unwrap();
    fs::write(source.join(".env.production"), "secret").unwrap();

    let executor = BuildExecutor::new(&source);
    executor
        .create_source_archive_with_extra_files(&source, &archive_path, &[])
        .unwrap();

    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    assert!(dest.join("src/main.ts").exists());
    assert!(!dest.join(".git/config").exists());
    assert!(!dest.join(".tako/cache/x").exists());
    assert!(!dest.join("node_modules/pkg/index.js").exists());
    assert!(!dest.join("target/debug/out.txt").exists());
    assert!(!dest.join(".env.production").exists());
}

#[cfg(unix)]
#[test]
fn test_create_source_archive_preserves_symlinks() {
    use std::os::unix::fs as unix_fs;

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let archive_path = temp.path().join("source.tar.zst");
    let dest = temp.path().join("dest");

    fs::create_dir_all(source.join("sdk")).unwrap();
    fs::create_dir_all(source.join("app")).unwrap();
    fs::write(source.join("sdk/index.js"), "ok").unwrap();
    unix_fs::symlink("../sdk", source.join("app/linked-sdk")).unwrap();

    let executor = BuildExecutor::new(&source);
    executor
        .create_source_archive_with_extra_files(&source, &archive_path, &[])
        .unwrap();

    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    let metadata = fs::symlink_metadata(dest.join("app/linked-sdk")).unwrap();
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn test_create_archive_preserves_directory_symlink_without_following() {
    use std::os::unix::fs as unix_fs;

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let outside = temp.path().join("outside");
    let archive_path = temp.path().join("test.tar.zst");
    let dest = temp.path().join("dest");

    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    unix_fs::symlink(&outside, source.join("linked")).unwrap();

    let executor = BuildExecutor::new(&source);
    executor
        .create_archive(&source, &archive_path, &[])
        .unwrap();

    BuildExecutor::extract_archive(&archive_path, &dest).unwrap();
    let metadata = fs::symlink_metadata(dest.join("linked")).unwrap();
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn test_compute_source_hash_supports_directory_symlinks() {
    use std::os::unix::fs as unix_fs;

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("sdk")).unwrap();
    fs::create_dir_all(source.join("app")).unwrap();
    fs::write(source.join("sdk/index.js"), "ok").unwrap();
    unix_fs::symlink("../sdk", source.join("app/linked-sdk")).unwrap();

    let executor = BuildExecutor::new(&source);
    let hash = executor.compute_source_hash(&source).unwrap();
    assert!(!hash.is_empty());
}

#[cfg(unix)]
#[test]
fn test_compute_dir_hash_uses_symlink_target_without_following_directory() {
    use std::os::unix::fs as unix_fs;

    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("project");
    let outside = temp.path().join("outside");

    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "secret-v1").unwrap();
    unix_fs::symlink(&outside, dir.join("linked")).unwrap();

    let hash1 = compute_dir_hash(&dir, &[]).unwrap();
    fs::write(outside.join("secret.txt"), "secret-v2").unwrap();
    let hash2 = compute_dir_hash(&dir, &[]).unwrap();

    assert_eq!(hash1, hash2);
}

#[test]
fn test_compute_file_hash() {
    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("test.txt");
    fs::write(&file_path, "hello world").unwrap();

    let hash = compute_file_hash(&file_path).unwrap();
    // SHA256 of "hello world"
    assert_eq!(
        hash,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn test_compute_dir_hash_deterministic() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("project");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "aaa").unwrap();
    fs::write(dir.join("b.txt"), "bbb").unwrap();

    let hash1 = compute_dir_hash(&dir, &[]).unwrap();
    let hash2 = compute_dir_hash(&dir, &[]).unwrap();
    assert_eq!(hash1, hash2);

    // Modify a file
    fs::write(dir.join("a.txt"), "changed").unwrap();
    let hash3 = compute_dir_hash(&dir, &[]).unwrap();
    assert_ne!(hash1, hash3);
}

#[test]
fn test_compute_source_hash_matches_source_archive_filters() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::create_dir_all(source.join("dist")).unwrap();
    fs::create_dir_all(source.join(".git")).unwrap();
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::create_dir_all(source.join("target/debug")).unwrap();

    fs::write(source.join(".gitignore"), "dist/\n").unwrap();
    fs::write(source.join("src/main.ts"), "main-v1").unwrap();
    fs::write(source.join("dist/out.txt"), "out-v1").unwrap();
    fs::write(source.join(".env.production"), "secret-v1").unwrap();
    fs::write(source.join(".git/config"), "git-v1").unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "pkg-v1").unwrap();
    fs::write(source.join("target/debug/out.txt"), "out-v1").unwrap();

    let executor = BuildExecutor::new(&source);
    let hash1 = executor.compute_source_hash(&source).unwrap();

    // Changes to excluded files should not change the source hash.
    fs::write(source.join("dist/out.txt"), "out-v2").unwrap();
    fs::write(source.join(".env.production"), "secret-v2").unwrap();
    fs::write(source.join(".git/config"), "git-v2").unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "pkg-v2").unwrap();
    fs::write(source.join("target/debug/out.txt"), "out-v2").unwrap();
    let hash2 = executor.compute_source_hash(&source).unwrap();
    assert_eq!(hash1, hash2);

    // Changes to included files should change the source hash.
    fs::write(source.join("src/main.ts"), "main-v2").unwrap();
    let hash3 = executor.compute_source_hash(&source).unwrap();
    assert_ne!(hash2, hash3);
}

#[test]
fn test_generate_version_falls_back_when_git_commit_missing() {
    let temp = TempDir::new().unwrap();
    let executor = BuildExecutor::new(temp.path());

    let version = executor.generate_version(Some("abcdef123456")).unwrap();
    assert_eq!(version, "nogit_abcdef12");
}

#[test]
fn test_generate_version_falls_back_with_timestamp_when_no_hash() {
    let temp = TempDir::new().unwrap();
    let executor = BuildExecutor::new(temp.path());

    let version = executor.generate_version(None).unwrap();
    assert!(version.starts_with("nogit_"));
    assert!(version.len() > "nogit_".len());
}
