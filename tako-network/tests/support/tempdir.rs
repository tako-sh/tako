pub(crate) fn private_tempdir() -> tempfile::TempDir {
    // Production rejects symlink ancestors (including macOS /var) and shared roots.
    let parent = std::env::temp_dir()
        .canonicalize()
        .expect("canonical temporary directory");
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder
        .tempdir_in(parent)
        .expect("private temporary directory")
}
