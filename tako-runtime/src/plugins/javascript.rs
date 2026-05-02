use std::path::{Path, PathBuf};

use crate::plugin::{PluginContext, RuntimePlugin};
use crate::types::{
    DownloadDef, EntrypointDef, EnvsDef, ExtractDef, ManifestMainDef, PackageManagerDef,
    PackageManagerDevDef, PresetDef, RuntimeDef, ServerDef, SymlinkDef, VersionSourceDef,
};

// ── Shared JS constants ────────────────────────────────────────────

fn js_entrypoint_candidates() -> Vec<String> {
    [
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "index.mts",
        "index.mjs",
        "src/index.ts",
        "src/index.tsx",
        "src/index.js",
        "src/index.jsx",
        "src/index.mts",
        "src/index.mjs",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

fn js_manifest_main() -> Option<ManifestMainDef> {
    Some(ManifestMainDef {
        file: "package.json".to_string(),
        field: "main".to_string(),
    })
}

fn js_production_envs() -> EnvsDef {
    let mut envs = std::collections::HashMap::new();
    envs.insert(
        "production".to_string(),
        [("NODE_ENV".to_string(), "production".to_string())]
            .into_iter()
            .collect(),
    );
    envs.insert(
        "development".to_string(),
        [("NODE_ENV".to_string(), "development".to_string())]
            .into_iter()
            .collect(),
    );
    EnvsDef { environments: envs }
}

// ── Package manager detection ──────────────────────────────────────

/// Detected package manager with optional version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageManager {
    Bun,
    Npm,
    Pnpm,
    Yarn,
}

impl PackageManager {
    pub fn id(&self) -> &'static str {
        match self {
            PackageManager::Bun => "bun",
            PackageManager::Npm => "npm",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn => "yarn",
        }
    }
}

