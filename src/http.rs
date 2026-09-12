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
        .route("/v1/energy", get(energy))
        .route("/v1/commands/{name}", get(command_get).post(command_post))
        .layer(cors)
        .with_state(state)
}

type SharedState = Arc<AppState>;

// ── Error type ─────────────────────────────────────────────────────────────────

#[allow(dead_code)]
enum AppError {
    Unauthorized,
    Unavailable,
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
            AppError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "MQTT telemetry unavailable",
            )
                .into_response(),
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

fn require_read_auth(state: &SharedState, headers: &HeaderMap) -> Result<(), AppError> {
    if require_auth(state, headers).is_ok() {
        return Ok(());
    }
    let Some(token) = &state.cfg.read_token else {
        return Err(AppError::Unauthorized);
    };
    let header = headers
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let bearer = header.strip_prefix("Bearer ").unwrap_or(header);
    let ok: bool = subtle::ConstantTimeEq::ct_eq(bearer.as_bytes(), token.as_bytes()).into();
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health(State(state): State<SharedState>) -> Json<HealthResponse> {
    let connected = state.shared.is_connected();
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
    require_read_auth(&state, &headers)?;
    state.snapshot().map(Json).ok_or(AppError::Unavailable)
}

async fn energy(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    require_read_auth(&state, &headers)?;
    // Consumers must evaluate fresh telemetry for each question, including outages.
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(state.shared.energy(&state.cfg.energy)),
    )
        .into_response())
}

async fn events(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_read_auth(&state, &headers) {
        return e.into_response();
    }

    let Some((generation, rx)) = state.shared.subscribe() else {
        return AppError::Unavailable.into_response();
    };

    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .take_while(move |res| {
            state.shared.generation_is_connected(generation)
                && res
                    .as_ref()
                    .is_ok_and(|event| event.generation == generation && event.snapshot.is_some())
        })
        .map(|res| match res {
            Ok(event) => {
                let snap = event.snapshot;
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
            read_token: Some("read-secret".into()),
            energy: crate::energy::EnergyConfig::default(),
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
        state.shared.set_connected(true);
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
    fn update_voltage(state: &SharedState, value: i32) {
        state.shared.update(crate::state::ParsedUpdate {
            service: "battery".into(),
            path: "0/Dc/0/Voltage".into(),
            value: serde_json::json!(value),
        });
    }

    #[tokio::test]
    async fn snapshot_outage_and_recovery_preserve_auth_and_liveness() {
        let state = make_state(cfg_with_token("secret"));
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer secret".parse().unwrap());
        for connected in [false, true] {
            state.shared.set_connected(connected);
            let response = snapshot(State(state.clone()), headers.clone())
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(health(State(state.clone())).await.mqtt_connected, connected);
            assert_eq!(
                snapshot(State(state.clone()), HeaderMap::new())
                    .await
                    .into_response()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                events(State(state.clone()), HeaderMap::new())
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                events(State(state.clone()), headers.clone()).await.status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        update_voltage(&state, 52);
        assert_eq!(
            snapshot(State(state.clone()), headers.clone())
                .await
                .unwrap_or_else(|_| panic!("expected telemetry"))
                .0
                .battery["0/Dc/0/Voltage"],
            52
        );
        state.shared.set_connected(false);
        update_voltage(&state, 99); // Late data cannot repopulate an offline cache.
        state.shared.set_connected(true);
        assert!(state.snapshot().is_none());
        update_voltage(&state, 48);
        assert_eq!(state.snapshot().unwrap().battery["0/Dc/0/Voltage"], 48);
    }

    #[tokio::test]
    async fn sse_discards_queued_old_snapshots_after_fast_reconnect() {
        let state = make_state(cfg_insecure());
        state.shared.set_connected(true);
        update_voltage(&state, 52);
        let response = events(State(state.clone()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        state.shared.broadcast_snapshot();
        state.shared.set_connected(false);
        state.shared.set_connected(true);
        update_voltage(&state, 48);
        state.shared.broadcast_snapshot();
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            axum::body::to_bytes(response.into_body(), 4096),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            body.is_empty(),
            "old SSE session must terminate without stale data"
        );
        let response = events(State(state.clone()), HeaderMap::new()).await;
        update_voltage(&state, 49);
        state.shared.broadcast_snapshot();
        let mut body = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = std::str::from_utf8(&frame).unwrap();
        let data = frame.strip_prefix("data: ").unwrap().trim();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data).unwrap(),
            serde_json::json!({"battery": {"0/Dc/0/Voltage": 49}})
        );
        state.shared.set_connected(false);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), body.next())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn read_token_is_limited_to_telemetry_routes() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let state = make_state(cfg_with_token("full-secret"));
        state.shared.set_connected(true);
        update_voltage(&state, 52);
        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel();
        *state.shared.command_tx.lock() = Some(tx);
        let app = router(state.clone());
        for path in ["/v1/snapshot", "/v1/events", "/v1/energy"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("Authorization", "Bearer read-secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        for method in ["GET", "POST"] {
            for path in ["/v1/commands/silence_alarm", "/v1/commands/unknown"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .header("Authorization", "Bearer read-secret")
                            .header("Content-Type", "application/json")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {path}"
                );
            }
        }
        assert!(commands.try_recv().is_err());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commands/silence_alarm")
                    .header("Authorization", "Bearer full-secret")
                    .header("Content-Type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(commands.try_recv().unwrap().0, "W/test/vebus/0/Alarm");
    }

    #[tokio::test]
    async fn energy_outage_is_an_authenticated_noncacheable_semantic_response() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let state = make_state(cfg_with_token("full-secret"));
        let app = router(state);
        for token in [
            None,
            Some("wrong"),
            Some("read-secret"),
            Some("full-secret"),
        ] {
            let mut request = Request::builder().uri("/v1/energy");
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            if matches!(token, Some("read-secret" | "full-secret")) {
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["Cache-Control"], "no-store");
                let body = axum::body::to_bytes(response.into_body(), 16384)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["schema_version"], 1);
                assert_eq!(value["mqtt_connected"], false);
                assert!(value["generated_at"].as_u64().unwrap() > 0);
                assert_eq!(
                    value["metrics"]["battery_soc"]["value"],
                    serde_json::Value::Null
                );
                assert_eq!(value["reports"]["battery"]["status"], "unavailable");
                assert_eq!(value["reports"]["status"]["status"], "unavailable");
            } else {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
        }
    }
}
