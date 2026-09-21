//! A fake a2a-goose agent for the hub to be built against (memo §8.1).
//!
//! The hub defines the wire contract, so it is built first — against this, not
//! against a real agent. The fake completes the `hello`, replays a
//! captured-shaped `/events` stream, and answers `status.get` / `sessions.list`
//! / `history.*` with fixtures. The replayed events are literal JSON authored
//! from §3.1, so the schema the hub consumes is the real one, not a second one
//! invented to match the hub's types.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{ActivityFrame, Hello, RequestFrame, ServerFrame, PROTOCOL_VERSION};

/// Environment variable the fake agent reads its hub URL from.
pub const HUB_URL_ENV: &str = "ROOST_HUB_URL";
/// Where the hub terminates agent tunnels.
pub const DEFAULT_HUB_URL: &str = "ws://127.0.0.1:3000/agent/ws";

/// What the fake agent pretends to be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Replays the fixture stream forever; `inFlight` 0. A healthy agent.
    Happy,
    /// Sends a few events then goes quiet with `inFlight` 1 — the stuck case.
    Stuck,
    /// Sends no activity and reports `inFlight` 0. A genuinely idle agent.
    Idle,
}

impl Scenario {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "happy" => Some(Scenario::Happy),
            "stuck" => Some(Scenario::Stuck),
            "idle" => Some(Scenario::Idle),
            _ => None,
        }
    }
}

/// A configurable fake agent.
#[derive(Debug, Clone)]
pub struct FakeAgent {
    pub agent_id: String,
    pub host: String,
    pub kind: String,
    pub agent_version: String,
    pub boot_id: String,
    pub scenario: Scenario,
    pub replay_interval: Duration,
}

