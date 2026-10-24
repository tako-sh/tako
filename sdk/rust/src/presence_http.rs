use crate::presence::{PresenceRegistry, PresenceVerifyInput};
use ::axum::Json;
use ::axum::body::{Body, to_bytes};
use ::axum::extract::State;
use ::axum::http::{Method, Request, StatusCode};
use ::axum::middleware::Next;
use ::axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

const REGISTRY_PATH: &str = "/channels/registry";
const AUTHORIZE_PATH: &str = "/channels/authorize";

fn is_presence_request(request: &Request<Body>) -> bool {
    matches!(request.uri().path(), REGISTRY_PATH | AUTHORIZE_PATH)
        && request
            .headers()
            .get("host")
            .and_then(|host| host.to_str().ok())
            .is_some_and(|host| crate::host_without_port(host).ends_with(".tako"))
}

pub(crate) async fn presence_middleware(
    State(registry): State<Arc<PresenceRegistry>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !is_presence_request(&request) {
        return next.run(request).await;
    }
    handle_presence(request, &registry, crate::internal_token()).await
}

async fn handle_presence(
    request: Request<Body>,
    registry: &PresenceRegistry,
    token: Option<&str>,
) -> Response {
    let provided = request
        .headers()
        .get(crate::INTERNAL_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if !token.is_some_and(|token| Some(token) == provided) {
        return (StatusCode::FORBIDDEN, Json(json!({"error":"Forbidden"}))).into_response();
    }
    let mut response = match (request.method(), request.uri().path()) {
        (&Method::GET, REGISTRY_PATH) => Json(json!(registry.metadata())).into_response(),
        (&Method::POST, AUTHORIZE_PATH) => {
            let body = match to_bytes(request.into_body(), 1 << 20).await {
                Ok(body) => body,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":"Invalid request"})),
                    )
                        .into_response();
                }
            };
            let input = match serde_json::from_slice::<PresenceVerifyInput>(&body) {
                Ok(input) if !input.channel.is_empty() && !input.operation.is_empty() => input,
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":"Invalid request"})),
                    )
                        .into_response();
                }
            };
            match registry.authorize(input).await {
                Some(result) if result.ok => Json(result).into_response(),
                Some(_) => {
                    return (StatusCode::FORBIDDEN, Json(json!({"error":"Forbidden"})))
                        .into_response();
                }
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error":"Presence not defined"})),
                    )
                        .into_response();
                }
            }
        }
        _ => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(json!({"error":"Method not allowed"})),
            )
                .into_response();
        }
    };
    if let Some(token) = token.and_then(|token| token.parse().ok()) {
        response
            .headers_mut()
            .insert(crate::INTERNAL_TOKEN_HEADER, token);
    }
    response
}

#[cfg(test)]
mod tests;
