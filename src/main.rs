use tokio::signal;
use tracing::{info, warn};

mod alarms;
mod config;
mod energy;
mod http;
mod mqtt_bridge;
mod state;
mod transport;
mod whitelist;

use crate::config::Config;
use crate::mqtt_bridge::MqttBridge;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("inverter-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    init_tracing();

    let cfg = Config::from_env()?;
    if cfg.allow_insecure {
        warn!(
            "GATEWAY_ALLOW_INSECURE=1 is set — bearer auth is DISABLED. Do not use in production."
        );
    }
    info!(bind = %cfg.http_bind, mqtt = %cfg.mqtt_host, "starting inverter-gateway");

    let listeners = transport::Listeners::bind(cfg.http_bind, cfg.https.as_ref()).await?;
    let state = AppState::new(cfg.clone());
    state.shared.start_sse_coalesce();
    let mqtt = MqttBridge::start(state.clone(), cfg.clone())?;

    let app = http::router(state.clone());
    let result = listeners.serve(app, shutdown_signal()).await;
    mqtt.shutdown().await;
    result?;
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,inverter_gateway=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => warn!("ctrl-c received"),
        _ = term => warn!("terminate received"),
    }
}