impl FakeAgent {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            host: "dev-container-3".to_string(),
            kind: "devcontainer".to_string(),
            agent_version: "0.9.1".to_string(),
            boot_id: "7f3c1a9e".to_string(),
            scenario: Scenario::Happy,
            replay_interval: Duration::from_millis(250),
        }
    }

    pub fn scenario(mut self, scenario: Scenario) -> Self {
        self.scenario = scenario;
        self
    }

    pub fn boot_id(mut self, boot_id: impl Into<String>) -> Self {
        self.boot_id = boot_id.into();
        self
    }

    pub fn replay_interval(mut self, interval: Duration) -> Self {
        self.replay_interval = interval;
        self
    }

    /// The `hello` this agent sends.
    pub fn hello(&self) -> Hello {
        Hello {
            agent_id: self.agent_id.clone(),
            host: self.host.clone(),
            kind: self.kind.clone(),
            agent_version: self.agent_version.clone(),
            protocol_version: PROTOCOL_VERSION,
            boot_id: self.boot_id.clone(),
            started_at: Some("2026-09-20T02:30:00Z".to_string()),
            skills: vec!["ask".to_string()],
            capabilities: vec![
                "activity".to_string(),
                "history".to_string(),
                "logs".to_string(),
                "reboot".to_string(),
            ],
        }
    }

    /// Connect, greet, replay, and answer requests until the tunnel closes.
    pub async fn run(&self, url: &str) -> anyhow::Result<()> {
        let (socket, _response) = tokio_tungstenite::connect_async(url).await?;
        let (mut sink, mut stream) = socket.split();

        sink.send(Message::Text(hello_frame(&self.hello()).to_string().into()))
            .await?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let _writer = AbortOnDrop(tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        }));

        let _replay = AbortOnDrop(self.spawn_replay(out_tx.clone()));

        while let Some(message) = stream.next().await {
            let message = message?;
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Ok(frame) = ServerFrame::parse(&value) else {
                continue;
            };
            let response = match frame {
                ServerFrame::Request(request) => {
                    let body = self.answer(&request);
                    response_frame(request.id, body)
                }
                ServerFrame::Command(command) => {
                    let body = self.command_body(&command.action, command.mode.as_deref());
                    response_frame(command.id, body)
                }
                ServerFrame::Unknown { .. } => continue,
            };
            if out_tx
                .send(Message::Text(response.to_string().into()))
                .is_err()
            {
                break;
            }
        }

        Ok(())
    }

    fn spawn_replay(&self, out: mpsc::UnboundedSender<Message>) -> tokio::task::JoinHandle<()> {
        let boot_id = self.boot_id.clone();
        let scenario = self.scenario;
        let interval = self.replay_interval;
        tokio::spawn(async move {
            let events = fixture_events();
            let limit = match scenario {
                Scenario::Happy => events.len(),
                Scenario::Stuck => 3,
                Scenario::Idle => 0,
            };
            let mut seq = 0u64;
            loop {
                for event in events.iter().take(limit) {
                    seq += 1;
                    let frame = ActivityFrame {
                        boot_id: boot_id.clone(),
                        seq,
                        at: None,
                        context_id: Some("ctx-1".to_string()),
                        task_id: Some("task-1".to_string()),
                        session_id: Some("sess_0001".to_string()),
                        skill: Some("ask".to_string()),
                        event: event.clone(),
                    };
                    if out
                        .send(Message::Text(activity_frame(&frame).to_string().into()))
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(interval).await;
                }
                // Only the happy agent keeps a turn running forever; the others
                // go quiet, which is what "stuck" and "idle" mean.
                if scenario != Scenario::Happy {
                    return;
                }
            }
        })
    }

    fn answer(&self, request: &RequestFrame) -> Value {
        match request.method.as_str() {
            "status.get" => self.status_body(),
            "sessions.list" => json!({
                "sessions": [{
                    "contextId": "ctx-1",
                    "sessionId": "sess_0001",
                    "cwd": "/workspaces/roost",
                    "skillId": "ask",
                    "idleSecs": 3,
                    "inFlight": 0,
                    "retained": true
                }]
            }),
            "history.sessions" => history_sessions(&request.params),
            "history.session" => history_session(&request.params),
            "history.messages" => history_messages(&request.params),
            "history.search" => history_search(&request.params),
            "logs.tail" => json!({ "lines": [] }),
            _ => json!({}),
        }
    }

    fn status_body(&self) -> Value {
        let in_flight = if self.scenario == Scenario::Stuck {
            1
        } else {
            0
        };
        json!({
            "activity": { "enabled": true, "backlog": 512, "subscribers": 1 },
            "acp": { "state": "ready", "inFlight": in_flight, "pid": 4321, "restarts": 0 },
            "sessions": { "count": 2, "retained": 2 },
            "registry": { "registered": true }
        })
    }

    fn command_body(&self, action: &str, mode: Option<&str>) -> Value {
        match (action, mode) {
            ("reboot", Some("preflight")) => {
                json!({ "wouldInstall": "0.9.2", "supervised": true, "inFlight": 0 })
            }
            ("reboot", _) => json!({ "restarting": true }),
            _ => json!({}),
        }
    }
}

/// An async task that is aborted when this guard is dropped.
///
/// `run` spawns a writer and a replay task; without this, cancelling the `run`
/// future (as a test or the operator might) would leak those tasks and hold the
/// socket open, so a dropped tunnel would never read as offline.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Add the `type` discriminator to a typed frame.
fn hello_frame(hello: &Hello) -> Value {
    tagged("hello", hello)
}

fn activity_frame(frame: &ActivityFrame) -> Value {
    tagged("activity", frame)
}

fn tagged(kind: &str, value: &impl serde::Serialize) -> Value {
    let mut value = serde_json::to_value(value).expect("frame is serialisable");
    value
        .as_object_mut()
        .expect("frame is an object")
        .insert("type".to_string(), Value::String(kind.to_string()));
    value
}

/// A `response` frame answering a `request` or `command`.
fn response_frame(id: String, body: Value) -> Value {
    json!({ "type": "response", "id": id, "ok": true, "body": body })
}