/// Read the raw `packageManager` field from package.json (e.g. `"pnpm@9.1.0"`).
pub fn read_package_manager_spec(project_dir: &Path) -> Option<String> {
    let pkg_path = project_dir.join("package.json");
    let content = std::fs::read_to_string(pkg_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("packageManager")?.as_str().map(str::to_string)
}

/// Walk up from `project_dir` to find the JS workspace/project root.
///
/// Returns the nearest ancestor directory (inclusive) that contains a lockfile.
/// Falls back to `project_dir` if no lockfile is found anywhere in the tree.
pub fn find_js_project_root(project_dir: &Path) -> PathBuf {
    let lockfile_names = [
        "bun.lock",
        "bun.lockb",
        "pnpm-lock.yaml",
        "yarn.lock",
        "package-lock.json",
    ];
    let mut current = project_dir;
    loop {
        for name in &lockfile_names {
            if current.join(name).is_file() {
                return current.to_path_buf();
            }
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return project_dir.to_path_buf(),
        }
    }
}

/// Detect the package manager for a JavaScript project.
///
/// Priority:
/// 1. `packageManager` field in package.json (e.g. `"pnpm@9.0.0"`)
/// 2. Lockfile presence (bun.lock, pnpm-lock.yaml, yarn.lock, package-lock.json)
/// 3. None (caller picks the default)
pub fn detect_package_manager(project_dir: &Path) -> Option<PackageManager> {
    // 1. Check packageManager field in package.json
    if let Some(pm) = read_package_manager_field(project_dir) {
        return Some(pm);
    }

    // 2. Check lockfiles
    if project_dir.join("bun.lockb").is_file() || project_dir.join("bun.lock").is_file() {
        return Some(PackageManager::Bun);
    }
    if project_dir.join("pnpm-lock.yaml").is_file() {
        return Some(PackageManager::Pnpm);
    }
    if project_dir.join("yarn.lock").is_file() {
        return Some(PackageManager::Yarn);
    }
    if project_dir.join("package-lock.json").is_file() {
        return Some(PackageManager::Npm);
    }
    None
}

fn read_package_manager_field(project_dir: &Path) -> Option<PackageManager> {
    let pkg_path = project_dir.join("package.json");
    let content = std::fs::read_to_string(pkg_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let pm_field = json.get("packageManager")?.as_str()?;
    // Format: "pnpm@9.0.0" or just "pnpm"
    let name = pm_field.split('@').next()?;
    match name {
        "bun" => Some(PackageManager::Bun),
        "npm" => Some(PackageManager::Npm),
        "pnpm" => Some(PackageManager::Pnpm),
        "yarn" => Some(PackageManager::Yarn),
        _ => None,
    }
}

/// Resolve the effective package manager for a context.
fn resolve_pm(ctx: &PluginContext, default: PackageManager) -> PackageManager {
    // Explicit override from tako.toml or manifest (may include version like "pnpm@9.1.0")
    if let Some(pm_str) = ctx.package_manager {
        let name = pm_str.split_once('@').map_or(pm_str, |(name, _)| name);
        match name {
            "bun" => return PackageManager::Bun,
            "npm" => return PackageManager::Npm,
            "pnpm" => return PackageManager::Pnpm,
            "yarn" => return PackageManager::Yarn,
            _ => {}
        }
    }

    // Auto-detect from project
    detect_package_manager(ctx.project_dir).unwrap_or(default)
}

// ── Package manager → commands ─────────────────────────────────────

fn pm_install_production(pm: &PackageManager) -> String {
    match pm {
        PackageManager::Bun => "bun install --production".to_string(),
        PackageManager::Npm => {
            "npm ci --omit=dev 2>/dev/null || npm install --omit=dev".to_string()
        }
        PackageManager::Pnpm => {
            // npm ships with node (full distribution extracted). Install pnpm if needed.
            // Falls back gracefully if workspace:* deps can't resolve on server
            // (the build artifact includes node_modules from the local build).
            "command -v pnpm >/dev/null 2>&1 || npm install -g pnpm 2>/dev/null; pnpm install --prod 2>/dev/null || true"
                .to_string()
        }
        PackageManager::Yarn => {
            "command -v yarn >/dev/null 2>&1 || npm install -g yarn 2>/dev/null; yarn install --production 2>/dev/null || true"
                .to_string()
        }
    }
}

fn pm_install_dev(pm: &PackageManager) -> String {
    match pm {
        PackageManager::Bun => "bun install".to_string(),
        PackageManager::Npm => "npm install".to_string(),
        PackageManager::Pnpm => "pnpm install".to_string(),
        PackageManager::Yarn => "yarn install".to_string(),
    }
}

fn pm_add_command(pm: &PackageManager) -> String {
    match pm {
        PackageManager::Bun => "bun add {package}".to_string(),
        PackageManager::Npm => "npm install {package}".to_string(),
        PackageManager::Pnpm => "pnpm add {package}".to_string(),
        PackageManager::Yarn => "yarn add {package}".to_string(),
    }
}

fn pm_run_command(pm: &PackageManager) -> &'static str {
    match pm {
        PackageManager::Bun => "bun run",
        PackageManager::Npm => "npm run",
        PackageManager::Pnpm => "pnpm run",
        PackageManager::Yarn => "yarn run",
    }
}

fn pm_lockfiles(pm: &PackageManager) -> Vec<String> {
    match pm {
        PackageManager::Bun => vec!["bun.lockb".to_string(), "bun.lock".to_string()],
        PackageManager::Npm => vec!["package-lock.json".to_string()],
        PackageManager::Pnpm => vec!["pnpm-lock.yaml".to_string()],
        PackageManager::Yarn => vec!["yarn.lock".to_string()],
    }
}

fn pm_build_command(pm: &PackageManager) -> String {
    let run = pm_run_command(pm);
    match pm {
        PackageManager::Bun | PackageManager::Npm | PackageManager::Pnpm | PackageManager::Yarn => {
            format!("{run} --if-present build")
        }
    }
}

/// Default dev command for JS runtimes: run through the SDK HTTP
/// entrypoint, same as production. The workflow worker is a separate
/// subprocess spawned by `tako-dev-server` via `WorkerSupervisor`
/// (scale-to-zero, short idle). Framework presets (vite, tanstack-start)
/// override this with their own dev server.
fn js_dev_command_bun() -> Vec<String> {
    vec![
        "bun".to_string(),
        "run".to_string(),
        "node_modules/tako.sh/dist/entrypoints/bun-server.mjs".to_string(),
        "{main}".to_string(),
    ]
}

fn js_dev_command_node() -> Vec<String> {
    vec![
        "node".to_string(),
        "--experimental-strip-types".to_string(),
        "node_modules/tako.sh/dist/entrypoints/node-server.mjs".to_string(),
        "{main}".to_string(),
    ]
}

fn build_package_manager_def(pm: &PackageManager) -> PackageManagerDef {
    PackageManagerDef {
        id: pm.id().to_string(),
        name: None,
        lockfiles: pm_lockfiles(pm),
        add: Some(pm_add_command(pm)),
        install: Some(pm_install_production(pm)),
        development: Some(PackageManagerDevDef {
            install: Some(pm_install_dev(pm)),
        }),
    }
}

// ── Download config (hardcoded) ─────────────────────────────────────

fn download_def_for(id: &str) -> Option<DownloadDef> {
    use std::collections::HashMap;

    match id {
        "bun" => Some(DownloadDef {
            version_source: Some(VersionSourceDef {
                source_type: "github_releases".into(),
                repo: Some("oven-sh/bun".into()),
                tag_prefix: Some("bun-v".into()),
            }),
            url: Some("https://github.com/oven-sh/bun/releases/download/bun-v{version}/bun-{os}-{arch}.zip".into()),
            format: Some("zip".into()),
            checksum_url: Some("https://github.com/oven-sh/bun/releases/download/bun-v{version}/SHASUMS256.txt".into()),
            checksum_format: Some("shasums".into()),
            os_map: HashMap::from([("macos".into(), "darwin".into()), ("linux".into(), "linux".into())]),
            arch_map: HashMap::from([("x64".into(), "x64".into()), ("arm64".into(), "aarch64".into())]),
            arch_variants: HashMap::from([("x64-musl".into(), "x64-musl".into()), ("arm64-musl".into(), "aarch64-musl".into())]),
            extract: Some(ExtractDef {
                binary: Some("bun-{os}-{arch}/bun".into()),
                strip_components: Some(0),
                all: false,
                symlinks: vec![SymlinkDef { name: "bunx".into(), target: "./bun".into() }],
            }),
        }),
        "node" => Some(DownloadDef {
            version_source: Some(VersionSourceDef {
                source_type: "github_releases".into(),
                repo: Some("nodejs/node".into()),
                tag_prefix: Some("v".into()),
            }),
            url: Some("https://nodejs.org/dist/v{version}/node-v{version}-{os}-{arch}.tar.gz".into()),
            format: Some("tar.gz".into()),
            checksum_url: Some("https://nodejs.org/dist/v{version}/SHASUMS256.txt".into()),
            checksum_format: Some("shasums".into()),
            os_map: HashMap::from([("macos".into(), "darwin".into()), ("linux".into(), "linux".into())]),
            arch_map: HashMap::from([("x64".into(), "x64".into()), ("arm64".into(), "arm64".into())]),
            arch_variants: HashMap::new(),
            extract: Some(ExtractDef {
                binary: Some("bin/node".into()),
                // Extract the full node distribution (bin/node, bin/npm, lib/node_modules/...)
                // so npm and corepack are available for installing package managers.
                strip_components: Some(1),
                all: true,
                symlinks: vec![],
            }),
        }),
        _ => None,
    }
}

// ── Bun Plugin ─────────────────────────────────────────────────────

pub struct BunPlugin;

impl BunPlugin {
    fn build_def(&self, pm: &PackageManager) -> RuntimeDef {
        let mut envs = js_production_envs();
        envs.environments
            .entry("production".to_string())
            .or_default()
            .insert("BUN_ENV".to_string(), "production".to_string());
        envs.environments
            .entry("development".to_string())
            .or_default()
            .insert("BUN_ENV".to_string(), "development".to_string());

        RuntimeDef {
            id: "bun".to_string(),
            language: "javascript".to_string(),
            entrypoint: EntrypointDef {
                candidates: js_entrypoint_candidates(),
                manifest: js_manifest_main(),
            },
            preset: PresetDef {
                main: Some("src/index.ts".to_string()),
                dev: js_dev_command_bun(),
                watch: vec![],
                start: vec![
                    "{bin}".to_string(),
                    "run".to_string(),
                    "node_modules/tako.sh/dist/entrypoints/bun-server.mjs".to_string(),
                    "{main}".to_string(),
                ],
                build: Some(pm_build_command(pm)),
            },
            server: ServerDef {
                entrypoint_path: None,
                launch_args: vec![
                    "{bin}".to_string(),
                    "run".to_string(),
                    "node_modules/tako.sh/dist/entrypoints/bun-server.mjs".to_string(),
                    "{main}".to_string(),
                ],
            },
            envs,
            package_manager: build_package_manager_def(pm),
            download: download_def_for("bun"),
        }
    }
}

impl RuntimePlugin for BunPlugin {
    fn id(&self) -> &'static str {
        "bun"
    }

    fn language(&self) -> &'static str {
        "javascript"
    }

    fn runtime_def(&self, ctx: &PluginContext) -> RuntimeDef {
        let pm = resolve_pm(ctx, PackageManager::Bun);
        self.build_def(&pm)
    }

    fn default_runtime_def(&self) -> RuntimeDef {
        self.build_def(&PackageManager::Bun)
    }
}

