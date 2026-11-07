const DETECT_LIBC_COMMAND: &str = "if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then echo musl; \
elif command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -Eqi 'glibc|gnu libc|gnu c library'; then echo glibc; \
elif command -v getconf >/dev/null 2>&1 && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then echo glibc; \
elif ls /lib/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1 >/dev/null 2>&1; then echo musl; \
else echo unknown; fi";

pub(in crate::commands::server) async fn detect_server_target(
    ssh: &crate::ssh::SshClient,
) -> Result<crate::config::ServerTarget, String> {
    let combined = format!(
        "echo ARCH:$(uname -m 2>/dev/null || echo unknown); echo LIBC:$({})",
        DETECT_LIBC_COMMAND
    );
    let output = ssh
        .exec(&combined)
        .await
        .map_err(|e| format!("Failed to detect server target: {}", e))?;

    let mut arch_str = String::new();
    let mut libc_str = String::new();
    for line in output.stdout.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("ARCH:") {
            arch_str = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("LIBC:") {
            libc_str = val.trim().to_string();
        }
    }

    let arch = parse_detected_arch(&arch_str)?;
    let libc = parse_detected_libc(&libc_str)?;

    crate::config::ServerTarget::normalized(&arch, &libc)
        .map_err(|e| format!("Unsupported target metadata: {}", e))
}

pub(super) fn parse_detected_arch(stdout: &str) -> Result<String, String> {
    let raw = stdout.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(raw_arch) = raw else {
        return Err("Could not detect server architecture from `uname -m` output".to_string());
    };

    crate::config::ServerTarget::normalize_arch(raw_arch).ok_or_else(|| {
        format!(
            "Unsupported server architecture '{}'. Supported architectures: x86_64, aarch64.",
            raw_arch
        )
    })
}

pub(super) fn parse_detected_libc(stdout: &str) -> Result<String, String> {
    let raw = stdout.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(raw_libc) = raw else {
        return Err("Could not detect server libc".to_string());
    };

    crate::config::ServerTarget::normalize_libc(raw_libc).ok_or_else(|| {
        format!(
            "Unsupported server libc '{}'. Tako requires glibc.",
            raw_libc
        )
    })
}
