//! Server-side routing: match incoming requests (Host + path) to an app.
//!
//! This is intentionally pure logic (no Pingora types) to keep it easy to test.

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    pub app: String,
    pub pattern: String,
    pub source_ip: tako_core::SourceIpMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRouteEntry {
    pub app: Arc<str>,
    pub host: Arc<str>,
    pub service: Option<Arc<str>>,
    pub path: Option<Arc<str>>,
    pub source_ip: tako_core::SourceIpMode,
    pub specificity: (u8, usize, u8),
}

#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    app_routes: std::collections::HashMap<String, Vec<String>>,
    app_source_ip: std::collections::HashMap<String, tako_core::SourceIpMode>,
    compiled: Vec<CompiledRouteEntry>,
    services: std::collections::HashMap<(String, String), (String, tako_core::SourceIpMode)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRoute {
    pub app: Arc<str>,
    pub service: Option<Arc<str>>,
    pub path: Option<Arc<str>>,
    pub source_ip: tako_core::SourceIpMode,
}

impl RouteTable {
    /// The first of `routes` that another app environment already claims,
    /// with that app environment.
    pub fn claimed_elsewhere<'a>(
        &self,
        app: &str,
        routes: &'a [String],
    ) -> Option<(&'a str, &str)> {
        routes.iter().find_map(|route| {
            self.app_routes
                .iter()
                .filter(|(owner, _)| owner.as_str() != app)
                .find(|(_, claimed)| claimed.iter().any(|other| same_route(other, route)))
                .map(|(owner, _)| (route.as_str(), owner.as_str()))
        })
    }

    pub fn set_app_routes(&mut self, app: String, routes: Vec<String>) {
        self.set_app_routes_with_source_ip(app, routes, tako_core::SourceIpMode::Auto);
    }

    pub fn set_app_routes_with_source_ip(
        &mut self,
        app: String,
        routes: Vec<String>,
        source_ip: tako_core::SourceIpMode,
    ) {
        self.services.retain(|(owner, _), _| owner != &app);
        self.app_source_ip.insert(app.clone(), source_ip);
        self.app_routes.insert(app, routes);
        self.rebuild();
    }

    pub(crate) fn set_config_routes(
        &mut self,
        config: &crate::instances::AppConfig,
        routes: Vec<String>,
    ) {
        let app = config.deployment_id();
        self.services.retain(|(owner, _), _| owner != &app);
        self.app_source_ip.insert(app.clone(), config.source_ip);
        self.app_routes.insert(app.clone(), routes);
        if let Some(compose) = &config.compose {
            for (service, settings) in compose.routed_services(&config.environment) {
                for route in settings.routes() {
                    self.services.insert(
                        (app.clone(), route),
                        (
                            service.into(),
                            settings.source_ip.unwrap_or(config.source_ip),
                        ),
                    );
                }
            }
        }
        self.rebuild();
    }

    pub fn remove_app_routes(&mut self, app: &str) {
        self.services.retain(|(owner, _), _| owner != app);
        self.app_routes.remove(app);
        self.app_source_ip.remove(app);
        self.rebuild();
    }

    pub fn routes_for_app(&self, app: &str) -> Vec<String> {
        self.app_routes.get(app).cloned().unwrap_or_default()
    }

    pub fn needs_cloudflare_ip_ranges(&self) -> bool {
        self.compiled.iter().any(|entry| {
            matches!(
                entry.source_ip,
                tako_core::SourceIpMode::Auto | tako_core::SourceIpMode::CloudflareProxy
            )
        })
    }

    pub fn app_for_route_domain(&self, domain: &str) -> Option<String> {
        self.app_routes.iter().find_map(|(app, routes)| {
            routes
                .iter()
                .any(|route| split_route(route).0.eq_ignore_ascii_case(domain))
                .then(|| app.clone())
        })
    }

    pub fn select(&self, host: &str, path: &str) -> Option<String> {
        self.select_with_route(host, path)
            .map(|selected| selected.app.to_string())
    }

    pub fn select_with_route(&self, host: &str, path: &str) -> Option<SelectedRoute> {
        select_route_for_request_compiled(&self.compiled, host, path)
    }

    fn rebuild(&mut self) {
        let mut entries = Vec::new();

        for (app, patterns) in &self.app_routes {
            let source_ip = self.app_source_ip.get(app).copied().unwrap_or_default();
            for pattern in patterns {
                if pattern.is_empty() {
                    continue;
                }
                entries.push(RouteEntry {
                    app: app.clone(),
                    pattern: pattern.clone(),
                    source_ip,
                });
            }
        }

        self.compiled = compile_routes(&entries);
        for entry in &mut self.compiled {
            let pattern = format!("{}{}", entry.host, entry.path.as_deref().unwrap_or(""));
            if let Some((service, source_ip)) = self.services.get(&(entry.app.to_string(), pattern))
            {
                entry.service = Some(Arc::from(service.as_str()));
                entry.source_ip = *source_ip;
            }
        }
    }
}

