use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pingora_core::Result;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};
use tokio::sync::Notify;

use crate::protocol;
use crate::route_pattern::{route_host_matches_request, split_route_pattern};

// ---------------------------------------------------------------------------
// Route matching helpers (ported from tako-server/src/routing.rs)
// ---------------------------------------------------------------------------

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

fn route_specificity(pattern: &str) -> (u8, usize, u8) {
    if pattern.is_empty() {
        return (0, 0, 0);
    }
    let (pattern_host, pattern_path) = split_route_pattern(pattern);

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

// ---------------------------------------------------------------------------
// Compiled route entry
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct CompiledRoute {
    host: String,
    path: Option<String>,
    app_id: String,
    specificity: (u8, usize, u8),
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppRoute {
    upstream_port: u16,
    active: bool,
    notify: Arc<Notify>,
    images: tako_images::ImagesConfig,
    channel_store_key: String,
}

#[derive(Clone)]
pub(crate) struct RouteTarget {
    pub(crate) app_id: String,
    pub(crate) channel_store_key: String,
    pub(crate) upstream_port: u16,
    pub(crate) active: bool,
    pub(crate) images: tako_images::ImagesConfig,
}

#[derive(Clone, Default)]
pub struct Routes {
    /// Per-app route patterns (the raw strings from tako.toml).
    app_routes: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// Compiled routes sorted by specificity (most specific first).
    compiled: Arc<Mutex<Vec<CompiledRoute>>>,
    /// Per-app upstream + active state.
    apps: Arc<Mutex<HashMap<String, AppRoute>>>,
}

impl Routes {
    /// Register (or replace) all routes for an app.
    #[cfg(test)]
    pub fn set_routes(
        &self,
        app_id: String,
        routes: Vec<String>,
        upstream_port: u16,
        active: bool,
    ) {
        self.set_routes_with_images(
            app_id,
            routes,
            upstream_port,
            active,
            tako_images::ImagesConfig::default(),
        );
    }

    #[cfg(test)]
    pub fn set_routes_with_images(
        &self,
        app_id: String,
        routes: Vec<String>,
        upstream_port: u16,
        active: bool,
        images: tako_images::ImagesConfig,
    ) {
        self.set_routes_with_images_and_channel_store_key(
            app_id.clone(),
            routes,
            upstream_port,
            active,
            images,
            app_id,
        );
    }

    pub fn set_routes_with_images_and_channel_store_key(
        &self,
        app_id: String,
        routes: Vec<String>,
        upstream_port: u16,
        active: bool,
        images: tako_images::ImagesConfig,
        channel_store_key: String,
    ) {
        {
            let mut ar = self.app_routes.lock().unwrap();
            ar.insert(app_id.clone(), routes);
            self.rebuild(&ar);
        }

        let mut apps = self.apps.lock().unwrap();
        let entry = apps.entry(app_id).or_insert_with(|| AppRoute {
            upstream_port,
            active,
            notify: Arc::new(Notify::new()),
            images: images.clone(),
            channel_store_key: channel_store_key.clone(),
        });
        entry.upstream_port = upstream_port;
        entry.active = active;
        entry.images = images;
        entry.channel_store_key = channel_store_key;
        if active {
            entry.notify.notify_waiters();
        }
    }

    /// Remove all routes for an app.
    pub fn remove_app(&self, app_id: &str) {
        let mut ar = self.app_routes.lock().unwrap();
        ar.remove(app_id);
        self.rebuild(&ar);
        drop(ar);
        self.apps.lock().unwrap().remove(app_id);
    }

    pub fn set_active(&self, app_id: &str, active: bool) {
        if let Some(r) = self.apps.lock().unwrap().get_mut(app_id) {
            r.active = active;
            if active {
                r.notify.notify_waiters();
            }
        }
    }

    /// Mark the route active and update the upstream port atomically.
    ///
    /// Called when the app signals its bound port on the readiness pipe.
    pub fn activate_with_port(&self, app_id: &str, port: u16) {
        if let Some(r) = self.apps.lock().unwrap().get_mut(app_id) {
            r.upstream_port = port;
            r.active = true;
            r.notify.notify_waiters();
        }
    }

    /// Find the best matching route for a (host, path) pair.
    pub fn lookup(&self, host: &str, path: &str) -> Option<RouteTarget> {
        let app_id = {
            let compiled = self.compiled.lock().unwrap();
            let mut found = None;
            for entry in compiled.iter() {
                if !route_host_matches_request(&entry.host, host) {
                    continue;
                }
                if let Some(p) = &entry.path
                    && !path_matches(p, path)
                {
                    continue;
                }
                found = Some(entry.app_id.clone());
                break;
            }
            found?
        };
        let apps = self.apps.lock().unwrap();
        let r = apps.get(&app_id)?.clone();
        Some(RouteTarget {
            app_id,
            channel_store_key: r.channel_store_key,
            upstream_port: r.upstream_port,
            active: r.active,
            images: r.images,
        })
    }

    /// All route patterns across all apps, for error pages.
    pub fn all_display_routes(&self) -> Vec<String> {
        self.app_routes
            .lock()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    pub async fn wait_for_active_port(
        &self,
        app_id: &str,
        timeout: std::time::Duration,
    ) -> Option<u16> {
        let notify = {
            let apps = self.apps.lock().unwrap();
            let r = apps.get(app_id)?;
            if r.active {
                return Some(r.upstream_port);
            }
            r.notify.clone()
        };

        // Register interest before awaiting so a notify_waiters() that fires
        // between the lock release above and the await below is not lost.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let _ = tokio::time::timeout(timeout, notified).await;
        let apps = self.apps.lock().unwrap();
        apps.get(app_id)
            .and_then(|r| r.active.then_some(r.upstream_port))
    }

    /// Rebuild the compiled route table from all app_routes.
    fn rebuild(&self, app_routes: &HashMap<String, Vec<String>>) {
        let mut entries = Vec::new();
        for (app_id, patterns) in app_routes {
            for pattern in patterns {
                if pattern.is_empty() {
                    continue;
                }
                let (host, path) = split_route_pattern(pattern);
                entries.push(CompiledRoute {
                    host: host.to_string(),
                    path: path.map(|p| p.to_string()),
                    app_id: app_id.clone(),
                    specificity: route_specificity(pattern),
                });
            }
        }
        // Most specific first. Stable order for ties.
        entries.sort_by(|a, b| b.specificity.cmp(&a.specificity));
        *self.compiled.lock().unwrap() = entries;
    }
}

fn is_managed_dev_hostname(hostname: &str) -> bool {
    hostname
        .strip_suffix(".tako.test")
        .or_else(|| hostname.strip_suffix(".test"))
        .is_some()
}

fn unknown_application_name(hostname: &str) -> &str {
    // Strip the more specific suffix first so `app.tako.test` yields `app`, not `app.tako`.
    hostname
        .strip_suffix(".tako.test")
        .or_else(|| hostname.strip_suffix(".test"))
        .unwrap_or(hostname)
}

fn unknown_application_response_body(hostname: &str, routes: &Routes) -> String {
    if !is_managed_dev_hostname(hostname) {
        return "Misdirected Request".to_string();
    }

    let app_name = unknown_application_name(hostname);
    let mut known = routes.all_display_routes();
    known.sort();
    let routes: Vec<String> = known.iter().map(|r| format!("  https://{r}")).collect();

    format!(
        "Unknown application \"{app_name}\". Known routes:\n{}",
        routes.join("\n")
    )
}

#[derive(Clone)]
pub struct DevProxy {
    pub routes: Routes,
    pub events: tokio::sync::mpsc::UnboundedSender<protocol::DevEvent>,
    pub channels: crate::dev_channels::DevChannelStore,
}

#[derive(Default)]
pub struct Ctx {
    upstream_port: Option<u16>,
    host: Option<String>,
    path: Option<String>,
}

#[async_trait]
impl ProxyHttp for DevProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Self::CTX {
        Ctx::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let (hostname, path) = {
            let req = session.req_header();
            // HTTP/2 uses :authority (stored in URI), HTTP/1.1 uses Host header.
            let raw = req
                .uri
                .host()
                .or_else(|| req.headers.get("host").and_then(|h| h.to_str().ok()))
                .unwrap_or("");
            let host = raw.split(':').next().unwrap_or(raw).to_string();
            let path = req.uri.path().to_string();
            (host, path)
        };
        ctx.host = Some(hostname.clone());
        ctx.path = Some(path.clone());

        let _ = self.events.send(protocol::DevEvent::RequestStarted {
            host: hostname.clone(),
            path: path.clone(),
        });

        let Some(mut target) = self.routes.lookup(&hostname, &path) else {
            let mut header = ResponseHeader::build(421, None)?;
            header.insert_header("Content-Type", "text/plain")?;
            session
                .write_response_header(Box::new(header), false)
                .await?;

            session
                .write_response_body(
                    Some(unknown_application_response_body(&hostname, &self.routes).into()),
                    true,
                )
                .await?;
            return Ok(true);
        };

        // Channel requests are scoped by the matched app. Each app gets a
        // separate local dev replay store even when channel names overlap.
        if path.starts_with(tako_channels::CHANNELS_BASE_PATH) {
            let method = session.req_header().method.as_str().to_string();
            return crate::dev_channels::try_handle(
                session,
                &self.channels,
                &target.channel_store_key,
                &path,
                &method,
            )
            .await;
        }

        if !target.active {
            let ready_port = self
                .routes
                .wait_for_active_port(&target.app_id, std::time::Duration::from_secs(30))
                .await;
            let Some(active_port) = ready_port else {
                let mut header = ResponseHeader::build(503, None)?;
                header.insert_header("Content-Type", "text/plain")?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session
                    .write_response_body(Some("Starting…".into()), true)
                    .await?;
                return Ok(true);
            };
            target.upstream_port = active_port;
            target.active = true;
        }

        if crate::image::is_image_request_path(&path) {
            let method = session.req_header().method.as_str().to_string();
            return crate::image::try_handle(session, &target, &path, &hostname, &method).await;
        }

        ctx.upstream_port = Some(target.upstream_port);
        Ok(false)
    }

    async fn logging(
        &self,
        _session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) where
        Self::CTX: Send + Sync,
    {
        if let Some(host) = ctx.host.take() {
            let path = ctx.path.take().unwrap_or_default();
            let _ = self
                .events
                .send(protocol::DevEvent::RequestFinished { host, path });
        }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let port = ctx
            .upstream_port
            .ok_or_else(|| pingora_core::Error::new(pingora_core::ErrorType::ConnectNoRoute))?;
        let peer = HttpPeer::new(("127.0.0.1".to_string(), port), false, String::new());
        Ok(Box::new(peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_matches_wildcard_route() {
        let routes = Routes::default();
        routes.set_routes(
            "app".to_string(),
            vec!["*.app.test".to_string()],
            3000,
            true,
        );

        let hit = routes.lookup("foo.app.test", "/");
        assert!(hit.is_some());
        let hit = hit.unwrap();
        assert_eq!(hit.app_id, "app");
        assert_eq!(hit.upstream_port, 3000);
        assert!(hit.active);

        // Unrelated host should not match.
        assert!(routes.lookup("foo.other.test", "/").is_none());
    }

    #[test]
    fn lookup_returns_channel_store_key() {
        let routes = Routes::default();
        routes.set_routes_with_images_and_channel_store_key(
            "reg:/repo/tako.toml".to_string(),
            vec!["app.test".to_string()],
            3000,
            true,
            tako_images::ImagesConfig::default(),
            "app".to_string(),
        );

        let hit = routes.lookup("app.test", "/").unwrap();
        assert_eq!(hit.app_id, "reg:/repo/tako.toml");
        assert_eq!(hit.channel_store_key, "app");
    }

    #[test]
    fn activate_with_port_updates_port_and_marks_active() {
        let routes = Routes::default();
        // Register with a placeholder port and inactive.
        routes.set_routes("app".to_string(), vec!["app.test".to_string()], 0, false);

        routes.activate_with_port("app", 54321);

        let hit = routes.lookup("app.test", "/").unwrap();
        assert_eq!(hit.upstream_port, 54321);
        assert!(hit.active);
    }

    #[tokio::test]
    async fn routes_waits_for_active_port() {
        let routes = Routes::default();
        routes.set_routes("app".to_string(), vec!["a.test".to_string()], 1234, false);

        let r2 = routes.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            r2.set_active("app", true);
        });

        assert_eq!(
            routes
                .wait_for_active_port("app", std::time::Duration::from_secs(1))
                .await,
            Some(1234)
        );
    }

    #[tokio::test]
    async fn wait_for_active_port_returns_refreshed_port() {
        let routes = Routes::default();
        routes.set_routes("app".to_string(), vec!["app.test".to_string()], 0, false);

        let r2 = routes.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            r2.activate_with_port("app", 4321);
        });

        let port = routes
            .wait_for_active_port("app", std::time::Duration::from_secs(1))
            .await;
        assert_eq!(port, Some(4321));
    }

    #[test]
    fn lookup_matches_path_route() {
        let routes = Routes::default();
        routes.set_routes(
            "api".to_string(),
            vec!["app.test/api/*".to_string()],
            3001,
            true,
        );
        routes.set_routes("web".to_string(), vec!["app.test".to_string()], 3002, true);

        // /api/users → api app
        let hit = routes.lookup("app.test", "/api/users");
        assert!(hit.is_some());
        let hit = hit.unwrap();
        assert_eq!(hit.app_id, "api");
        assert_eq!(hit.upstream_port, 3001);

        // / → web app
        let hit = routes.lookup("app.test", "/");
        assert!(hit.is_some());
        let hit = hit.unwrap();
        assert_eq!(hit.app_id, "web");
        assert_eq!(hit.upstream_port, 3002);
    }

    #[test]
    fn lookup_exact_path_beats_wildcard_path() {
        let routes = Routes::default();
        routes.set_routes(
            "exact".to_string(),
            vec!["app.test/api/health".to_string()],
            3001,
            true,
        );
        routes.set_routes(
            "wildcard".to_string(),
            vec!["app.test/api/*".to_string()],
            3002,
            true,
        );

        let hit = routes.lookup("app.test", "/api/health").unwrap();
        assert_eq!(hit.app_id, "exact");

        let hit = routes.lookup("app.test", "/api/other").unwrap();
        assert_eq!(hit.app_id, "wildcard");
    }

    #[test]
    fn lookup_exact_host_beats_wildcard_host() {
        let routes = Routes::default();
        routes.set_routes(
            "exact".to_string(),
            vec!["api.app.test".to_string()],
            3001,
            true,
        );
        routes.set_routes(
            "wildcard".to_string(),
            vec!["*.app.test".to_string()],
            3002,
            true,
        );

        let hit = routes.lookup("api.app.test", "/").unwrap();
        assert_eq!(hit.app_id, "exact");

        let hit = routes.lookup("other.app.test", "/").unwrap();
        assert_eq!(hit.app_id, "wildcard");
    }

    #[test]
    fn lookup_matches_local_alias_of_exact_host_route() {
        let routes = Routes::default();
        routes.set_routes("app".to_string(), vec!["app.test".to_string()], 3000, true);

        let hit = routes.lookup("app.local", "/").unwrap();
        assert_eq!(hit.app_id, "app");
        assert_eq!(hit.upstream_port, 3000);
    }

    #[test]
    fn lookup_matches_local_alias_of_wildcard_and_path_route() {
        let routes = Routes::default();
        routes.set_routes(
            "app".to_string(),
            vec!["*.app.test/api/*".to_string()],
            3000,
            true,
        );

        let hit = routes.lookup("foo.app.local", "/api/health").unwrap();
        assert_eq!(hit.app_id, "app");
        assert_eq!(hit.upstream_port, 3000);
    }

    #[test]
    fn remove_app_cleans_up() {
        let routes = Routes::default();
        routes.set_routes("app".to_string(), vec!["app.test".to_string()], 3000, true);
        assert!(routes.lookup("app.test", "/").is_some());

        routes.remove_app("app");
        assert!(routes.lookup("app.test", "/").is_none());
        assert!(routes.all_display_routes().is_empty());
    }

    #[test]
    fn all_display_routes_shows_full_patterns() {
        let routes = Routes::default();
        routes.set_routes(
            "app".to_string(),
            vec!["app.test".to_string(), "app.test/api".to_string()],
            3000,
            true,
        );

        let mut display = routes.all_display_routes();
        display.sort();
        assert_eq!(display, vec!["app.test", "app.test/api"]);
    }

    #[test]
    fn unknown_application_response_lists_routes_for_managed_hosts() {
        let routes = Routes::default();
        routes.set_routes("app".to_string(), vec!["app.test".to_string()], 3000, true);

        assert_eq!(
            unknown_application_response_body("missing.test", &routes),
            "Unknown application \"missing\". Known routes:\n  https://app.test"
        );
        assert_eq!(
            unknown_application_response_body("missing.tako.test", &routes),
            "Unknown application \"missing\". Known routes:\n  https://app.test"
        );
    }

    #[test]
    fn unknown_application_response_hides_routes_for_lan_and_external_hosts() {
        let routes = Routes::default();
        routes.set_routes("app".to_string(), vec!["app.test".to_string()], 3000, true);

        assert_eq!(
            unknown_application_response_body("missing.local", &routes),
            "Misdirected Request"
        );
        assert_eq!(
            unknown_application_response_body("local-rb.affinehq.com", &routes),
            "Misdirected Request"
        );
    }

    #[test]
    fn hostname_matches_basic() {
        assert!(route_host_matches_request("app.test", "app.test"));
        assert!(!route_host_matches_request("app.test", "other.test"));
        assert!(route_host_matches_request("*.app.test", "foo.app.test"));
        assert!(!route_host_matches_request("*.app.test", "app.test"));
        assert!(route_host_matches_request("app.test", "app.local"));
        assert!(route_host_matches_request("*.app.test", "foo.app.local"));
    }

    #[test]
    fn path_matches_basic() {
        assert!(path_matches("/api/*", "/api/users"));
        assert!(path_matches("/api/*", "/api"));
        assert!(!path_matches("/api/*", "/apifoo"));
        assert!(path_matches("/api", "/api"));
        assert!(path_matches("/api", "/api/"));
        assert!(!path_matches("/api", "/api/users"));
    }

    #[test]
    fn specificity_ordering() {
        // exact host > wildcard host
        assert!(route_specificity("app.test") > route_specificity("*.app.test"));
        // longer path > shorter path
        assert!(route_specificity("app.test/api/v1/*") > route_specificity("app.test/api/*"));
        // exact path > wildcard path of same length
        assert!(route_specificity("app.test/api") > route_specificity("app.test/api/*"));
    }
}
