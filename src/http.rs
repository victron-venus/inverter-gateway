use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{sse, IntoResponse, Response, Sse},
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::debug;

use crate::state::{AppState, Snapshot};
use crate::whitelist;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/events", get(events))
        .route("/v1/commands/{name}", get(command_get).post(command_post))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

type SharedState = Arc<AppState>;

// ── Auth ───────────────────────────────────────────────────────────────────────

struct AuthError;

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response()
    }
}

fn require_auth(state: &SharedState, headers: &HeaderMap) -> Result<(), StatusCode> {
    if let Some(token) = &state.cfg.api_token {
        let header = headers.get("Authorization").and_then(|v| v.to_str().ok());
        let header = match header {
            Some(h) => h,
            None => return Err(StatusCode::UNAUTHORIZED),
        };
        let bearer = header.strip_prefix("Bearer ").unwrap_or(header);
        if bearer != token {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health(_state: State<SharedState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        mqtt_connected: true,
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
) -> Result<Json<Snapshot>, Response> {
    require_auth(&state, &headers).map_err(|_| AuthError.into_response())?;
    Ok(Json(state.snapshot()))
}

async fn events(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(code) = require_auth(&state, &headers) {
        return (code, "Unauthorized").into_response();
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
) -> Result<Json<CommandInfo>, Response> {
    require_auth(&_state, &headers).map_err(|_| AuthError.into_response())?;
    if !whitelist::is_known(&name) {
        return Err((StatusCode::NOT_FOUND, format!("unknown command: {name}")).into_response());
    }
    Ok(Json(CommandInfo { name }))
}

async fn command_post(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<CommandResult>, Response> {
    require_auth(&state, &headers).map_err(|_| AuthError.into_response())?;
    debug!(cmd = %name, "command POST");
    match whitelist::execute(&state, &name, body).await {
        Ok(()) => Ok(Json(CommandResult {
            ok: true,
            message: None,
        })),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string()).into_response()),
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
