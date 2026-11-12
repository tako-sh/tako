//! Detect which Tako SDK an app depends on, and at what version.
//!
//! Detection only reads files the app already has: installed packages and
//! lockfiles for JavaScript, `go.mod` for Go, and `Cargo.lock` for Rust.
//! Nothing is executed, and a missing or unreadable file means "unknown".

use std::path::Path;

/// npm package of the JavaScript SDK.
const JS_PACKAGE: &str = "tako.sh";
/// Module path of the Go HTTP SDK.
const GO_MODULE: &str = "tako.sh";
/// Crate name of the Rust SDK.
const RUST_CRATE: &str = "tako";

/// Version recorded for a Go module replaced by a local directory, matching
/// what Go's own build info prints for it.
const GO_LOCAL_VERSION: &str = "(devel)";

/// The Tako SDK an app is built against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSdk {
    /// `javascript`, `go`, or `rust`. The JavaScript package and the Go
    /// module share the name `tako.sh`, so the name alone is ambiguous.
    pub language: &'static str,
    /// Package, module, or crate name as the ecosystem spells it.
    pub name: String,
    pub version: String,
}

/// Detect the Tako SDK used by the app in `app_dir`.
///
/// `runtime_id` picks the language for native runtimes (`bun`, `node`, `go`).
/// Any other value, such as a container release or an explicit start command,
/// tries Go, then Rust, then JavaScript, so a backend SDK wins over a
/// JavaScript package that only serves the browser. Files are looked up in
/// `app_dir` and its ancestors, stopping at `source_root`.
pub fn detect_app_sdk(runtime_id: &str, app_dir: &Path, source_root: &Path) -> Option<AppSdk> {
    let dirs = search_dirs(app_dir, source_root);
    match runtime_id {
        "bun" | "node" => detect_js(&dirs),
        "go" => detect_go(&dirs),
        _ => detect_go(&dirs)
            .or_else(|| detect_rust(&dirs))
            .or_else(|| detect_js(&dirs)),
    }
}

fn search_dirs<'a>(app_dir: &'a Path, source_root: &Path) -> Vec<&'a Path> {
    let dirs: Vec<&Path> = app_dir
        .ancestors()
        .take_while(|dir| dir.starts_with(source_root))
        .collect();
    if dirs.is_empty() { vec![app_dir] } else { dirs }
}

fn read_nearest(dirs: &[&Path], file: &str) -> Option<String> {
    dirs.iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(file)).ok())
}

// ── JavaScript ─────────────────────────────────────────────────────

