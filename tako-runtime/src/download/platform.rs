pub(super) fn resolve_os() -> &'static str {
    std::env::consts::OS
}

pub(super) fn resolve_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
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
) -> Result<String, String> {
    let generic = resolve_arch();
    arch_map
        .get(generic)
        .cloned()
        .ok_or_else(|| format!("no arch mapping for '{generic}'"))
}
