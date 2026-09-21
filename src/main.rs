//! The roost hub binary.

use std::sync::Arc;

use roost::config::Config;
use roost::fleet::Hub;
use roost::server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env();
    let port = config.port;
    let static_dir = config.static_dir.clone();

    let hub = Hub::new(config);
    let app = server::app(Arc::clone(&hub));

    // Bind all interfaces *inside* the container; the published port is what
    // decides real reachability (memo §6.1).
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("roost listening on http://0.0.0.0:{port} (serving {static_dir})");
    axum::serve(listener, app).await?;
    Ok(())
}
