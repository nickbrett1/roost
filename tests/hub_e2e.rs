//! End-to-end: a real hub, a real fake agent, over a real WebSocket (memo §8.1).
//!
//! These tests exercise the wire the hub defines: `hello` registration, verbatim
//! `activity` publication, `status.get` request/response, stuck detection, and
//! the offline-on-disconnect transition.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use roost::config::Config;
use roost::fake_agent::{FakeAgent, Scenario};
use roost::fleet::{AgentStateName, Hub};
use roost::server;
use serde_json::json;

struct TestHub {
    hub: Arc<Hub>,
    addr: SocketAddr,
    _server: tokio::task::JoinHandle<()>,
}

async fn start_hub(config: Config) -> TestHub {
    let hub = Hub::new(config);
    let app = server::app(Arc::clone(&hub));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestHub { hub, addr, _server }
}

impl TestHub {
    fn ws_url(&self) -> String {
        format!("ws://{}/agent/ws", self.addr)
    }
}

/// Poll a predicate until it holds or the timeout expires.
async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.expect("read");
    let text = String::from_utf8_lossy(&buffer).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, body)
}

/// The whole response, headers included. Freshness is a header, so the cache
/// test cannot go through `http_get`, which throws them away.
async fn http_get_raw(addr: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.expect("read");
    String::from_utf8_lossy(&buffer).to_string()
}

/// The build stamp is only useful if the browser re-reads the page that carries
/// it, so how the bundle is cached is part of the feature, not a deployment
/// detail. Hashed assets may be cached forever; the page may not.
#[tokio::test]
async fn hashed_assets_are_immutable_but_the_page_is_revalidated() {
    let dir = std::env::temp_dir().join(format!("roost-static-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets")).expect("mkdir");
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><title>roost</title>",
    )
    .expect("index");
    std::fs::write(dir.join("assets/index-abc123.js"), "console.log(1)").expect("asset");

    let test = start_hub(Config {
        static_dir: dir.to_string_lossy().into_owned(),
        ..Config::default()
    })
    .await;

    let page = http_get_raw(test.addr, "/").await;
    assert!(
        page.starts_with("HTTP/1.1 200"),
        "index.html must be served, got: {page}"
    );
    assert!(
        page.to_lowercase().contains("cache-control: no-cache"),
        "the page must revalidate on every load, got: {page}"
    );

    let asset = http_get_raw(test.addr, "/assets/index-abc123.js").await;
    assert!(
        asset.to_lowercase().contains("immutable"),
        "a content-hashed asset may be cached forever, got: {asset}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fake_agent_registers_and_publishes_activity() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        ..Config::default()
    })
    .await;

    let agent = FakeAgent::new("a2a-goose-dev").replay_interval(Duration::from_millis(20));
    let task = tokio::spawn({
        let agent = agent.clone();
        let url = test.ws_url();
        async move { agent.run(&url).await }
    });

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || !hub.snapshot(0).is_empty()).await,
        "the agent should have registered"
    );

    let view = test.hub.agent_view("a2a-goose-dev", 0).expect("agent view");
    assert!(view.connected);
    assert_eq!(view.host, "dev-container-3");
    assert_eq!(view.kind, "devcontainer");
    assert_eq!(view.agent_version, "0.9.1");
    assert_eq!(view.protocol_version, 1);
    assert!(view.capabilities.contains(&"activity".to_string()));
    assert_eq!(view.state, AgentStateName::Live);

    // Activity is replayed and recorded verbatim.
    let hub = Arc::clone(&test.hub);
    let has_tool_call = || {
        hub.recent("a2a-goose-dev")
            .is_some_and(|entries| entries.iter().any(|entry| entry.event_type == "tool_call"))
    };
    assert!(
        wait_for(Duration::from_secs(5), has_tool_call).await,
        "the replayed tool_call should have arrived"
    );
    let recent = test.hub.recent("a2a-goose-dev").expect("ring");
    assert!(recent.iter().any(|entry| entry.event_type == "tool_call"));

    // status.get filled in the session count via the reverse tunnel.
    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .and_then(|view| view.session_count)
            .is_some())
        .await,
        "status.get should have been answered"
    );

    task.abort();
}

#[tokio::test]
async fn a_stuck_agent_is_flagged() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        stuck_after_ms: 400,
        ..Config::default()
    })
    .await;

    let agent = FakeAgent::new("a2a-goose-nas")
        .scenario(Scenario::Stuck)
        .replay_interval(Duration::from_millis(10));
    let task = tokio::spawn({
        let agent = agent.clone();
        let url = test.ws_url();
        async move { agent.run(&url).await }
    });

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-nas", roost::fleet::now_ms())
            .is_some_and(|view| view.stuck && view.in_flight > 0))
        .await,
        "an agent with inFlight > 0 and no movement should be stuck"
    );

    task.abort();
}

