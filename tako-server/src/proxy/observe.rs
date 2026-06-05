use pingora_http::RequestHeader;
use std::time::{Duration, Instant};

const REQUEST_ID_HEADER: &str = "x-request-id";
const REQUEST_ID_LEN: usize = 16;
const MAX_PROPAGATED_REQUEST_ID_LEN: usize = 128;

#[derive(Debug)]
pub(super) struct RequestObservation {
    started_at: Instant,
    request_id: String,
    app: Option<String>,
    instance: Option<String>,
    route: Option<String>,
    handler: &'static str,
    handler_result: &'static str,
    cache_result: &'static str,
    route_lookup_ms: u64,
    cold_start_wait_ms: u64,
    upstream_response_ms: u64,
    upstream_started_at: Option<Instant>,
}

impl RequestObservation {
    pub(super) fn new() -> Self {
        Self {
            started_at: Instant::now(),
            request_id: generate_request_id(),
            app: None,
            instance: None,
            route: None,
            handler: "proxy",
            handler_result: "pending",
            cache_result: "-",
            route_lookup_ms: 0,
            cold_start_wait_ms: 0,
            upstream_response_ms: 0,
            upstream_started_at: None,
        }
    }

    pub(super) fn initialize_request_id(&mut self, request: &RequestHeader) {
        self.request_id = request
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(normalize_propagated_request_id)
            .map(str::to_string)
            .unwrap_or_else(generate_request_id);
    }

    pub(super) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(super) fn app(&self) -> &str {
        self.app.as_deref().unwrap_or("-")
    }

    pub(super) fn instance(&self) -> &str {
        self.instance.as_deref().unwrap_or("-")
    }

    pub(super) fn route(&self) -> &str {
        self.route.as_deref().unwrap_or("-")
    }

    pub(super) fn handler(&self) -> &str {
        self.handler
    }

    pub(super) fn handler_result(&self) -> &str {
        self.handler_result
    }

    pub(super) fn cache_result(&self) -> &str {
        self.cache_result
    }

    pub(super) fn route_lookup_ms(&self) -> u64 {
        self.route_lookup_ms
    }

    pub(super) fn cold_start_wait_ms(&self) -> u64 {
        self.cold_start_wait_ms
    }

    pub(super) fn upstream_response_ms(&self) -> u64 {
        self.upstream_response_ms
    }

    pub(super) fn total_ms(&self) -> u64 {
        elapsed_ms(self.started_at.elapsed())
    }

    pub(super) fn set_route_lookup_elapsed(&mut self, elapsed: Duration) {
        self.route_lookup_ms = elapsed_ms(elapsed);
    }

    pub(super) fn set_app_route(&mut self, app: &str, route: Option<&str>) {
        self.app = Some(app.to_string());
        self.route = route.map(str::to_string);
    }

    pub(super) fn set_instance(&mut self, instance: &str) {
        self.instance = Some(instance.to_string());
    }

    pub(super) fn set_handler(&mut self, handler: &'static str, result: &'static str) {
        self.handler = handler;
        self.handler_result = result;
    }

    pub(super) fn set_handler_result(&mut self, result: &'static str) {
        self.handler_result = result;
    }

    pub(super) fn set_cache_result(&mut self, result: &'static str) {
        self.cache_result = result;
    }

    pub(super) fn set_cold_start_wait(&mut self, elapsed: Duration) {
        self.cold_start_wait_ms = elapsed_ms(elapsed);
    }

    pub(super) fn start_upstream(&mut self) {
        self.upstream_started_at = Some(Instant::now());
    }

    pub(super) fn finish_upstream_response(&mut self) {
        if let Some(started_at) = self.upstream_started_at.take() {
            self.upstream_response_ms = elapsed_ms(started_at.elapsed());
        }
    }
}

fn normalize_propagated_request_id(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_PROPAGATED_REQUEST_ID_LEN {
        return None;
    }

    trimmed
        .bytes()
        .all(|byte| matches!(byte, b'!'..=b'~'))
        .then_some(trimmed)
}

fn generate_request_id() -> String {
    nanoid::nanoid!(REQUEST_ID_LEN)
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propagated_request_id_is_trimmed_and_reused() {
        assert_eq!(
            normalize_propagated_request_id(" req-123 "),
            Some("req-123")
        );
    }

    #[test]
    fn propagated_request_id_rejects_empty_control_and_long_values() {
        assert_eq!(normalize_propagated_request_id("   "), None);
        assert_eq!(normalize_propagated_request_id("req\n123"), None);
        assert_eq!(
            normalize_propagated_request_id(&"x".repeat(MAX_PROPAGATED_REQUEST_ID_LEN + 1)),
            None
        );
    }

    #[test]
    fn generated_request_ids_are_compact() {
        let request_id = generate_request_id();

        assert_eq!(request_id.len(), REQUEST_ID_LEN);
    }

    #[test]
    fn observation_records_route_backend_and_timings() {
        let mut observation = RequestObservation::new();

        observation.set_route_lookup_elapsed(Duration::from_millis(3));
        observation.set_app_route("demo/production", Some("/blog"));
        observation.set_instance("inst123");
        observation.set_cold_start_wait(Duration::from_millis(45));
        observation.set_handler("proxy", "upstream");
        observation.set_cache_result("miss");

        assert_eq!(observation.app(), "demo/production");
        assert_eq!(observation.route(), "/blog");
        assert_eq!(observation.instance(), "inst123");
        assert_eq!(observation.route_lookup_ms(), 3);
        assert_eq!(observation.cold_start_wait_ms(), 45);
        assert_eq!(observation.handler(), "proxy");
        assert_eq!(observation.handler_result(), "upstream");
        assert_eq!(observation.cache_result(), "miss");
    }
}
