mod agwpe;
mod config;
mod proxy;
mod rewrite;
mod state;
mod ui;

use config::CliArgs;
use proxy::AppContext;
use state::create_shared_state;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::broadcast;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("AGWPE error: {0}")]
    Agwpe(#[from] agwpe::AgwpeError),
    #[error("Configuration error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[tokio::main]
async fn main() -> Result<(), ClientError> {
    let cli = CliArgs::parse();

    let log_level = match cli.verbosity {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .init();

    let config = cli.resolve_config()?;
    let listen_addr = cli.listen_addr.clone();

    let shared_state = create_shared_state(config.clone());
    let (log_tx, _) = broadcast::channel::<state::DebugLogEntry>(256);

    let agwpe_manager = agwpe::AgwpeManager::new(shared_state.clone(), log_tx.clone());

    let ctx = Arc::new(AppContext {
        state: shared_state,
        agwpe: agwpe_manager,
        log_tx,
    });

    let app = proxy::create_router(ctx);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;

    tracing::info!("Packet browser client starting");
    tracing::info!("Listening on http://{}", listen_addr);
    tracing::info!("AGWPE: {}:{}", config.agwpe_host, config.agwpe_port);
    tracing::info!("My callsign: {}", config.my_callsign);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install Ctrl+C handler");
    tracing::info!("Shutting down...");
}