/// The replayed `/events` bodies, authored from §3.1.
pub fn fixture_events() -> Vec<Value> {
    vec![
        json!({ "type": "request_received", "cwd": "/workspaces/roost", "promptBytes": 128 }),
        json!({ "type": "turn_started", "sessionId": "sess_0001", "reused": false }),
        json!({ "type": "thought", "text": "Let me look at the repo structure." }),
        json!({ "type": "plan", "entries": ["read the memo", "scaffold the hub"] }),
        json!({
            "type": "tool_call",
            "id": "call_1",
            "title": "Read src/main.rs",
            "toolKind": "read",
            "status": "in_progress"
        }),
        json!({ "type": "tool_call_update", "id": "call_1", "status": "completed" }),
        json!({ "type": "answer", "deltaBytes": 42 }),
        json!({ "type": "usage", "used": 12_000, "size": 200_000 }),
        json!({
            "type": "finished",
            "stopReason": "end_turn",
            "totalTokens": 1_234,
            "inputTokens": 1_000,
            "outputTokens": 234,
            "contextTokens": 12_000
        }),
    ]
}

/// A small, stable transcript corpus for the hub's History mode (memo §4.5).
fn fixture_sessions() -> Vec<Value> {
    vec![
        json!({
            "sessionId": "sess_0001",
            "name": "Scaffold the roost hub",
            "workingDir": "/workspaces/roost",
            "createdAt": "2026-09-20T02:00:00Z",
            "updatedAt": "2026-09-20T02:31:07Z",
            "messageCount": 5,
            "tokens": 18_432,
            "cost": 0.42
        }),
        json!({
            "sessionId": "sess_0002",
            "name": "Fix the docker publish plugin path",
            "workingDir": "/workspaces/roost",
            "createdAt": "2026-09-21T12:00:00Z",
            "updatedAt": "2026-09-21T12:55:00Z",
            "messageCount": 3,
            "tokens": 9_012,
            "cost": 0.19
        }),
        json!({
            "sessionId": "sess_0003",
            "name": "Review the mission control memo",
            "workingDir": "/workspaces/a2a-goose",
            "createdAt": "2026-09-19T09:00:00Z",
            "updatedAt": "2026-09-19T09:40:00Z",
            "messageCount": 4,
            "tokens": 33_210,
            "cost": 0.77
        }),
    ]
}

fn fixture_messages(session_id: &str) -> Vec<Value> {
    let lines: Vec<(&str, &str)> = match session_id {
        "sess_0002" => vec![
            (
                "user",
                "The publish step dies with unknown flag: --bootstrap.",
            ),
            (
                "assistant",
                "DOCKER_CONFIG relocates the cli-plugins directory.",
            ),
            (
                "assistant",
                "Link $HOME/.docker/cli-plugins back into the per-job config.",
            ),
        ],
        "sess_0003" => vec![
            ("user", "Read the mission control memo."),
            (
                "assistant",
                "The agent renders nothing; the hub is the only UI.",
            ),
            ("user", "What about history?"),
            (
                "assistant",
                "goose's sessions.db is the record; the hub queries it over the wire.",
            ),
        ],
        _ => vec![
            ("user", "Add a fake agent to roost."),
            ("assistant", "Starting with the wire protocol."),
            ("tool", "Read src/main.rs"),
            (
                "assistant",
                "The hub registers agents and fans out activity.",
            ),
            ("user", "Now add history."),
        ],
    };
    lines
        .iter()
        .enumerate()
        .map(|(index, (role, text))| {
            json!({
                "index": index,
                "role": role,
                "createdAt": format!("2026-09-20T02:0{index}:00Z"),
                "text": text
            })
        })
        .collect()
}

/// `history.sessions { cwd, q, limit }` — summaries, newest first.
fn history_sessions(params: &Value) -> Value {
    let cwd = params.get("cwd").and_then(Value::as_str);
    let query = params
        .get("q")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let sessions: Vec<Value> = fixture_sessions()
        .into_iter()
        .filter(|session| cwd.is_none_or(|cwd| session["workingDir"] == cwd))
        .filter(|session| {
            query.as_ref().is_none_or(|q| {
                session["name"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase().contains(q))
            })
        })
        .take(limit)
        .collect();
    json!({ "sessions": sessions, "nextCursor": null })
}

/// `history.session { id }` — one session's metadata plus its turns.
fn history_session(params: &Value) -> Value {
    let id = params.get("id").and_then(Value::as_str).unwrap_or("");
    match fixture_sessions()
        .into_iter()
        .find(|session| session["sessionId"] == id)
    {
        Some(session) => json!({ "session": session, "messages": fixture_messages(id) }),
        None => json!({ "session": null }),
    }
}

