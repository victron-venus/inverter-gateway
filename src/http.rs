use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{sse, IntoResponse, Response, Sse},
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::debug;

use crate::state::{AppState, Snapshot};
use crate::whitelist;

pub fn router(state: Arc<AppState>) -> Router {
    let cors = if state.cfg.cors_origins.is_empty() {
        CorsLayer::new()
    } else {
        let origins: Vec<_> = state
            .cfg
            .cors_origins
            .iter()
            .map(|o| o.parse::<HeaderValue>().unwrap())
            .collect();
        CorsLayer::new().allow_origin(AllowOrigin::list(origins))
    };

    Router::new()
        .route("/health", get(health))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/events", get(events))
        .route("/v1/commands/{name}", get(command_get).post(command_post))
        .layer(cors)
        .with_state(state)
}

type SharedState = Arc<AppState>;

// ── Error type ─────────────────────────────────────────────────────────────────

#[allow(dead_code)]
enum AppError {
    Unauthorized,
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response()
            }
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        }
    }
}

fn require_auth(state: &SharedState, headers: &HeaderMap) -> Result<(), AppError> {
    if state.cfg.allow_insecure {
        return Ok(());
    }
    let Some(token) = &state.cfg.api_token else {
        return Err(AppError::Unauthorized);
    };
    let header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let bearer = header.strip_prefix("Bearer ").unwrap_or(header);
    // Constant-time compare to mitigate timing attacks.
    let ok: bool = subtle::ConstantTimeEq::ct_eq(bearer.as_bytes(), token.as_bytes()).into();
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health(State(state): State<SharedState>) -> Json<HealthResponse> {
    let connected = *state.shared.mqtt_connected.read();
    Json(HealthResponse {
        status: if connected { "ok" } else { "degraded" }.to_string(),
        mqtt_connected: connected,
    })
}

#[derive(serde::Serialize)]
struct HealthResponse {
    status: String,
    mqtt_connected: bool,
}

async fn snapshot(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Snapshot>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(state.snapshot()))
}

async fn events(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let rx = state.shared.sse_tx.subscribe();

    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|res| match res {
        Ok(snap) => {
            let data = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
            Ok::<_, std::convert::Infallible>(sse::Event::default().data(data))
        }
        Err(e) => {
            tracing::error!(error = %e, "sse broadcast error");
            Ok(sse::Event::default().comment("error"))
        }
    });

    Sse::new(stream)
        .keep_alive(sse::KeepAlive::default())
        .into_response()
}

async fn command_get(
    State(_state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CommandInfo>, AppError> {
    require_auth(&_state, &headers)?;
    if !whitelist::is_known(&name) {
        return Err(AppError::NotFound(format!("unknown command: {name}")));
    }
    Ok(Json(CommandInfo { name }))
}

async fn command_post(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<CommandResult>, AppError> {
    require_auth(&state, &headers)?;
    debug!(cmd = %name, "command POST");
    match whitelist::execute(&state, &name, body).await {
        Ok(()) => Ok(Json(CommandResult {
            ok: true,
            message: None,
        })),
        Err(whitelist::CommandError::NotWired) => {
            Err(AppError::Internal("command not wired to MQTT".to_string()))
        }
        Err(whitelist::CommandError::UnknownCommand(n)) => {
            Err(AppError::NotFound(format!("unknown command: {n}")))
        }
        Err(whitelist::CommandError::PublishFailed(e)) => {
            Err(AppError::Internal(format!("mqtt publish failed: {e}")))
        }
    }
}

#[derive(serde::Serialize)]
struct CommandInfo {
    name: String,
}

#[derive(serde::Serialize)]
struct CommandResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_state(cfg: Config) -> Arc<AppState> {
        AppState::new(cfg)
    }

    fn cfg_with_token(token: &str) -> Config {
        Config {
            mqtt_host: "x".into(),
            mqtt_port: 1883,
            mqtt_username: "u".into(),
            mqtt_password: "p".into(),
            mqtt_client_id: "c".into(),
            http_bind: "127.0.0.1:0".parse().unwrap(),
            api_token: Some(token.to_string()),
            topic_prefix: "N/test/".into(),
            write_topic_prefix: "W/test/".into(),
            allow_insecure: false,
            cors_origins: vec![],
        }
    }

    fn cfg_insecure() -> Config {
        let mut c = cfg_with_token("token");
        c.allow_insecure = true;
        c
    }

    #[test]
    fn auth_rejects_missing_header() {
        let state = make_state(cfg_with_token("secret"));
        let headers = HeaderMap::new();
        assert!(require_auth(&state, &headers).is_err());
    }

    #[test]
    fn auth_rejects_wrong_token() {
        let state = make_state(cfg_with_token("secret"));
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer wrong".parse().unwrap());
        assert!(require_auth(&state, &headers).is_err());
    }

    #[test]
    fn auth_accepts_correct_token() {
        let state = make_state(cfg_with_token("secret"));
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer secret".parse().unwrap());
        assert!(require_auth(&state, &headers).is_ok());
    }

    #[test]
    fn auth_skips_when_insecure() {
        let state = make_state(cfg_insecure());
        let headers = HeaderMap::new();
        assert!(require_auth(&state, &headers).is_ok());
    }

    #[test]
    fn auth_rejects_when_token_required_but_missing() {
        // No token set, not insecure → must reject.
        let mut cfg = cfg_with_token("secret");
        cfg.api_token = None;
        let state = make_state(cfg);
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer secret".parse().unwrap());
        assert!(require_auth(&state, &headers).is_err());
    }

    #[test]
    fn health_reflects_mqtt_connected() {
        let state = make_state(cfg_insecure());
        *state.shared.mqtt_connected.write() = true;
        let s = Arc::clone(&state);
        let resp = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { health(State(s)).await });
        assert!(resp.mqtt_connected);
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn health_reports_degraded_when_disconnected() {
        let state = make_state(cfg_insecure());
        // mqtt_connected stays false
        let s = Arc::clone(&state);
        let resp = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { health(State(s)).await });
        assert!(!resp.mqtt_connected);
        assert_eq!(resp.status, "degraded");
    }
}
