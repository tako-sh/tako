use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One deployment group. Compose owns the service processes and shared resources.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ComposeConfig {
    /// Compose file, relative to the app directory and inside it.
    pub file: String,
    /// Metadata captured from services.*.x-tako, carried in release manifests.
    /// This field is not authored in tako.toml.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub services: BTreeMap<String, ComposeServiceConfig>,
}

/// Tako settings under services.<name>.x-tako. Container settings belong to Compose.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComposeServiceConfig {
    #[serde(default)]
    pub envs: BTreeMap<String, ComposeServiceEnvironment>,
    /// The service is an HTTP app even without routes (a private backend).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub http: bool,
    /// The image runs a Tako SDK and receives the Tako runtime.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sdk: bool,
}

/// How Tako treats an `x-tako` service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeServiceKind {
    /// Routes in any environment, or `http: true`: an app with a backend pool.
    Http,
    /// `sdk: true` without routes or `http`: an SDK worker or job.
    Worker,
}

impl ComposeServiceConfig {
    /// Routes in any environment, including development, make a service an
    /// HTTP app, as does `http: true`.
    pub fn is_http(&self) -> bool {
        self.http
            || self
                .envs
                .values()
                .any(|settings| !settings.routes().is_empty())
    }

    pub fn kind(&self) -> ComposeServiceKind {
        if self.is_http() {
            ComposeServiceKind::Http
        } else {
            ComposeServiceKind::Worker
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComposeServiceEnvironment {
    pub route: Option<String>,
    pub routes: Option<Vec<String>>,
    /// Inherits the group setting when omitted.
    pub source_ip: Option<crate::SourceIpMode>,
}

impl ComposeServiceEnvironment {
    pub fn routes(&self) -> Vec<String> {
        self.route
            .clone()
            .map(|r| vec![r])
            .or_else(|| self.routes.clone())
            .unwrap_or_default()
    }
}

impl ComposeConfig {
    pub fn validate(&self) -> Result<(), String> {
        let file = std::path::Path::new(&self.file);
        if self.file.is_empty()
            || file.is_absolute()
            || file
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("compose.file must be relative to the app directory without '..'".into());
        }
        let mut routes = BTreeMap::new();
        for (service, config) in &self.services {
            if service.is_empty()
                || !service
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            {
                return Err("Invalid Compose service name".into());
            }
            if !config.is_http() && !config.sdk {
                return Err(format!(
                    "Compose service '{service}': x-tako needs routes, `http: true` or `sdk: true`"
                ));
            }
            for (env, settings) in &config.envs {
                if settings.route.is_some() && settings.routes.is_some() {
                    return Err(format!(
                        "Compose service '{service}', environment '{env}': use route or routes, not both"
                    ));
                }
                for route in settings.routes() {
                    if route.is_empty()
                        || route.contains('$')
                        || route.chars().any(char::is_whitespace)
                    {
                        return Err(format!(
                            "Compose service '{service}': routes must be literal, nonempty host/path patterns"
                        ));
                    }
                    // Hostnames are case insensitive; URL paths are not. Exact
                    // paths match with or without trailing slashes in both proxies.
                    let (host, path) = route
                        .split_once('/')
                        .map_or((route.as_str(), None), |(host, path)| (host, Some(path)));
                    // `host` and `host/*` both match every path with equal
                    // specificity, so they are the same binding.
                    let path = path
                        .map(|path| path.trim_end_matches('/').to_owned())
                        .filter(|path| path != "*");
                    let key = (env, host.to_ascii_lowercase(), path);
                    if let Some(previous) = routes.insert(key, service) {
                        return Err(format!(
                            "Compose route '{route}' in '{env}' is assigned to both '{previous}' and '{service}'"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn routes(&self, environment: &str) -> Vec<String> {
        self.services
            .values()
            .filter_map(|service| service.envs.get(environment))
            .flat_map(ComposeServiceEnvironment::routes)
            .collect()
    }

    pub fn routed_services<'a>(
        &'a self,
        environment: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a ComposeServiceEnvironment)> {
        self.services.iter().filter_map(move |(name, service)| {
            service
                .envs
                .get(environment)
                .filter(|settings| !settings.routes().is_empty())
                .map(|settings| (name.as_str(), settings))
        })
    }

    /// HTTP apps, which own a backend pool and readiness check.
    pub fn http_services(&self) -> impl Iterator<Item = &str> {
        self.services
            .iter()
            .filter(|(_, service)| service.is_http())
            .map(|(name, _)| name.as_str())
    }

    pub fn is_http(&self, service: &str) -> bool {
        self.services
            .get(service)
            .is_some_and(ComposeServiceConfig::is_http)
    }

    pub fn is_sdk(&self, service: &str) -> bool {
        self.services
            .get(service)
            .is_some_and(|service| service.sdk)
    }

    /// Whether any service receives the Tako SDK runtime.
    pub fn has_sdk_services(&self) -> bool {
        self.services.values().any(|service| service.sdk)
    }

    /// Local HTTP apps get a stable hostname even without a public production
    /// route. Workers never receive one.
    pub fn for_development(&self, default_host: &str, base_host: Option<&str>) -> Self {
        let mut config = self.clone();
        for (service, settings) in &mut config.services {
            if !settings.is_http() {
                continue;
            }
            let development = settings.envs.entry("development".into()).or_default();
            let mut routes = development.routes();
            if routes.is_empty() {
                routes.push(format!(
                    "{}.{}",
                    service.to_ascii_lowercase().replace('_', "-"),
                    default_host
                ));
            } else if let Some(base) = base_host {
                for route in &mut routes {
                    *route = route.replace(base, default_host);
                }
            }
            development.route = None;
            development.routes = Some(routes);
        }
        config
    }

    /// For a single HTTP app, commands may omit --service. Never guess in a group.
    pub fn default_service(&self) -> Result<&str, String> {
        let mut services = self.http_services();
        let first = services
            .next()
            .ok_or("Compose group has no HTTP app; specify --service")?;
        if services.next().is_some() {
            return Err("Compose group has multiple HTTP apps; specify --service".into());
        }
        Ok(first)
    }
}