/// `history.messages { id, cursor, limit }` — paginated transcript.
fn history_messages(params: &Value) -> Value {
    let id = params.get("id").and_then(Value::as_str).unwrap_or("");
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(2) as usize;
    let start = params
        .get("cursor")
        .and_then(Value::as_str)
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0);
    let all = fixture_messages(id);
    let page: Vec<Value> = all.iter().skip(start).take(limit).cloned().collect();
    let end = start + page.len();
    let next_cursor = if end < all.len() {
        Value::String(end.to_string())
    } else {
        Value::Null
    };
    json!({ "sessionId": id, "messages": page, "nextCursor": next_cursor })
}

/// `history.search { q, limit }` — matches across sessions, grouped by session.
fn history_search(params: &Value) -> Value {
    let query = params
        .get("q")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    if query.is_empty() {
        return json!({ "matches": [] });
    }
    let mut matches = Vec::new();
    for session in fixture_sessions() {
        let id = session["sessionId"].as_str().unwrap_or("");
        let hits: Vec<Value> = fixture_messages(id)
            .into_iter()
            .filter(|message| {
                message["text"]
                    .as_str()
                    .is_some_and(|text| text.to_lowercase().contains(&query))
            })
            .collect();
        if !hits.is_empty() {
            matches.push(json!({
                "sessionId": id,
                "name": session["name"],
                "matches": hits
            }));
        }
    }
    matches.truncate(limit);
    json!({ "matches": matches })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scenario_names_round_trip() {
        assert_eq!(Scenario::from_name("stuck"), Some(Scenario::Stuck));
        assert_eq!(Scenario::from_name("nope"), None);
    }

    #[test]
    fn fixtures_cover_the_documented_event_types() {
        let types: Vec<String> = fixture_events()
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "request_received",
            "turn_started",
            "tool_call",
            "tool_call_update",
            "thought",
            "plan",
            "answer",
            "usage",
            "finished",
        ] {
            assert!(types.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn status_body_reports_in_flight_only_when_stuck() {
        assert_eq!(
            FakeAgent::new("a").scenario(Scenario::Stuck).status_body()["acp"]["inFlight"],
            1
        );
        assert_eq!(
            FakeAgent::new("a").scenario(Scenario::Happy).status_body()["acp"]["inFlight"],
            0
        );
    }

    #[test]
    fn history_sessions_filters_by_cwd_and_query() {
        assert_eq!(
            history_sessions(&json!({}))["sessions"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            history_sessions(&json!({ "cwd": "/workspaces/roost" }))["sessions"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            history_sessions(&json!({ "q": "docker" }))["sessions"][0]["sessionId"],
            "sess_0002"
        );
    }

    #[test]
    fn history_messages_paginate_with_a_cursor() {
        let first = history_messages(&json!({ "id": "sess_0001", "limit": 2 }));
        assert_eq!(first["messages"].as_array().unwrap().len(), 2);
        assert_eq!(first["nextCursor"], "2");

        let last = history_messages(&json!({ "id": "sess_0001", "cursor": "4", "limit": 2 }));
        assert_eq!(last["messages"].as_array().unwrap().len(), 1);
        assert!(last["nextCursor"].is_null());
    }

    #[test]
    fn history_search_groups_matches_by_session() {
        let found = history_search(&json!({ "q": "history" }));
        let matches = found["matches"].as_array().unwrap();
        assert!(matches
            .iter()
            .any(|entry| entry["sessionId"] == "sess_0001"));
        assert!(history_search(&json!({ "q": "" }))["matches"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn history_session_misses_cleanly() {
        assert!(history_session(&json!({ "id": "nope" }))["session"].is_null());
        assert_eq!(
            history_session(&json!({ "id": "sess_0003" }))["session"]["name"],
            "Review the mission control memo"
        );
    }

    #[test]
    fn hello_frame_carries_the_type_tag() {
        let value = hello_frame(&FakeAgent::new("x").hello());
        assert_eq!(value["type"], "hello");
        assert_eq!(value["agentId"], "x");
        assert_eq!(value["protocolVersion"], PROTOCOL_VERSION);
    }
}
