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

    // §7.1: the per-agent credential is the boundary. When none is configured
    // the tailnet is the only door, so say so out loud rather than leave an
    // operator to assume `/agent/ws` is gated.
    if !config.agent_auth_enabled() {
        if config.auth_off_ack {
            println!("authentication off: /agent/ws accepts any agent (acknowledged by ROOST_AUTH_OFF_ACK)");
        } else {
            eprintln!(
                "WARNING: no ROOST_AGENT_TOKENS configured; /agent/ws accepts any agent \
                 (authentication off). Set ROOST_AUTH_OFF_ACK=1 to accept this silently."
            );
        }
    }

    let hub = Hub::new(config);
    let app = server::app(Arc::clone(&hub));

    // Bind all interfaces *inside* the container; the published port is what
    // decides real reachability (memo §6.1).
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("roost listening on http://0.0.0.0:{port} (serving {static_dir})");
    axum::serve(listener, app).await?;
    Ok(())
}