fn detect_js(dirs: &[&Path]) -> Option<AppSdk> {
    if !dirs.iter().any(|dir| package_json_declares_sdk(dir)) {
        return None;
    }
    // The installed package is what a local build bundles. Without it, the
    // lockfile says what a production install on the server resolves.
    let version = dirs
        .iter()
        .find_map(|dir| package_json_version(&dir.join("node_modules").join(JS_PACKAGE)))
        .or_else(|| dirs.iter().find_map(|dir| locked_js_version(dir)))?;
    Some(AppSdk {
        language: "javascript",
        name: JS_PACKAGE.to_string(),
        version,
    })
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn package_json_declares_sdk(dir: &Path) -> bool {
    let Some(package) = read_json(&dir.join("package.json")) else {
        return false;
    };
    [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .any(|section| {
        package
            .get(section)
            .and_then(|deps| deps.get(JS_PACKAGE))
            .is_some()
    })
}

fn package_json_version(package_dir: &Path) -> Option<String> {
    let package = read_json(&package_dir.join("package.json"))?;
    let version = package.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

fn locked_js_version(dir: &Path) -> Option<String> {
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).ok();
    if let Some(lock) = read("bun.lock") {
        return bun_lock_version(&lock, dir);
    }
    if let Some(lock) = read("package-lock.json") {
        return package_lock_version(&lock);
    }
    if let Some(lock) = read("pnpm-lock.yaml") {
        return pnpm_lock_version(&lock);
    }
    read("yarn.lock").and_then(|lock| yarn_lock_version(&lock))
}

fn looks_like_version(value: &str) -> bool {
    value.starts_with(|c: char| c.is_ascii_digit())
}

/// `"tako.sh": ["tako.sh@1.2.3", ...]`, or `tako.sh@workspace:<dir>` for a
/// workspace member whose own `package.json` holds the version.
fn bun_lock_version(lock: &str, lock_dir: &Path) -> Option<String> {
    let key = format!("\"{JS_PACKAGE}\":");
    let spec_prefix = format!("\"{JS_PACKAGE}@");
    lock.match_indices(&key).find_map(|(index, _)| {
        let entry = lock[index + key.len()..].trim_start().strip_prefix('[')?;
        let spec = entry.trim_start().strip_prefix(&spec_prefix)?;
        let spec = &spec[..spec.find('"')?];
        match spec.strip_prefix("workspace:") {
            Some(member) => package_json_version(&lock_dir.join(member)),
            None => looks_like_version(spec).then(|| spec.to_string()),
        }
    })
}

fn package_lock_version(lock: &str) -> Option<String> {
    let lock: serde_json::Value = serde_json::from_str(lock).ok()?;
    let version = |entry: &serde_json::Value| {
        Some(entry.get("version")?.as_str()?.to_string()).filter(|v| looks_like_version(v))
    };
    if let Some(packages) = lock.get("packages") {
        let entry = packages.get(format!("node_modules/{JS_PACKAGE}"))?;
        // A workspace member is a link to the entry that carries the version.
        let linked = entry
            .get("resolved")
            .and_then(|resolved| resolved.as_str())
            .filter(|_| entry.get("link").and_then(|link| link.as_bool()) == Some(true))
            .and_then(|resolved| packages.get(resolved));
        return version(linked.unwrap_or(entry));
    }
    version(lock.get("dependencies")?.get(JS_PACKAGE)?)
}

/// Package keys look like `tako.sh@1.2.3:`, `/tako.sh@1.2.3(peer@1.0.0):`, or
/// `/tako.sh/1.2.3:` depending on the lockfile version.
fn pnpm_lock_version(lock: &str) -> Option<String> {
    lock.lines().find_map(|line| {
        let key = line.trim().trim_start_matches(['\'', '"']);
        let key = key.strip_prefix('/').unwrap_or(key);
        let rest = key.strip_prefix(JS_PACKAGE)?;
        let rest = rest.strip_prefix('@').or_else(|| rest.strip_prefix('/'))?;
        let end = rest.find(['(', ':', '_', '\'', '"'])?;
        looks_like_version(rest).then(|| rest[..end].to_string())
    })
}

/// Classic entries read `version "1.2.3"`; Berry entries read `version: 1.2.3`.
fn yarn_lock_version(lock: &str) -> Option<String> {
    let mut in_entry = false;
    for line in lock.lines() {
        if !line.starts_with(char::is_whitespace) {
            let header = line.trim_start_matches('"');
            in_entry = header
                .strip_prefix(JS_PACKAGE)
                .is_some_and(|rest| rest.starts_with('@'));
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(value) = line.trim().strip_prefix("version") {
            let version = value.trim_start_matches(':').trim().trim_matches('"');
            return looks_like_version(version).then(|| version.to_string());
        }
    }
    None
}

// ── Go ─────────────────────────────────────────────────────────────

fn detect_go(dirs: &[&Path]) -> Option<AppSdk> {
    go_mod_sdk(&read_nearest(dirs, "go.mod")?)
}

fn go_mod_sdk(go_mod: &str) -> Option<AppSdk> {
    let mut required: Option<String> = None;
    // (version the replacement is limited to, replacement, replacement version)
    let mut replacement: Option<(Option<String>, String, Option<String>)> = None;
    let mut block: Option<String> = None;

    for line in go_mod.lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line == ")" {
            block = None;
            continue;
        }
        let mut tokens = line
            .split_whitespace()
            .map(|token| token.trim_matches(['"', '`']));
        let directive = match &block {
            Some(directive) => directive.clone(),
            None => {
                let directive = tokens.next().unwrap_or_default().to_string();
                let mut rest = tokens.clone();
                if rest.next() == Some("(") {
                    block = Some(directive);
                    continue;
                }
                directive
            }
        };
        let tokens: Vec<&str> = tokens.collect();
        match (directive.as_str(), tokens.as_slice()) {
            ("require", [module, version, ..]) if *module == GO_MODULE => {
                required = Some((*version).to_string());
            }
            ("replace", [module, "=>", target, version @ ..]) if *module == GO_MODULE => {
                replacement = Some((
                    None,
                    (*target).to_string(),
                    version.first().map(|v| (*v).to_string()),
                ));
            }
            ("replace", [module, only, "=>", target, version @ ..]) if *module == GO_MODULE => {
                replacement = Some((
                    Some((*only).to_string()),
                    (*target).to_string(),
                    version.first().map(|v| (*v).to_string()),
                ));
            }
            _ => {}
        }
    }

    let required = required?;
    let sdk = |name: &str, version: String| AppSdk {
        language: "go",
        name: name.to_string(),
        version,
    };
    Some(match replacement {
        Some((only, target, version)) if only.as_ref().is_none_or(|only| *only == required) => {
            match version {
                // Another module stands in for the SDK: record what is compiled.
                Some(version) => sdk(&target, version),
                // A local directory has no version to record.
                None => sdk(GO_MODULE, GO_LOCAL_VERSION.to_string()),
            }
        }
        _ => sdk(GO_MODULE, required),
    })
}

// ── Rust ───────────────────────────────────────────────────────────

fn detect_rust(dirs: &[&Path]) -> Option<AppSdk> {
    cargo_lock_sdk(&read_nearest(dirs, "Cargo.lock")?)
}

/// Path and git dependencies still list the crate's own version, so every
/// source of the `tako` crate is reported the same way.
fn cargo_lock_sdk(cargo_lock: &str) -> Option<AppSdk> {
    let mut name: Option<&str> = None;
    let mut version: Option<&str> = None;
    // The trailing table header flushes the last package.
    for line in cargo_lock.lines().chain(["[[package]]"]) {
        let line = line.trim();
        if line.starts_with('[') {
            if name == Some(RUST_CRATE)
                && let Some(version) = version
            {
                return Some(AppSdk {
                    language: "rust",
                    name: RUST_CRATE.to_string(),
                    version: version.to_string(),
                });
            }
            name = None;
            version = None;
        } else if let Some(value) = toml_string_value(line, "name") {
            name = Some(value);
        } else if let Some(value) = toml_string_value(line, "version") {
            version = Some(value);
        }
    }
    None
}

fn toml_string_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let value = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
    value.trim().strip_prefix('"')?.strip_suffix('"')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, file: &str, content: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn sdk(language: &'static str, name: &str, version: &str) -> Option<AppSdk> {
        Some(AppSdk {
            language,
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    const APP_PACKAGE_JSON: &str = r#"{"name":"app","dependencies":{"tako.sh":"^1.4.0"}}"#;

    #[test]
    fn js_reads_the_installed_package_version() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "package.json", APP_PACKAGE_JSON);
        write(
            temp.path(),
            "node_modules/tako.sh/package.json",
            r#"{"name":"tako.sh","version":"1.4.2"}"#,
        );
        // The lockfile disagrees; the installed package is what gets built.
        write(
            temp.path(),
            "package-lock.json",
            r#"{"packages":{"node_modules/tako.sh":{"version":"1.4.0"}}}"#,
        );
        for runtime in ["bun", "node", "container"] {
            assert_eq!(
                detect_app_sdk(runtime, temp.path(), temp.path()),
                sdk("javascript", "tako.sh", "1.4.2"),
                "{runtime}"
            );
        }
    }

    #[test]
    fn js_resolves_from_the_workspace_root_but_not_above_the_source_root() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        let app = root.join("apps/web");
        write(&app, "package.json", APP_PACKAGE_JSON);
        write(
            &root,
            "node_modules/tako.sh/package.json",
            r#"{"version":"2.0.0"}"#,
        );
        assert_eq!(
            detect_app_sdk("bun", &app, &root),
            sdk("javascript", "tako.sh", "2.0.0")
        );
        assert_eq!(detect_app_sdk("bun", &app, &app), None);
    }

    #[test]
    fn js_requires_the_app_to_declare_the_sdk() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path(),
            "package.json",
            r#"{"dependencies":{"react":"19"}}"#,
        );
        write(
            temp.path(),
            "node_modules/tako.sh/package.json",
            r#"{"version":"1.4.2"}"#,
        );
        assert_eq!(detect_app_sdk("bun", temp.path(), temp.path()), None);
        assert_eq!(detect_app_sdk("container", temp.path(), temp.path()), None);
    }

    #[test]
    fn js_falls_back_to_each_lockfile_format() {
        for (file, content, expected) in [
            (
                "bun.lock",
                "{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {\n    \"\": { \"dependencies\": { \"tako.sh\": \"^1.4.0\" } },\n  },\n  \"packages\": {\n    \"tako.sh\": [\"tako.sh@1.4.3\", \"\", {}, \"sha512-abc\"],\n  }\n}\n",
                "1.4.3",
            ),
            (
                "package-lock.json",
                r#"{"lockfileVersion":3,"packages":{"":{"dependencies":{"tako.sh":"^1.4.0"}},"node_modules/tako.sh":{"version":"1.4.4","resolved":"https://registry.npmjs.org/tako.sh/-/tako.sh-1.4.4.tgz"}}}"#,
                "1.4.4",
            ),
            (
                "package-lock.json",
                r#"{"lockfileVersion":1,"dependencies":{"tako.sh":{"version":"1.4.5"}}}"#,
                "1.4.5",
            ),
            (
                "pnpm-lock.yaml",
                "lockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies:\n      tako.sh:\n        specifier: ^1.4.0\n        version: 1.4.6\n\npackages:\n\n  tako.sh@1.4.6:\n    resolution: {integrity: sha512-abc}\n",
                "1.4.6",
            ),
            (
                "pnpm-lock.yaml",
                "lockfileVersion: '6.0'\n\npackages:\n\n  /tako.sh@1.4.7(react@19.0.0):\n    resolution: {integrity: sha512-abc}\n",
                "1.4.7",
            ),
            (
                "yarn.lock",
                "# yarn lockfile v1\n\nreact@^19.0.0:\n  version \"19.0.0\"\n\ntako.sh@^1.4.0:\n  version \"1.4.8\"\n  resolved \"https://registry.yarnpkg.com/tako.sh/-/tako.sh-1.4.8.tgz\"\n",
                "1.4.8",
            ),
            (
                "yarn.lock",
                "__metadata:\n  version: 8\n\n\"tako.sh@npm:^1.4.0\":\n  version: 1.4.9\n  resolution: \"tako.sh@npm:1.4.9\"\n",
                "1.4.9",
            ),
        ] {
            let temp = TempDir::new().unwrap();
            write(temp.path(), "package.json", APP_PACKAGE_JSON);
            write(temp.path(), file, content);
            assert_eq!(
                detect_app_sdk("node", temp.path(), temp.path()),
                sdk("javascript", "tako.sh", expected),
                "{file}"
            );
        }
    }

    #[test]
    fn js_lockfiles_follow_workspace_members_to_their_own_version() {
        let temp = TempDir::new().unwrap();
        let app = temp.path().join("apps/web");
        write(
            &app,
            "package.json",
            r#"{"dependencies":{"tako.sh":"workspace:*"}}"#,
        );
        write(
            temp.path(),
            "sdk/javascript/package.json",
            r#"{"name":"tako.sh","version":"0.0.0"}"#,
        );
        write(
            temp.path(),
            "bun.lock",
            "{\n  \"packages\": {\n    \"tako.sh\": [\"tako.sh@workspace:sdk/javascript\"],\n  }\n}\n",
        );
        assert_eq!(
            detect_app_sdk("bun", &app, temp.path()),
            sdk("javascript", "tako.sh", "0.0.0")
        );

        fs::remove_file(temp.path().join("bun.lock")).unwrap();
        write(
            temp.path(),
            "package-lock.json",
            r#"{"packages":{"node_modules/tako.sh":{"resolved":"sdk/javascript","link":true},"sdk/javascript":{"name":"tako.sh","version":"0.0.1"}}}"#,
        );
        assert_eq!(
            detect_app_sdk("bun", &app, temp.path()),
            sdk("javascript", "tako.sh", "0.0.1")
        );
    }

    #[test]
    fn js_without_an_installed_or_locked_version_is_unknown() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "package.json", APP_PACKAGE_JSON);
        assert_eq!(detect_app_sdk("bun", temp.path(), temp.path()), None);
        write(
            temp.path(),
            "bun.lock",
            "{ \"packages\": { \"tako.sh\": [\"tako.sh@github:tako-sh/tako#abc\"] } }",
        );
        assert_eq!(detect_app_sdk("bun", temp.path(), temp.path()), None);
    }

    #[test]
    fn go_reads_the_required_module_version() {
        for go_mod in [
            "module example.com/app\n\ngo 1.24\n\nrequire tako.sh v0.4.1\n",
            "module example.com/app\n\nrequire (\n\tgithub.com/go-chi/chi/v5 v5.5.0\n\ttako.sh v0.4.1 // indirect\n)\n",
            "module example.com/app\n\nrequire \"tako.sh\" v0.4.1\n\nreplace github.com/other/dep => ../dep\n",
            // A replacement pinned to another version does not apply.
            "module example.com/app\n\nrequire tako.sh v0.4.1\n\nreplace tako.sh v0.3.0 => ../tako\n",
        ] {
            let temp = TempDir::new().unwrap();
            write(temp.path(), "go.mod", go_mod);
            for runtime in ["go", "container"] {
                assert_eq!(
                    detect_app_sdk(runtime, temp.path(), temp.path()),
                    sdk("go", "tako.sh", "v0.4.1"),
                    "{go_mod}"
                );
            }
        }
    }

    #[test]
    fn go_records_what_is_knowable_about_replacements() {
        for (go_mod, expected) in [
            (
                "module app\n\nrequire tako.sh v0.0.0\n\nreplace tako.sh => ../../..\n",
                sdk("go", "tako.sh", "(devel)"),
            ),
            (
                "module app\n\nrequire tako.sh v0.4.1\n\nreplace (\n\ttako.sh v0.4.1 => /src/tako\n)\n",
                sdk("go", "tako.sh", "(devel)"),
            ),
            (
                "module app\n\nrequire tako.sh v0.4.1\n\nreplace tako.sh => example.com/fork/tako v0.5.0\n",
                sdk("go", "example.com/fork/tako", "v0.5.0"),
            ),
        ] {
            let temp = TempDir::new().unwrap();
            write(temp.path(), "go.mod", go_mod);
            assert_eq!(
                detect_app_sdk("go", temp.path(), temp.path()),
                expected,
                "{go_mod}"
            );
        }
    }

    #[test]
    fn go_without_the_sdk_module_is_unknown() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path(),
            "go.mod",
            "module app\n\nrequire github.com/tako-sh/tako/sdk/go v0.4.1\n\nreplace tako.sh => ../tako\n",
        );
        assert_eq!(detect_app_sdk("go", temp.path(), temp.path()), None);
    }

    #[test]
    fn rust_reads_the_locked_crate_version_from_any_source() {
        for (lock, expected) in [
            (
                "version = 4\n\n[[package]]\nname = \"axum\"\nversion = \"0.8.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"tako\"\nversion = \"0.2.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"abc\"\ndependencies = [\n \"axum\",\n]\n\n[[package]]\nname = \"tako-app\"\nversion = \"9.9.9\"\n",
                "0.2.1",
            ),
            // A path dependency is the last package and has no source.
            (
                "[[package]]\nname = \"app\"\nversion = \"1.0.0\"\ndependencies = [\n \"tako\",\n]\n\n[[package]]\nname = \"tako\"\nversion = \"0.3.0\"",
                "0.3.0",
            ),
        ] {
            let temp = TempDir::new().unwrap();
            let app = temp.path().join("services/api");
            write(&app, "Cargo.toml", "[package]\nname = \"app\"\n");
            write(temp.path(), "Cargo.lock", lock);
            assert_eq!(
                detect_app_sdk("container", &app, temp.path()),
                sdk("rust", "tako", expected)
            );
        }
    }

    #[test]
    fn rust_without_the_sdk_crate_is_unknown() {
        let temp = TempDir::new().unwrap();
        write(
            temp.path(),
            "Cargo.lock",
            "[[package]]\nname = \"tako-cli\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"axum\"\nversion = \"0.8.4\"\n",
        );
        assert_eq!(detect_app_sdk("container", temp.path(), temp.path()), None);
    }

    #[test]
    fn other_runtimes_prefer_a_backend_sdk_over_a_browser_package() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "package.json", APP_PACKAGE_JSON);
        write(
            temp.path(),
            "node_modules/tako.sh/package.json",
            r#"{"version":"1.4.2"}"#,
        );
        write(
            temp.path(),
            "Cargo.lock",
            "[[package]]\nname = \"tako\"\nversion = \"0.2.1\"\n",
        );
        assert_eq!(
            detect_app_sdk("container", temp.path(), temp.path()),
            sdk("rust", "tako", "0.2.1")
        );
        write(
            temp.path(),
            "go.mod",
            "module app\n\nrequire tako.sh v0.4.1\n",
        );
        assert_eq!(
            detect_app_sdk("unknown", temp.path(), temp.path()),
            sdk("go", "tako.sh", "v0.4.1")
        );
        // A native runtime never reports another language's SDK.
        assert_eq!(
            detect_app_sdk("bun", temp.path(), temp.path()),
            sdk("javascript", "tako.sh", "1.4.2")
        );
    }

    #[test]
    fn an_app_without_any_sdk_is_unknown() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "Dockerfile", "FROM nginx\n");
        for runtime in ["bun", "node", "go", "container", "unknown"] {
            assert_eq!(detect_app_sdk(runtime, temp.path(), temp.path()), None);
        }
    }
}
