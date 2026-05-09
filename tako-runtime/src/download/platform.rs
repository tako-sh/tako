pub(super) fn resolve_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    }
}

pub(super) fn resolve_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn is_musl() -> bool {
    #[cfg(target_os = "linux")]
    {
        let arch = std::env::consts::ARCH;
        std::path::Path::new(&format!("/lib/ld-musl-{arch}.so.1")).exists()
            || std::path::Path::new("/etc/alpine-release").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub(super) fn resolve_os_value(
    os_map: &std::collections::HashMap<String, String>,
) -> Result<String, String> {
    let generic = resolve_os();
    os_map
        .get(generic)
        .cloned()
        .ok_or_else(|| format!("no OS mapping for '{generic}'"))
}

pub(super) fn resolve_arch_value(
    arch_map: &std::collections::HashMap<String, String>,
    arch_variants: &std::collections::HashMap<String, String>,
) -> Result<String, String> {
    let generic = resolve_arch();
    if is_musl() {
        let musl_key = format!("{generic}-musl");
        if let Some(value) = arch_variants.get(&musl_key) {
            return Ok(value.clone());
        }
    }
    arch_map
        .get(generic)
        .cloned()
        .ok_or_else(|| format!("no arch mapping for '{generic}'"))
}
