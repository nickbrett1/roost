//! The hub <-> agent wire protocol (memo §5).
//!
//! Three verbs, matching the agent's two capabilities:
//!
//! - **publish** — agent -> hub: `hello`, `activity`, `log`.
//! - **query** — hub -> agent: `request`, answered by `response`.
//! - **command** — hub -> agent: `command`, answered by `response`.
//!
//! Two forward-compatibility rules are baked in, because fetch-latest-on-boot
//! makes version skew the normal state (§5.7):
//!
//! - Frames are dispatched on their `type` tag; an unrecognised tag becomes
//!   [`ClientFrame::Unknown`] / [`ServerFrame::Unknown`] rather than an error.
//! - The `activity` payload is kept as **opaque JSON**. The hub is a router,
//!   not a decoder, and must survive event types that did not exist when it was
//!   built (§3.1: "every consumer must ignore unknown `type` values").

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Wire protocol version carried in [`Hello`].
pub const PROTOCOL_VERSION: u32 = 1;

/// First frame an agent sends after the tunnel opens (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub agent_id: String,
    pub host: String,
    pub kind: String,
    pub agent_version: String,
    pub protocol_version: u32,
    /// Per-process boot id. Required: `seq` resets when the agent restarts, so
    /// the hub's ordering key is `(agentId, bootId, seq)`, never `(agentId, seq)`.
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// One published activity frame (§5.3). The `event` is the agent's own §3.1
/// envelope, forwarded verbatim — the hub invents no new event schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFrame {
    pub boot_id: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    pub event: Value,
}

impl ActivityFrame {
    /// The inner event's `type` discriminator, if present.
    pub fn event_type(&self) -> Option<&str> {
        self.event.get("type").and_then(Value::as_str)
    }
}

/// A bounded log tail line (§3.4, §5.3) — a separate, opt-in subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub line: String,
}

/// The answer to a `request` or `command` (§5.4, §5.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseFrame {
    pub id: String,
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A frame the hub sends an agent (§5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestFrame {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl RequestFrame {
    pub fn to_value(&self) -> Value {
        json!({
            "type": "request",
            "id": self.id,
            "method": self.method,
            "params": self.params,
        })
    }
}

/// A frame the hub sends an agent to act on the deployment (§5.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandFrame {
    pub id: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

impl CommandFrame {
    pub fn to_value(&self) -> Value {
        let mut value = json!({
            "type": "command",
            "id": self.id,
            "action": self.action,
        });
        let object = value.as_object_mut().expect("json! built an object");
        if let Some(mode) = &self.mode {
            object.insert("mode".to_string(), Value::String(mode.clone()));
        }
        if self.force {
            object.insert("force".to_string(), Value::Bool(true));
        }
        value
    }
}

/// A frame an agent sends the hub.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientFrame {
    Hello(Box<Hello>),
    Activity(Box<ActivityFrame>),
    Log(Box<LogFrame>),
    Response(Box<ResponseFrame>),
    /// A tag this hub build does not know. Ignored, never fatal (§3.1).
    Unknown {
        type_tag: String,
    },
}

impl ClientFrame {
    /// Parse a frame, dispatching on its `type` tag.
    pub fn parse(value: &Value) -> anyhow::Result<Self> {
        let type_tag = value.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(match type_tag {
            "hello" => ClientFrame::Hello(Box::new(serde_json::from_value(value.clone())?)),
            "activity" => ClientFrame::Activity(Box::new(serde_json::from_value(value.clone())?)),
            "log" => ClientFrame::Log(Box::new(serde_json::from_value(value.clone())?)),
            "response" => ClientFrame::Response(Box::new(serde_json::from_value(value.clone())?)),
            _ => ClientFrame::Unknown {
                type_tag: type_tag.to_string(),
            },
        })
    }
}

/// A frame the hub sends an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    Request(Box<RequestFrame>),
    Command(Box<CommandFrame>),
    Unknown { type_tag: String },
}