// ── Node Plugin ────────────────────────────────────────────────────

pub struct NodePlugin;

impl NodePlugin {
    fn build_def(&self, pm: &PackageManager) -> RuntimeDef {
        RuntimeDef {
            id: "node".to_string(),
            language: "javascript".to_string(),
            entrypoint: EntrypointDef {
                candidates: js_entrypoint_candidates(),
                manifest: js_manifest_main(),
            },
            preset: PresetDef {
                main: Some("index.js".to_string()),
                dev: js_dev_command_node(),
                watch: vec![
                    "**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx", "**/*.mts", "**/*.mjs",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                start: vec![
                    "{bin}".to_string(),
                    "--experimental-strip-types".to_string(),
                    "node_modules/tako.sh/dist/entrypoints/node-server.mjs".to_string(),
                    "{main}".to_string(),
                ],
                build: Some(pm_build_command(pm)),
            },
            server: ServerDef {
                entrypoint_path: None,
                launch_args: vec![
                    "{bin}".to_string(),
                    "--experimental-strip-types".to_string(),
                    "node_modules/tako.sh/dist/entrypoints/node-server.mjs".to_string(),
                    "{main}".to_string(),
                ],
            },
            envs: js_production_envs(),
            package_manager: build_package_manager_def(pm),
            download: download_def_for("node"),
        }
    }
}

impl RuntimePlugin for NodePlugin {
    fn id(&self) -> &'static str {
        "node"
    }