#[tokio::test]
async fn a_dropped_tunnel_reads_as_offline_not_live() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        ..Config::default()
    })
    .await;

    let agent = FakeAgent::new("a2a-goose-dev").replay_interval(Duration::from_millis(20));
    let task = tokio::spawn({
        let agent = agent.clone();
        let url = test.ws_url();
        async move { agent.run(&url).await }
    });

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| view.connected))
        .await,
        "the agent should connect first"
    );

    task.abort();

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| view.state == AgentStateName::Offline))
        .await,
        "a dropped tunnel must read as offline"
    );
}

#[tokio::test]
async fn the_browser_api_serves_the_fleet() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        ..Config::default()
    })
    .await;

    // The declared healthcheck stays JSON (Homepage parses it).
    let (status, body) = http_get(test.addr, "/healthz").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"status\""));

    // An unknown agent is a typed 404, not a 500.
    let (status, _) = http_get(test.addr, "/api/agents/nobody").await;
    assert_eq!(status, 404);

    let agent = FakeAgent::new("a2a-goose-dev").replay_interval(Duration::from_millis(20));
    let task = tokio::spawn({
        let agent = agent.clone();
        let url = test.ws_url();
        async move { agent.run(&url).await }
    });

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || !hub.snapshot(0).is_empty()).await,
        "the agent should register"
    );

    let (status, body) = http_get(test.addr, "/api/fleet").await;
    assert_eq!(status, 200);
    assert!(
        body.contains("a2a-goose-dev"),
        "the fleet API should list the agent, got: {body}"
    );

    let (status, body) = http_get(test.addr, "/api/agents/a2a-goose-dev/activity").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"events\""));

    task.abort();
}

#[tokio::test]
async fn a_tunnel_that_sends_activity_before_hello_is_rejected() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let test = start_hub(Config::default()).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(test.ws_url())
        .await
        .expect("connect");
    socket
        .send(Message::Text(
            r#"{"type":"activity","bootId":"x","seq":1,"event":{"type":"thought"}}"#
                .to_string()
                .into(),
        ))
        .await
        .expect("send");

    // The hub closes the tunnel without ever registering the agent.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(test.hub.snapshot(0).is_empty());
}

#[tokio::test]
async fn history_is_proxied_over_the_tunnel() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        ..Config::default()
    })
    .await;

    let agent = FakeAgent::new("a2a-goose-dev").scenario(Scenario::Idle);
    let task = tokio::spawn({
        let agent = agent.clone();
        let url = test.ws_url();
        async move { agent.run(&url).await }
    });
    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| view.connected))
        .await,
        "the agent should connect"
    );

    // Over the wire: the hub asks, the agent answers.
    let body = test
        .hub
        .request(
            "a2a-goose-dev",
            "history.sessions",
            json!({ "cwd": "/workspaces/roost" }),
        )
        .await
        .expect("history.sessions");
    assert_eq!(body["sessions"].as_array().unwrap().len(), 2);

    // Through the browser route.
    let (status, body) = http_get(
        test.addr,
        "/api/agents/a2a-goose-dev/history/sessions?cwd=/workspaces/roost",
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body.contains("sess_0001") && body.contains("sess_0002"),
        "got {body}"
    );

    // Search.
    let (status, body) = http_get(
        test.addr,
        "/api/agents/a2a-goose-dev/history/search?q=history",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("sess_0001"));

    // A paginated transcript.
    let (status, body) = http_get(
        test.addr,
        "/api/agents/a2a-goose-dev/history/sessions/sess_0001/messages?limit=2",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("nextCursor"));

    // An unknown agent never reaches the tunnel: typed 404.
    let (status, _) = http_get(test.addr, "/api/agents/nobody/history/sessions").await;
    assert_eq!(status, 404);

    task.abort();
}

#[tokio::test]
async fn history_to_an_unreachable_agent_is_503() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        ..Config::default()
    })
    .await;

    let agent = FakeAgent::new("a2a-goose-dev").scenario(Scenario::Idle);
    let task = tokio::spawn({
        let agent = agent.clone();
        let url = test.ws_url();
        async move { agent.run(&url).await }
    });
    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| view.connected))
        .await
    );
    task.abort();

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| !view.connected))
        .await,
        "the tunnel should have dropped"
    );

    let (status, _) = http_get(test.addr, "/api/agents/a2a-goose-dev/history/sessions").await;
    assert_eq!(status, 503);
}

/// A per-agent credential map, as `Config` would hold it.
fn tokens(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs
        .iter()
        .map(|(agent, token)| (agent.to_string(), token.to_string()))
        .collect()
}

/// Dial `/agent/ws`, returning `Ok` for an accepted upgrade or `Err(status)` for
/// an HTTP refusal (the pre-upgrade `401`).
async fn dial_status(addr: SocketAddr, credential: Option<&str>) -> Result<(), u16> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut request = format!("ws://{addr}/agent/ws")
        .into_client_request()
        .expect("request");
    if let Some(credential) = credential {
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {credential}")).expect("header"),
        );
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok(_) => Ok(()),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(_) => Err(0),
    }
}

