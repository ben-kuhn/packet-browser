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

    // Auto-connect to AGWPE on startup
    if !config.my_callsign.is_empty() {
        tracing::info!("Attempting auto-connect to AGWPE at {}:{}", config.agwpe_host, config.agwpe_port);
        match agwpe_manager.connect_to_agwpe(config.agwpe_host.clone(), config.agwpe_port, config.my_callsign.clone()).await {
            Ok(_) => {
                tracing::info!("Successfully connected to AGWPE");
                // Query ports after successful connection
                if let Err(e) = agwpe_manager.query_ports().await {
                    tracing::warn!("Connected to AGWPE but failed to query ports: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to auto-connect to AGWPE: {}", e);
                tracing::warn!("Please start your AGWPE modem/server and verify the configuration");
            }
        }
    } else {
        tracing::info!("No callsign configured, skipping auto-connect");
    }

    let ctx = Arc::new(AppContext {
        state: shared_state,
        agwpe: agwpe_manager,
        log_tx,
    });

    let app = proxy::create_router(ctx);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    let bound = listener.local_addr().ok();

    print_startup_banner(&listen_addr, bound.as_ref());

    tracing::info!("Packet browser client starting");
    tracing::info!("Listening on http://{}", listen_addr);
    tracing::info!("AGWPE: {}:{}", config.agwpe_host, config.agwpe_port);
    tracing::info!("My callsign: {}", config.my_callsign);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn print_startup_banner(listen_addr: &str, bound: Option<&std::net::SocketAddr>) {
    // Goes through println! (not tracing) so it shows at any verbosity.
    let version = env!("CARGO_PKG_VERSION");
    let bar = "=".repeat(60);
    // Prefer the address we actually bound to (resolves :0 to a real port).
    let display = bound.map(|a| a.to_string()).unwrap_or_else(|| listen_addr.to_string());

    println!();
    println!("{}", bar);
    println!("  Packet Browser Client v{}", version);
    println!();
    println!("  Open http://{} in your browser", display);

    if let Some(addr) = bound {
        let ip = addr.ip();
        if !ip.is_loopback() {
            println!();
            println!("  WARNING: bound to {} (non-loopback address).", ip);
            println!("           Anyone who can reach this host on the network");
            println!("           can use this proxy and change its configuration.");
            println!("           Use --listen-addr 127.0.0.1:PORT to restrict it");
            println!("           to this machine only.");
        }
    }

    println!("{}", bar);
    println!();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install Ctrl+C handler");
    tracing::info!("Shutting down...");
}