pub fn compile_routes(routes: &[RouteEntry]) -> Vec<CompiledRouteEntry> {
    let mut compiled = Vec::with_capacity(routes.len());
    for entry in routes {
        if entry.pattern.is_empty() {
            continue;
        }

        let (pattern_host, pattern_path) = split_route(&entry.pattern);
        compiled.push(CompiledRouteEntry {
            app: Arc::from(entry.app.as_str()),
            host: Arc::from(pattern_host),
            service: None,
            path: pattern_path.map(Arc::from),
            source_ip: entry.source_ip,
            specificity: route_specificity(&entry.pattern),
        });
    }

    // Most specific first. Keep stable order for ties.
    compiled.sort_by_key(|entry| std::cmp::Reverse(entry.specificity));
    compiled
}

#[cfg(test)]
pub fn select_app_for_request_compiled(
    routes: &[CompiledRouteEntry],
    host: &str,
    path: &str,
) -> Option<String> {
    select_route_for_request_compiled(routes, host, path).map(|selected| selected.app.to_string())
}

pub fn select_route_for_request_compiled(
    routes: &[CompiledRouteEntry],
    host: &str,
    path: &str,
) -> Option<SelectedRoute> {
    for entry in routes {
        if !hostname_matches(&entry.host, host) {
            continue;
        }
        if let Some(p) = &entry.path
            && !path_matches(p, path)
        {
            continue;
        }
        return Some(SelectedRoute {
            app: entry.app.clone(),
            service: entry.service.clone(),
            path: entry.path.clone(),
            source_ip: entry.source_ip,
        });
    }
    None
}

/// Select the best matching app for a request (uncompiled reference implementation, tests only).
#[cfg(test)]
fn select_app_for_request(routes: &[RouteEntry], host: &str, path: &str) -> Option<String> {
    let mut best: Option<(&RouteEntry, (u8, usize, u8))> = None;

    for entry in routes {
        if !route_matches(&entry.pattern, host, path) {
            continue;
        }

        let spec = route_specificity(&entry.pattern);
        match best {
            None => best = Some((entry, spec)),
            Some((_, best_spec)) => {
                if spec > best_spec {
                    best = Some((entry, spec));
                }
            }
        }
    }

    best.map(|(e, _)| e.app.clone())
}

#[cfg(test)]
fn route_matches(pattern: &str, host: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let (pattern_host, pattern_path) = split_route(pattern);
    if !hostname_matches(pattern_host, host) {
        return false;
    }
    match pattern_path {
        None => true,
        Some(p) => path_matches(p, path),
    }
}

fn route_specificity(pattern: &str) -> (u8, usize, u8) {
    if pattern.is_empty() {
        return (0, 0, 0);
    }
    let (pattern_host, pattern_path) = split_route(pattern);

    let host_score: u8 = if pattern_host.starts_with("*.") { 1 } else { 2 };

    let (path_len, exact_bonus) = match pattern_path {
        None => (0, 0),
        Some(p) => {
            if let Some(prefix) = p.strip_suffix("/*") {
                (prefix.len(), 0)
            } else if p.ends_with('*') {
                let prefix = &p[..p.len().saturating_sub(1)];
                (prefix.len(), 0)
            } else {
                (normalize_exact_path(p).len(), 1)
            }
        }
    };

    (host_score, path_len, exact_bonus)
}

/// Whether two route patterns claim the same requests. Hosts compare without
/// case, as requests match them; paths compare exactly.
fn same_route(a: &str, b: &str) -> bool {
    let ((a_host, a_path), (b_host, b_path)) = (split_route(a), split_route(b));
    a_host.eq_ignore_ascii_case(b_host) && a_path == b_path
}

fn split_route(route: &str) -> (&str, Option<&str>) {
    match route.find('/') {
        Some(idx) => (&route[..idx], Some(&route[idx..])),
        None => (route, None),
    }
}

fn hostname_matches(pattern: &str, hostname: &str) -> bool {
    // RFC 7230 §2.7.1: host is case-insensitive
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // *.example.com should not match example.com
        if hostname.eq_ignore_ascii_case(suffix) {
            return false;
        }
        // Check hostname ends with ".{suffix}" without allocating
        hostname.len() > suffix.len()
            && hostname.as_bytes()[hostname.len() - suffix.len() - 1] == b'.'
            && hostname[hostname.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    } else {
        pattern.eq_ignore_ascii_case(hostname)
    }
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        path.starts_with(prefix)
            && (path.len() == prefix.len() || path[prefix.len()..].starts_with('/'))
    } else if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len().saturating_sub(1)];
        path.starts_with(prefix)
    } else {
        normalize_exact_path(pattern) == normalize_exact_path(path)
    }
}

fn normalize_exact_path(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

#[cfg(test)]
mod tests;