/// Spawn a fake agent against the hub and hand back its task, so a test can
/// assert whether it ever registered.
fn spawn_agent(test: &TestHub, agent: FakeAgent) -> tokio::task::JoinHandle<()> {
    let url = test.ws_url();
    tokio::spawn(async move {
        let _ = agent.run(&url).await;
    })
}

/// Dial `/agent/ws`, send a `hello`, then go silent: never read, never answer.
/// This is a half-open tunnel from the hub's side — the socket is open but no
/// traffic ever crosses it again. Returns the live socket so the test can keep
/// it open (dropping it would send a FIN and change what is being tested).
async fn dial_and_go_silent(addr: SocketAddr, agent_id: &str) -> tokio::task::JoinHandle<()> {
    let hello = json!({
        "type": "hello",
        "agentId": agent_id,
        "host": "dev-container-3",
        "kind": "devcontainer",
        "agentVersion": "0.9.1",
        "protocolVersion": 1,
        "bootId": "boot-silent",
        "skills": ["ask"],
        "capabilities": ["activity"],
    });
    tokio::spawn(async move {
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/ws"))
            .await
            .expect("connect");
        use futures_util::SinkExt;
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                hello.to_string().into(),
            ))
            .await
            .expect("hello");
        // Hold the socket open and ignore everything the hub sends. `read()`
        // never returns, so nothing is ever answered.
        std::future::pending::<()>().await;
        drop(socket);
    })
}

#[tokio::test]
async fn a_silent_tunnel_is_dropped_and_the_agent_marked_offline() {
    // A short idle deadline keeps the test quick; the poll keeps firing so a
    // live agent would have answered and stayed connected.
    let test = start_hub(Config {
        status_poll_ms: 50,
        tunnel_idle_ms: Some(200),
        ..Config::default()
    })
    .await;

    let _silent = dial_and_go_silent(test.addr, "a2a-goose-dev").await;

    // It registers first: the hello is the last thing that crosses the wire.
    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| view.connected))
        .await,
        "the silent agent should have registered"
    );

    // Then the hub notices the silence and marks it offline, with no FIN from
    // the agent: the socket is open, it is simply not carrying anything.
    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| !view.connected))
        .await,
        "a tunnel that stops sending must be dropped and the agent marked offline"
    );

    let view = test.hub.agent_view("a2a-goose-dev", 0).expect("view");
    assert_eq!(view.state, AgentStateName::Offline);
}

#[tokio::test]
async fn an_anonymous_dial_is_refused_when_auth_is_on() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        agent_tokens: tokens(&[("a2a-goose-dev", "s3cret")]),
        ..Config::default()
    })
    .await;

    // No credential: refused at the handshake, before any `hello`.
    assert_eq!(dial_status(test.addr, None).await, Err(401));

    // A well-formed credential is let through the handshake.
    assert!(dial_status(test.addr, Some("s3cret")).await.is_ok());
}

#[tokio::test]
async fn an_agent_with_the_right_credential_connects() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        agent_tokens: tokens(&[("a2a-goose-dev", "s3cret")]),
        ..Config::default()
    })
    .await;

    let _task = spawn_agent(
        &test,
        FakeAgent::new("a2a-goose-dev")
            .credential("s3cret")
            .replay_interval(Duration::from_millis(20)),
    );

    let hub = Arc::clone(&test.hub);
    assert!(
        wait_for(Duration::from_secs(5), || hub
            .agent_view("a2a-goose-dev", 0)
            .is_some_and(|view| view.connected))
        .await,
        "the authenticated agent should have registered"
    );
}

#[tokio::test]
async fn a_wrong_credential_never_registers() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        agent_tokens: tokens(&[("a2a-goose-dev", "s3cret")]),
        ..Config::default()
    })
    .await;

    // Right name, wrong token: passes the handshake, then refused at the hello.
    let _task = spawn_agent(
        &test,
        FakeAgent::new("a2a-goose-dev").credential("wrong-token"),
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        test.hub.snapshot(0).is_empty(),
        "an agent with the wrong token must not appear in the fleet"
    );
}

#[tokio::test]
async fn an_agent_cannot_claim_another_agents_identity() {
    let test = start_hub(Config {
        status_poll_ms: 50,
        agent_tokens: tokens(&[("a2a-goose-dev", "s3cret")]),
        ..Config::default()
    })
    .await;

    // `s3cret` is valid, but it belongs to `a2a-goose-dev`; `nas-goose` is not
    // in the map, so the token must not let it claim that identity.
    let _task = spawn_agent(&test, FakeAgent::new("nas-goose").credential("s3cret"));

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        test.hub.agent_view("nas-goose", 0).is_none(),
        "a valid token must not authenticate a different agentId"
    );
}
