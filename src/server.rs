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
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{SplitStream, Stream, StreamExt};
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::services::ServeDir;

use crate::fleet::{
    now_ms, AgentStatus, Hub, HubEvent, Outbound, PendingMap, RebootError, RebootErrorKind,
};
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
        .route("/api/agents/{id}/history/sessions", get(history_sessions))
        .route("/api/agents/{id}/history/search", get(history_search))
        .route(
            "/api/agents/{id}/history/sessions/{session}",
            get(history_session),
        )
        .route(
            "/api/agents/{id}/history/sessions/{session}/messages",
            get(history_messages),
        )
        .route("/api/agents/{id}/reboot/preflight", get(reboot_preflight))
        .route("/api/agents/{id}/reboot", post(reboot))
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

/// A reboot's preflight (§5.5 rule 1): read-only, so the UI can arm a control
/// by naming the version it *would* install before anything is done.
async fn reboot_preflight(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.reboot_preflight(&id).await {
        Ok(preflight) => Json(json!({ "ok": true, "preflight": preflight })).into_response(),
        Err(error) => reboot_error_response(&id, error),
    }
}

/// Command a reboot (§5.5). Default-refused while turns are in flight; `force`
/// is a separate, confirmed decision, not a flag the UI sets by default.
async fn reboot(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Json(request): Json<RebootRequest>,
) -> Response {
    match hub.reboot(&id, request.force).await {
        Ok(ack) => Json(json!({ "ok": true, "reboot": ack })).into_response(),
        Err(error) => reboot_error_response(&id, error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RebootRequest {
    #[serde(default)]
    force: bool,
}

/// A refused reboot carries a status *and* a machine-readable reason, so the UI
/// distinguishes "this agent cannot reboot" from "a turn is in flight" rather
/// than showing one generic failure.
fn reboot_error_response(agent_id: &str, error: RebootError) -> Response {
    let status = match error.kind {
        RebootErrorKind::Unreachable => StatusCode::SERVICE_UNAVAILABLE,
        RebootErrorKind::Agent => StatusCode::BAD_GATEWAY,
        RebootErrorKind::Unsupported
        | RebootErrorKind::Unsupervised
        | RebootErrorKind::InFlight => StatusCode::CONFLICT,
    };
    (
        status,
        Json(json!({
            "error": error.kind.code(),
            "agentId": agent_id,
            "message": error.message,
        })),
    )
        .into_response()
}

fn not_found(id: &str) -> Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(json!({ "error": "unknown_agent", "agentId": id })),
    )
        .into_response()
}

/// `history.*` is request/response over the agent's tunnel (§5.4): the hub
/// carries the query, the agent's own schema guard answers it, and the result
/// is proxied to the browser and dropped — never cached (§5.6).
async fn history_sessions(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    proxy(&hub, &id, "history.sessions", query_params(query)).await
}

async fn history_search(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    proxy(&hub, &id, "history.search", query_params(query)).await
}

async fn history_session(
    State(hub): State<Arc<Hub>>,
    Path((id, session)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut params = query_params(query);
    params["id"] = json!(session);
    proxy(&hub, &id, "history.session", params).await
}

async fn history_messages(
    State(hub): State<Arc<Hub>>,
    Path((id, session)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut params = query_params(query);
    params["id"] = json!(session);
    proxy(&hub, &id, "history.messages", params).await
}

fn query_params(query: HashMap<String, String>) -> Value {
    Value::Object(
        query
            .into_iter()
            .map(|(key, value)| (key, json!(value)))
            .collect(),
    )
}

async fn proxy(hub: &Arc<Hub>, agent_id: &str, method: &str, params: Value) -> Response {
    let Some(view) = hub.agent_view(agent_id, now_ms()) else {
        return not_found(agent_id);
    };
    if !view.connected {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "agent_unreachable", "agentId": agent_id })),
        )
            .into_response();
    }
    match hub.request(agent_id, method, params).await {
        Ok(body) => Json(json!({ "ok": true, "body": body })).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "agent_error",
                "agentId": agent_id,
                "method": method,
                "message": error.to_string()
            })),
        )
            .into_response(),
    }
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

/// The agent tunnel (§5.1, §7.1).
///
/// A `Bearer` credential is read from the handshake and checked twice: the
/// header must be present when authentication is on (refused here, before the
/// upgrade, so an anonymous dial gets a plain `401`), and after the `hello` the
/// token must belong to the `agentId` it claims — knowing one agent's token must
/// not let it claim another's identity. Authentication is off only when no
/// `ROOST_AGENT_TOKENS` are configured.
async fn agent_ws(
    ws: WebSocketUpgrade,
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
) -> Response {
    let presented = bearer_token(&headers);
    if hub.config().agent_auth_enabled() && presented.is_none() {
        return unauthorized("missing_credential");
    }
    ws.on_upgrade(move |socket| handle_agent(socket, hub, presented))
}

/// The `Bearer` token from `Authorization`, if there is a well-formed one.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// A `401` for a dial that carries no credential.
fn unauthorized(reason: &str) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "reason": reason })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// Drive one agent's tunnel for its whole life. On exit the agent is marked
/// offline, so a dropped connection reads as "unreachable", never as live.
async fn handle_agent(socket: WebSocket, hub: Arc<Hub>, presented: Option<String>) {
    let (mut sink, mut stream) = socket.split();

    let hello = match read_hello(&mut stream).await {
        Ok(hello) => hello,
        Err(error) => {
            eprintln!("agent tunnel rejected: {error}");
            return;
        }
    };
    let agent_id = hello.agent_id.clone();

    // Identity is only known now, so this is where a token is bound to the
    // `agentId` it claims (§7.1). A mismatch is refused before registering: an
    // unauthenticated agent must never appear in the fleet.
    if !hub
        .config()
        .agent_token_matches(&agent_id, presented.as_deref())
    {
        eprintln!("agent {agent_id} rejected: credential does not match");
        let _ = sink.send(Message::Close(None)).await;
        return;
    }

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

    // A tunnel that goes silent is dropped, not trusted forever. TCP can die
    // without a FIN — a half-open socket parks `read()` indefinitely — so
    // inbound silence is the hub's only liveness signal (§5 defines no
    // heartbeat). The poller above guarantees an alive agent answers within one
    // `status_poll_ms`; `tunnel_idle` is three of those, so a genuinely live
    // agent is never dropped, while a vanished one is marked offline and, by
    // closing the socket, pushed to reconnect.
    let idle = hub.config().tunnel_idle();

    loop {
        let message = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {
                eprintln!(
                    "agent {agent_id} silent for {}s; dropping the tunnel",
                    idle.as_secs()
                );
                break;
            }
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
