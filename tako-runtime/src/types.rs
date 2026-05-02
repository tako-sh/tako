use serde::{Deserialize, Serialize};

/// Top-level runtime definition loaded from a TOML file.
/// The `id` is derived from the filename, not stored in the TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeDef {
    #[serde(default)]
    pub id: String,
    pub language: String,

    #[serde(default)]
    pub entrypoint: EntrypointDef,

    #[serde(default)]
    pub preset: PresetDef,

    #[serde(default)]
    pub server: ServerDef,

    #[serde(default)]
    pub envs: EnvsDef,

    #[serde(default)]
    pub package_manager: PackageManagerDef,

    #[serde(default)]
    pub download: Option<DownloadDef>,
}

/// Package manager definition embedded in a runtime definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageManagerDef {
    /// Binary/runtime package manager id (for example: "bun" or "npm").
    #[serde(default)]
    pub id: String,

    /// Optional display name (e.g. "Bun", "npm", "pnpm").
    #[serde(default)]
    pub name: Option<String>,

    /// Lockfile names associated with this runtime lane (e.g. ["bun.lockb", "bun.lock"]).
    #[serde(default)]
    pub lockfiles: Vec<String>,

    /// Command template to add a dependency. `{package}` is replaced.
    #[serde(default)]
    pub add: Option<String>,

    /// Production dependency install script (shell). This is the default.
    #[serde(default)]
    pub install: Option<String>,

    /// Development/build-time overrides.
    #[serde(default)]
    pub development: Option<PackageManagerDevDef>,
}

/// Development/build-time package manager overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageManagerDevDef {
    /// Build-time dependency install script (shell). Includes dev dependencies.
    #[serde(default)]
    pub install: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntrypointDef {
    #[serde(default)]
    pub candidates: Vec<String>,

    /// Generic manifest file to check for a main entrypoint.
    #[serde(default)]
    pub manifest: Option<ManifestMainDef>,
}

/// Describes a manifest file + field to read the main entrypoint from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestMainDef {
    /// Manifest filename relative to project root (e.g. "package.json").
    pub file: String,
    /// Dot-separated field path within the manifest (e.g. "main").
    pub field: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresetDef {
    #[serde(default)]
    pub main: Option<String>,

    #[serde(default)]
    pub dev: Vec<String>,

    /// File patterns to watch for changes during `tako dev`. When non-empty,
    /// Tako watches these patterns and restarts on change (for runtimes
    /// without built-in watch mode).
    #[serde(default)]
    pub watch: Vec<String>,

    #[serde(default)]
    pub start: Vec<String>,

    /// Build command (shell script) run during `tako deploy`.
    #[serde(default)]
    pub build: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerDef {
    #[serde(default)]
    pub entrypoint_path: Option<String>,

    #[serde(default)]
    pub launch_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvsDef {
    /// Per-environment default variables. Merged under user's tako.toml vars.
    /// Keys are environment names (e.g. "production", "development").
    #[serde(flatten)]
    pub environments: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadDef {
    #[serde(default)]
    pub version_source: Option<VersionSourceDef>,

    #[serde(default)]
    pub url: Option<String>,

    #[serde(default)]
    pub format: Option<String>,

    #[serde(default)]
    pub checksum_url: Option<String>,

    #[serde(default)]
    pub checksum_format: Option<String>,

    #[serde(default)]
    pub os_map: std::collections::HashMap<String, String>,

    #[serde(default)]
    pub arch_map: std::collections::HashMap<String, String>,

    #[serde(default)]
    pub arch_variants: std::collections::HashMap<String, String>,

    #[serde(default)]
    pub extract: Option<ExtractDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionSourceDef {
    #[serde(rename = "type")]
    pub source_type: String,

    #[serde(default)]
    pub repo: Option<String>,

    #[serde(default)]
    pub tag_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractDef {
    #[serde(default)]
    pub binary: Option<String>,

    #[serde(default)]
    pub strip_components: Option<u32>,

    /// When true, extract all files from the archive (not just the binary).
    /// Used for runtimes like Node.js where npm/npx/corepack must also be
    /// extracted from the distribution tarball.
    #[serde(default)]
    pub all: bool,

    #[serde(default)]
    pub symlinks: Vec<SymlinkDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymlinkDef {
    pub name: String,
    pub target: String,
}