impl ServerFrame {
    pub fn parse(value: &Value) -> anyhow::Result<Self> {
        let type_tag = value.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(match type_tag {
            "request" => ServerFrame::Request(Box::new(serde_json::from_value(value.clone())?)),
            "command" => ServerFrame::Command(Box::new(serde_json::from_value(value.clone())?)),
            _ => ServerFrame::Unknown {
                type_tag: type_tag.to_string(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_parses_the_memo_shape() {
        let raw = json!({
            "type": "hello",
            "agentId": "a2a-goose-dev",
            "host": "dev-container-3",
            "kind": "devcontainer",
            "agentVersion": "0.9.1",
            "protocolVersion": 1,
            "bootId": "7f3c1a9e",
            "startedAt": "2026-09-20T02:30:00Z",
            "skills": ["ask"],
            "capabilities": ["activity", "history", "logs", "reboot"],
            "somethingNew": true
        });
        let ClientFrame::Hello(hello) = ClientFrame::parse(&raw).expect("parses") else {
            panic!("expected hello");
        };
        assert_eq!(hello.agent_id, "a2a-goose-dev");
        assert_eq!(hello.boot_id, "7f3c1a9e");
        assert_eq!(
            hello.capabilities,
            vec!["activity", "history", "logs", "reboot"]
        );
    }

    #[test]
    fn activity_keeps_the_event_opaque() {
        let raw = json!({
            "type": "activity",
            "bootId": "7f3c1a9e",
            "seq": 42,
            "at": "2026-09-20T02:31:07.512Z",
            "contextId": "ctx-1",
            "event": { "type": "tool_call", "id": "call_1", "title": "Read src/lib.rs" }
        });
        let ClientFrame::Activity(frame) = ClientFrame::parse(&raw).expect("parses") else {
            panic!("expected activity");
        };
        assert_eq!(frame.seq, 42);
        assert_eq!(frame.context_id.as_deref(), Some("ctx-1"));
        assert_eq!(frame.event_type(), Some("tool_call"));
        assert_eq!(frame.event["title"], "Read src/lib.rs");
    }

    #[test]
    fn unknown_event_type_is_carried_not_rejected() {
        let raw = json!({
            "type": "activity",
            "bootId": "b",
            "seq": 1,
            "event": { "type": "from_the_future", "payload": 1 }
        });
        let ClientFrame::Activity(frame) = ClientFrame::parse(&raw).expect("parses") else {
            panic!("expected activity");
        };
        assert_eq!(frame.event_type(), Some("from_the_future"));
    }

    #[test]
    fn unknown_frame_type_is_ignored() {
        let raw = json!({ "type": "subscribe_ack", "whatever": 1 });
        assert_eq!(
            ClientFrame::parse(&raw).expect("parses"),
            ClientFrame::Unknown {
                type_tag: "subscribe_ack".to_string()
            }
        );
    }

    #[test]
    fn malformed_known_frame_is_an_error() {
        // hello without the required bootId must fail loudly, not silently.
        let raw = json!({ "type": "hello", "agentId": "x" });
        assert!(ClientFrame::parse(&raw).is_err());
    }

    #[test]
    fn request_round_trips() {
        let frame = RequestFrame {
            id: "r-1".to_string(),
            method: "history.sessions".to_string(),
            params: json!({ "cwd": "/workspaces/x" }),
        };
        let value = frame.to_value();
        assert_eq!(value["type"], "request");
        let ServerFrame::Request(parsed) = ServerFrame::parse(&value).expect("parses") else {
            panic!("expected request");
        };
        assert_eq!(*parsed, frame);
    }

    #[test]
    fn command_serialises_mode_and_force() {
        let value = CommandFrame {
            id: "c-1".to_string(),
            action: "reboot".to_string(),
            mode: Some("preflight".to_string()),
            force: true,
        }
        .to_value();
        assert_eq!(value["type"], "command");
        assert_eq!(value["action"], "reboot");
        assert_eq!(value["mode"], "preflight");
        assert_eq!(value["force"], true);

        // Absent fields are omitted, so a minimal command stays minimal.
        let plain = CommandFrame {
            id: "c-2".to_string(),
            action: "reboot".to_string(),
            mode: None,
            force: false,
        }
        .to_value();
        assert!(plain.get("mode").is_none());
        assert!(plain.get("force").is_none());
    }

    #[test]
    fn response_parses_the_memo_shape() {
        let raw = json!({ "type": "response", "id": "r-1", "ok": true, "body": { "acp": {} } });
        let ClientFrame::Response(response) = ClientFrame::parse(&raw).expect("parses") else {
            panic!("expected response");
        };
        assert!(response.ok);
        assert_eq!(response.id, "r-1");
    }
}