    fn language(&self) -> &'static str {
        "javascript"
    }

    fn runtime_def(&self, ctx: &PluginContext) -> RuntimeDef {
        let pm = resolve_pm(ctx, PackageManager::Pnpm);
        self.build_def(&pm)
    }

    fn default_runtime_def(&self) -> RuntimeDef {
        self.build_def(&PackageManager::Pnpm)
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_ctx(dir: &Path) -> PluginContext<'_> {
        PluginContext {
            project_dir: dir,
            package_manager: None,
        }
    }

    #[test]
    fn bun_plugin_default_uses_bun_pm() {
        let def = BunPlugin.default_runtime_def();
        assert_eq!(def.package_manager.id, "bun");
        assert_eq!(
            def.package_manager.install.as_deref(),
            Some("bun install --production")
        );
    }

    #[test]
    fn node_plugin_default_uses_pnpm() {
        let def = NodePlugin.default_runtime_def();
        assert_eq!(def.package_manager.id, "pnpm");
        let install = def.package_manager.install.as_deref().unwrap();
        assert!(install.contains("pnpm"), "install should mention pnpm");
    }

    #[test]
    fn node_plugin_detects_npm_from_lockfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        let ctx = default_ctx(tmp.path());
        let def = NodePlugin.runtime_def(&ctx);
        assert_eq!(def.package_manager.id, "npm");
        assert!(
            def.package_manager
                .install
                .as_deref()
                .unwrap()
                .contains("npm")
        );
    }

    #[test]
    fn node_plugin_detects_yarn_from_lockfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("yarn.lock"), "").unwrap();
        let ctx = default_ctx(tmp.path());
        let def = NodePlugin.runtime_def(&ctx);
        assert_eq!(def.package_manager.id, "yarn");
    }

    #[test]
    fn node_plugin_respects_package_manager_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Lockfile says npm, but override says yarn
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        let ctx = PluginContext {
            project_dir: tmp.path(),
            package_manager: Some("yarn"),
        };
        let def = NodePlugin.runtime_def(&ctx);
        assert_eq!(def.package_manager.id, "yarn");
    }

    #[test]
    fn detect_pm_reads_package_manager_field() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"packageManager": "pnpm@9.1.0"}"#,
        )
        .unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Pnpm)
        );
    }

    #[test]
    fn detect_pm_falls_back_to_lockfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bun.lock"), "").unwrap();
        assert_eq!(
            detect_package_manager(tmp.path()),
            Some(PackageManager::Bun)
        );
    }

    #[test]
    fn detect_pm_returns_none_when_no_signals() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(detect_package_manager(tmp.path()), None);
    }

    #[test]
    fn find_js_project_root_returns_project_dir_when_lockfile_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bun.lock"), "").unwrap();
        assert_eq!(find_js_project_root(tmp.path()), tmp.path());
    }

    #[test]
    fn find_js_project_root_walks_up_to_lockfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("monorepo");
        let app_dir = root.join("apps/web");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(root.join("bun.lock"), "").unwrap();
        assert_eq!(find_js_project_root(&app_dir), root);
    }

    #[test]
    fn find_js_project_root_falls_back_to_project_dir_when_no_lockfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        assert_eq!(find_js_project_root(&app_dir), app_dir);
    }

    #[test]
    fn find_js_project_root_finds_nearest_not_highest_lockfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let sub = root.join("sub");
        let app_dir = sub.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(root.join("bun.lock"), "").unwrap();
        std::fs::write(sub.join("bun.lock"), "").unwrap();
        // Finds the nearest (innermost) lockfile ancestor
        assert_eq!(find_js_project_root(&app_dir), sub);
    }

    #[test]
    fn all_plugins_have_download_def() {
        assert!(BunPlugin.default_runtime_def().download.is_some());
        assert!(NodePlugin.default_runtime_def().download.is_some());
    }

    #[test]
    fn all_plugins_have_main_placeholder_in_launch_args() {
        for def in [
            BunPlugin.default_runtime_def(),
            NodePlugin.default_runtime_def(),
        ] {
            assert!(
                def.server.launch_args.contains(&"{main}".to_string()),
                "plugin {} launch_args should contain {{main}}",
                def.id
            );
        }
    }

    #[test]
    fn runtime_def_for_uses_plugin() {
        // runtime_def_for(id, None) should return the plugin's default_runtime_def
        for id in ["bun", "node"] {
            let def = crate::runtime_def_for(id, None).unwrap();
            let plugin = crate::plugin::plugin_for_id(id).unwrap();
            let plugin_def = plugin.default_runtime_def();

            assert_eq!(def.id, plugin_def.id, "{id}: id");
            assert_eq!(def.preset.start, plugin_def.preset.start, "{id}: start");
            assert_eq!(
                def.package_manager.id, plugin_def.package_manager.id,
                "{id}: pm"
            );
            assert!(def.download.is_some(), "{id}: download from TOML");
        }
    }
}
