use crate::presence::{PresenceRegistry, PresenceVerifyInput};
use crate::stores::{MAX_STORE_BYTES, StoreLoadInput, StoreRegistry};
use ::axum::Json;
use ::axum::body::{Body, to_bytes};
use ::axum::extract::State;
use ::axum::http::{Method, Request, StatusCode};
use ::axum::middleware::Next;
use ::axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const REGISTRY_PATH: &str = "/channels/registry";
const AUTHORIZE_PATH: &str = "/channels/authorize";
const LOAD_PATH: &str = "/stores/load";

#[derive(Clone, Default)]
pub(crate) struct RealtimeState {
    pub presence: Option<Arc<PresenceRegistry>>,
    pub stores: Option<Arc<StoreRegistry>>,
}

fn is_realtime_request(request: &Request<Body>) -> bool {
    matches!(
        request.uri().path(),
        REGISTRY_PATH | AUTHORIZE_PATH | LOAD_PATH
    ) && request
        .headers()
        .get("host")
        .and_then(|host| host.to_str().ok())
        .is_some_and(|host| crate::host_without_port(host).ends_with(".tako"))
}

pub(crate) async fn realtime_middleware(
    State(state): State<RealtimeState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !is_realtime_request(&request) {
        return next.run(request).await;
    }
    handle_realtime(request, &state, crate::internal_token()).await
}

fn response(status: StatusCode, body: Value, token: Option<&str>) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    if let Some(token) = token.and_then(|token| token.parse().ok()) {
        response
            .headers_mut()
            .insert(crate::INTERNAL_TOKEN_HEADER, token);
    }
    response
}

async fn handle_realtime(
    request: Request<Body>,
    state: &RealtimeState,
    token: Option<&str>,
) -> Response {
    let provided = request
        .headers()
        .get(crate::INTERNAL_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if !token.is_some_and(|token| Some(token) == provided) {
        return response(StatusCode::FORBIDDEN, json!({"error":"Forbidden"}), None);
    }
    match (request.method(), request.uri().path()) {
        (&Method::GET, REGISTRY_PATH) => {
            let mut metadata = state
                .presence
                .as_ref()
                .map_or_else(Vec::new, |registry| registry.metadata());
            if let Some(stores) = &state.stores {
                metadata.extend(stores.metadata());
            }
            response(StatusCode::OK, json!(metadata), token)
        }
        (&Method::POST, AUTHORIZE_PATH) => {
            let input: PresenceVerifyInput = match read_input::<PresenceVerifyInput>(request).await
            {
                Some(input)
                    if !input.channel.is_empty()
                        && !input.operation.is_empty()
                        && input.params.is_object() =>
                {
                    input
                }
                _ => {
                    return response(
                        StatusCode::BAD_REQUEST,
                        json!({"error":"Invalid request"}),
                        token,
                    );
                }
            };
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                if let Some(stores) = &state.stores
                    && let Some(result) = stores.authorize(input.clone()).await
                {
                    return Some((result.ok, serde_json::to_value(result).unwrap()));
                }
                if let Some(presence) = &state.presence
                    && let Some(result) = presence.authorize(input).await
                {
                    return Some((result.ok, serde_json::to_value(result).unwrap()));
                }
                None
            })
            .await;
            match result {
                Ok(Some((true, body))) => response(StatusCode::OK, body, token),
                Ok(Some((false, _))) => {
                    response(StatusCode::FORBIDDEN, json!({"error":"Forbidden"}), token)
                }
                Ok(None) => response(StatusCode::NOT_FOUND, json!({"error":"Not defined"}), token),
                Err(_) => response(
                    StatusCode::GATEWAY_TIMEOUT,
                    json!({"error":"Authorization timed out"}),
                    token,
                ),
            }
        }
        (&Method::POST, LOAD_PATH) => {
            let input: StoreLoadInput = match read_input::<StoreLoadInput>(request).await {
                Some(input) if !input.store.is_empty() && input.params.is_object() => input,
                _ => {
                    return response(
                        StatusCode::BAD_REQUEST,
                        json!({"error":"Invalid request"}),
                        token,
                    );
                }
            };
            let Some(stores) = &state.stores else {
                return response(
                    StatusCode::NOT_FOUND,
                    json!({"error":"Store not defined"}),
                    token,
                );
            };
            match tokio::time::timeout(Duration::from_secs(30), stores.load(input)).await {
                Ok(Some(Ok(snapshot))) => response(
                    StatusCode::OK,
                    serde_json::to_value(snapshot).unwrap(),
                    token,
                ),
                Ok(Some(Err(_))) => response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error":"Store load failed"}),
                    token,
                ),
                Ok(None) => response(
                    StatusCode::NOT_FOUND,
                    json!({"error":"Store not defined"}),
                    token,
                ),
                Err(_) => response(
                    StatusCode::GATEWAY_TIMEOUT,
                    json!({"error":"Store load timed out"}),
                    token,
                ),
            }
        }
        _ => response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"Method not allowed"}),
            token,
        ),
    }
}

async fn read_input<T: serde::de::DeserializeOwned>(request: Request<Body>) -> Option<T> {
    let body = to_bytes(request.into_body(), MAX_STORE_BYTES).await.ok()?;
    serde_json::from_slice(&body).ok()
}

#[cfg(test)]
mod tests;
