use super::schema::*;
use crate::config::BUILTIN_LOCAL_STORAGE_RESOURCE_NAME;
use std::collections::HashMap;
use std::path::Path;

const RESERVED_DERIVED_ENV_VARS: &[&str] = &["ENV"];

impl Config {
    /// JavaScript app root relative to the config file.
    ///
    /// Tako discovers `channels/` and `workflows/` inside it.
    pub fn js_app_root(&self) -> &str {
        self.app_root
            .as_deref()
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .unwrap_or(DEFAULT_JS_APP_ROOT)
    }

    /// Return the effective workflows config for unnamed workflows on a
    /// given server.
    ///
    /// Precedence: `[servers.<name>.workflows]` > `[workflows]` > built-in
    /// defaults (`workers = 0`, `concurrency = 10`).
    pub fn workflows_for_server(&self, name: &str) -> EffectiveWorkflowsConfig {
        self.workflows_for_server_worker(name, None)
    }

    /// Return the effective workflows config for an optional named worker
    /// group on a given server.
    ///
    /// Precedence for `worker = "email"`:
    /// built-in defaults < `[workflows]` < `[workflows.email]` <
    /// `[servers.<name>.workflows]` < `[servers.<name>.workflows.email]`.
    pub fn workflows_for_server_worker(
        &self,
        server_name: &str,
        worker: Option<&str>,
    ) -> EffectiveWorkflowsConfig {
        let mut effective = EffectiveWorkflowsConfig::default();
        self.workflows.base.apply_to(&mut effective);
        if let Some(worker) = worker
            && let Some(group) = self.workflows.groups.get(worker)
        {
            group.apply_to(&mut effective);
        }

        if let Some(server) = self.servers.per_server.get(server_name)
            && let Some(workflows) = &server.workflows
        {
            workflows.base.apply_to(&mut effective);
            if let Some(worker) = worker
                && let Some(group) = workflows.groups.get(worker)
            {
                group.apply_to(&mut effective);
            }
        }

        effective
    }

    /// Get servers for a specific environment
    pub fn get_servers_for_env(&self, env_name: &str) -> Vec<&str> {
        self.envs
            .get(env_name)
            .map(|env| env.servers.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn storage_resource_for_env(
        &self,
        env_name: &str,
        resource_name: &str,
    ) -> Option<StorageResourceConfig> {
        if resource_name == BUILTIN_LOCAL_STORAGE_RESOURCE_NAME {
            return Some(StorageResourceConfig::local());
        }
        if let Some(resource) = self.storages.get(resource_name) {
            return Some(resource.clone());
        }
        if env_name == "development" {
            return Some(StorageResourceConfig::local());
        }
        None
    }

    /// Get effective idle timeout for an environment.
    pub fn get_idle_timeout(&self, env_name: &str) -> u32 {
        self.envs
            .get(env_name)
            .map(|env| env.idle_timeout)
            .unwrap_or_else(default_idle_timeout)
    }

    /// Get effective client source-IP mode for an environment.
    pub fn get_source_ip_mode(&self, env_name: &str) -> tako_core::SourceIpMode {
        self.envs
            .get(env_name)
            .and_then(|env| env.source_ip)
            .unwrap_or_default()
    }

    /// Get effective SSL provider for an environment.
    pub fn get_ssl_provider(&self, env_name: &str) -> tako_core::SslProvider {
        self.envs
            .get(env_name)
            .map(|env| env.ssl)
            .unwrap_or_default()
    }

    /// Get merged vars for an environment (global + per-env)
    pub fn get_merged_vars(&self, env_name: &str) -> HashMap<String, String> {
        let mut merged = self.vars.clone();
        if let Some(env_vars) = self.vars_per_env.get(env_name) {
            merged.extend(env_vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        for reserved in RESERVED_DERIVED_ENV_VARS {
            merged.remove(*reserved);
        }
        merged
    }

    pub fn ignored_reserved_var_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for reserved in RESERVED_DERIVED_ENV_VARS {
            if self.vars.contains_key(*reserved) {
                warnings.push(format!(
                    "[vars].{reserved} is ignored. Tako derives {reserved} automatically."
                ));
            }

            for env_name in self.vars_per_env.keys() {
                if self
                    .vars_per_env
                    .get(env_name)
                    .is_some_and(|vars| vars.contains_key(*reserved))
                {
                    warnings.push(format!(
                        "[vars.{env_name}].{reserved} is ignored. Tako derives {reserved} automatically."
                    ));
                }
            }
        }

        warnings
    }

    /// Check if tako.toml exists in a directory
    pub fn exists_in_dir<P: AsRef<Path>>(dir: P) -> bool {
        dir.as_ref().join("tako.toml").exists()
    }

    /// Check if a config file exists at an explicit path.
    pub fn exists_in_file<P: AsRef<Path>>(path: P) -> bool {
        path.as_ref().is_file()
    }

    /// Get routes for an environment
    pub fn get_routes(&self, env_name: &str) -> Option<Vec<String>> {
        self.envs.get(env_name).and_then(|env| {
            if let Some(route) = &env.route {
                Some(vec![route.clone()])
            } else {
                env.routes.clone()
            }
        })
    }

    /// Get all environment names
    pub fn get_environment_names(&self) -> Vec<String> {
        self.envs.keys().cloned().collect()
    }
}
