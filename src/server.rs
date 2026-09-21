//! The hub's HTTP surface: one route for agents to tunnel in, and the browser
//! API + UI on the rest (memo §6.1).
//!
//! Two populations, two routes:
//!
//! - `/agent/ws` — agents dial **out** and keep a WebSocket open (§5.1).
//! - everything else — the operator's browser: `/api/fleet`, `/api/agents/{id}`,
//!   the `/events` SSE fan-out, and the built Svelte bundle.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::{SplitStream, Stream, StreamExt};
use futures_util::SinkExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::services::ServeDir;

use crate::fleet::{now_ms, AgentStatus, Hub, HubEvent, Outbound, PendingMap};
use crate::protocol::{ClientFrame, Hello};

/// How long an agent has to send its `hello` before the tunnel is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the hub's router.
pub fn app(hub: Arc<Hub>) -> Router {
    let static_dir = hub.config().static_dir.clone();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/fleet", get(fleet))
        .route("/api/agents/{id}", get(agent))
        .route("/api/agents/{id}/activity", get(agent_activity))
        .route("/events", get(events))
        .route("/agent/ws", get(agent_ws))
        .fallback_service(ServeDir::new(static_dir))
        .with_state(hub)
}

/// The declared healthcheck. JSON, not bare text: Homepage's `customapi`
/// widget parses the body as JSON.
async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn fleet(State(hub): State<Arc<Hub>>) -> Json<Value> {
    Json(json!({ "agents": hub.snapshot(now_ms()) }))
}

async fn agent(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.agent_view(&id, now_ms()) {
        Some(view) => Json(json!(view)).into_response(),
        None => not_found(&id),
    }
}

async fn agent_activity(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.recent(&id) {
        Some(entries) => Json(json!({ "agentId": id, "events": entries })).into_response(),
        None => not_found(&id),
    }
}

fn not_found(id: &str) -> Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(json!({ "error": "unknown_agent", "agentId": id })),
    )
        .into_response()
}

/// The browser's live feed: a full fleet snapshot, then every hub event.
async fn events(State(hub): State<Arc<Hub>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = hub.subscribe();
    let initial = hub.snapshot(now_ms());
    let first =
        futures_util::stream::once(
            async move { Ok(sse_event(&HubEvent::Fleet { agents: initial })) },
        );
    let live = BroadcastStream::new(receiver).filter_map(|result| async move {
        // A lagged receiver has simply missed a view refresh; the next one
        // corrects it, so dropping the signal is correct, not lossy.
        result.ok().map(|event| Ok(sse_event(&event)))
    });
    Sse::new(first.chain(live)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn sse_event(event: &HubEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    Event::default().event("hub").data(data)
}

async fn agent_ws(ws: WebSocketUpgrade, State(hub): State<Arc<Hub>>) -> Response {
    ws.on_upgrade(move |socket| handle_agent(socket, hub))
}

/// Drive one agent's tunnel for its whole life. On exit the agent is marked
/// offline, so a dropped connection reads as "unreachable", never as live.
async fn handle_agent(socket: WebSocket, hub: Arc<Hub>) {
    let (mut sink, mut stream) = socket.split();

    let hello = match read_hello(&mut stream).await {
        Ok(hello) => hello,
        Err(error) => {
            eprintln!("agent tunnel rejected: {error}");
            return;
        }
    };
    let agent_id = hello.agent_id.clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let boot_id = hub.register(hello, tx, Arc::clone(&pending), now_ms());
    println!("agent {agent_id} connected (boot {boot_id})");

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let message = match frame {
                Outbound::Text(text) => Message::Text(text.into()),
                Outbound::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    let poller = spawn_status_poll(Arc::clone(&hub), agent_id.clone());

    while let Some(message) = stream.next().await {
        let message = match message {
            Ok(message) => message,
            Err(_) => break,
        };
        match message {
            Message::Text(text) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    eprintln!("agent {agent_id} sent non-JSON frame");
                    continue;
                };
                match ClientFrame::parse(&value) {
                    Ok(ClientFrame::Activity(frame)) => {
                        hub.record_activity(&agent_id, *frame, now_ms());
                    }
                    Ok(ClientFrame::Response(response)) => {
                        hub.deliver_response(&agent_id, *response);
                    }
                    Ok(ClientFrame::Log(_)) => {
                        // The log subscription is opt-in and not wired in M0.
                    }
                    Ok(ClientFrame::Hello(_)) => {
                        // A re-hello is a no-op: the tunnel is already registered.
                    }
                    Ok(ClientFrame::Unknown { type_tag }) => {
                        eprintln!("agent {agent_id} sent unknown frame type {type_tag:?}");
                    }
                    Err(error) => {
                        eprintln!("agent {agent_id} sent a malformed frame: {error}");
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
        }
    }

    poller.abort();
    hub.disconnect(&agent_id, &boot_id, now_ms());
    writer.abort();
    println!("agent {agent_id} disconnected (boot {boot_id})");
}

async fn read_hello(stream: &mut SplitStream<WebSocket>) -> anyhow::Result<Hello> {
    let message = tokio::time::timeout(HELLO_TIMEOUT, stream.next())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for hello"))?
        .ok_or_else(|| anyhow::anyhow!("tunnel closed before hello"))??;
    let Message::Text(text) = message else {
        anyhow::bail!("first frame must be a text hello");
    };
    let value: Value = serde_json::from_str(&text)?;
    match ClientFrame::parse(&value)? {
        ClientFrame::Hello(hello) => Ok(*hello),
        other => anyhow::bail!("first frame must be hello, got {other:?}"),
    }
}

/// Poll `status.get` so the fleet view has in-flight counts (§3.2, §6.3).
fn spawn_status_poll(hub: Arc<Hub>, agent_id: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_millis(hub.config().status_poll_ms.max(1));
        loop {
            match hub.request(&agent_id, "status.get", json!({})).await {
                Ok(body) => {
                    hub.apply_status(&agent_id, AgentStatus::from_body(&body), now_ms());
                }
                Err(_) => {
                    // An agent may not answer status.get; that is not fatal.
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}
