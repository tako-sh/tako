use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DEFAULT_JS_APP_ROOT: &str = "src";

/// Root configuration from tako.toml
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Application name (required; stable identity for deploy paths and hostnames)
    pub name: Option<String>,

    /// Build runtime override used for default preset selection when `preset` is omitted.
    pub runtime: Option<String>,

    /// Pinned runtime version (for example: "1.2.3"). Used by deploy instead of auto-detecting.
    pub runtime_version: Option<String>,

    /// Package manager override (e.g. "npm", "pnpm", "yarn", "bun").
    /// Auto-detected from package.json `packageManager` field or lockfiles if omitted.
    pub package_manager: Option<String>,

    /// App preset reference (e.g. "tanstack-start"). Provides `main` and `assets` defaults.
    pub preset: Option<String>,

    /// Custom dev command override (e.g. `["vite", "dev"]`).
    #[serde(default)]
    pub dev: Vec<String>,

    /// JavaScript app root, relative to the config file.
    /// `tako.gen.ts`, `channels/`, and `workflows/` live under this directory.
    pub app_root: Option<String>,

    /// Runtime entrypoint override relative to project root
    pub main: Option<String>,

    /// Asset directories to include in the deploy artifact (e.g. ["dist/client"]).
    #[serde(default)]
    pub assets: Vec<String>,

    /// Build settings for deploy artifact generation.
    #[serde(default)]
    pub build: BuildConfig,

    /// Multi-stage build (mutually exclusive with `build.run`).
    #[serde(default)]
    pub build_stages: Vec<BuildStage>,

    /// [vars] section - global environment variables
    #[serde(default)]
    pub vars: HashMap<String, String>,

    /// [vars.*] sections - per-environment variables
    #[serde(default)]
    pub vars_per_env: HashMap<String, HashMap<String, String>>,

    /// [envs.*] sections - environment configurations
    #[serde(default)]
    pub envs: HashMap<String, EnvConfig>,

    /// Release command run once on the deploy leader server during the "Preparing"
    /// phase, before any rolling update starts (e.g. `"bun run db:migrate"`).
    /// Can be overridden per environment via `[envs.<name>].release`.
    pub release: Option<String>,

    /// [workflows] section - base workflow worker configuration and named
    /// worker-group overrides.
    #[serde(default)]
    pub workflows: WorkflowsConfig,

    /// [servers.*] sections - per-app-per-server configuration.
    #[serde(default)]
    pub servers: ServersConfig,
}

/// Backward-compatible alias.
pub type TakoToml = Config;

/// Build configuration from [build].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildConfig {
    /// Build command (e.g. "vinxi build", "bun run build").
    pub run: Option<String>,

    /// Optional pre-build install command (e.g. "bun install").
    pub install: Option<String>,

    /// Working directory for build commands, relative to the project root.
    pub cwd: Option<String>,

    /// Additional file globs to include in the deploy artifact.
    #[serde(default)]
    pub include: Vec<String>,

    /// File globs to exclude from the deploy artifact.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Custom build stage from [[build_stages]].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildStage {
    /// Optional display label.
    #[serde(default)]
    pub name: Option<String>,

    /// Optional working directory relative to tako.toml location.
    /// Allows ".." for monorepo traversal (guarded against escaping workspace root).
    #[serde(default)]
    pub cwd: Option<String>,

    /// Optional preparatory command run before `run`.
    #[serde(default)]
    pub install: Option<String>,

    /// Required stage command.
    pub run: String,

    /// File globs to exclude from the deploy artifact.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Environment configuration from [envs.*]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    /// Single route (mutually exclusive with routes)
    pub route: Option<String>,

    /// Multiple routes (mutually exclusive with route)
    pub routes: Option<Vec<String>>,

    /// Servers assigned to this environment.
    #[serde(default)]
    pub servers: Vec<String>,

    /// Idle timeout in seconds (300 = 5 minutes).
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u32,

    /// Per-environment release command override. An empty string explicitly
    /// clears the top-level `release` command for this environment.
    pub release: Option<String>,
}

pub(super) fn default_idle_timeout() -> u32 {
    300
}

fn default_workers() -> u32 {
    0
}

fn default_concurrency() -> u32 {
    10
}

/// [servers] section — per-server overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServersConfig {
    /// Per-server overrides. Keyed by server name. Populated from
    /// `[servers.<name>]`.
    #[serde(default, flatten)]
    pub per_server: HashMap<String, ServerConfig>,
}

/// Per-server configuration: `[servers.<name>]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Per-server workflows override.
    #[serde(default)]
    pub workflows: Option<WorkflowsConfig>,
}

/// Concrete workflow worker settings after inheritance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EffectiveWorkflowsConfig {
    /// Number of always-on worker processes. `0` means scale-to-zero:
    /// spawn on enqueue/cron tick, exit after `worker_idle_timeout`.
    #[serde(default = "default_workers")]
    pub workers: u32,

    /// Max parallel task slots per worker. Default `10`.
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
}

impl Default for EffectiveWorkflowsConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            concurrency: default_concurrency(),
        }
    }
}

/// Partial workflow worker settings from `[workflows]`,
/// `[workflows.<worker>]`, `[servers.<name>.workflows]`, and
/// `[servers.<name>.workflows.<worker>]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowWorkerConfig {
    /// Number of always-on worker processes. `0` means scale-to-zero.
    pub workers: Option<u32>,

    /// Max parallel task slots per worker.
    pub concurrency: Option<u32>,
}

impl WorkflowWorkerConfig {
    pub fn apply_to(&self, target: &mut EffectiveWorkflowsConfig) {
        if let Some(workers) = self.workers {
            target.workers = workers;
        }
        if let Some(concurrency) = self.concurrency {
            target.concurrency = concurrency;
        }
    }
}

/// Workflow worker configuration with a base config and named worker-group
/// overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowsConfig {
    pub base: WorkflowWorkerConfig,

    #[serde(default)]
    pub groups: HashMap<String, WorkflowWorkerConfig>,
}
