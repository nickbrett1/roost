//! Hub fleet state: who is connected, what they just did, and what looks stuck.
//!
//! Per memo §5.6 the hub is a **router, not a store**: everything here is
//! ephemeral, derived state — a bounded recent-events ring, last-seen `seq`,
//! in-flight counts, `status.get` snapshots. Transcripts never live here.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::Config;
use crate::protocol::{ActivityFrame, Hello, RequestFrame, ResponseFrame};

/// Wall-clock milliseconds since the Unix epoch. All ages are computed in this
/// unit so tests can pass a synthetic "now" instead of sleeping.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A frame the hub pushes to an agent down its tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Text(String),
    Close,
}

/// Correlation map for in-flight `request`/`command` frames, keyed by id.
pub type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<ResponseFrame>>>>;

/// Cloneable handles to one agent's tunnel, used to issue requests without
/// holding the fleet lock across an await.
#[derive(Clone)]
pub struct AgentHandle {
    pub agent_id: String,
    pub outbound: mpsc::UnboundedSender<Outbound>,
    pub pending: PendingMap,
}

/// What happened when an activity frame arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    Accepted,
    /// Already seen within this boot (the reconnect seam may duplicate, §3.1).
    Duplicate,
    /// Carries a boot id the hub is not currently tracking.
    StaleBoot,
    UnknownAgent,
}

/// The link/liveness state rendered in the fleet view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStateName {
    Live,
    Stuck,
    Offline,
}

/// A recent activity event, held in the agent's bounded ring.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub seq: u64,
    pub event_type: String,
    pub at: Option<String>,
    pub received_at_ms: u64,
    pub context_id: Option<String>,
    pub session_id: Option<String>,
    pub skill: Option<String>,
    pub event: Value,
}

/// The subset of the agent's `status.get` body the fleet view reads (§3.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub in_flight: u32,
    pub session_count: Option<u64>,
    pub activity_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

impl AgentStatus {
    /// Tolerant extraction: a missing or renamed field is `None`, not an error.
    pub fn from_body(body: &Value) -> Self {
        Self {
            in_flight: body
                .pointer("/acp/inFlight")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
            session_count: body.pointer("/sessions/count").and_then(Value::as_u64),
            activity_enabled: body.pointer("/activity/enabled").and_then(Value::as_bool),
            raw: Some(body.clone()),
        }
    }
}

/// The per-agent view sent to browsers and rendered by the fleet list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentView {
    pub agent_id: String,
    pub host: String,
    pub kind: String,
    pub agent_version: String,
    pub protocol_version: u32,
    pub boot_id: String,
    pub started_at: Option<String>,
    pub skills: Vec<String>,
    pub capabilities: Vec<String>,
    pub state: AgentStateName,
    pub connected: bool,
    pub stuck: bool,
    pub in_flight: u32,
    pub session_count: Option<u64>,
    pub activity_enabled: Option<bool>,
    pub last_event_at_ms: Option<u64>,
    /// The agent's own stamp for that same event. The two differ as soon as a
    /// tunnel is restored: an agent re-pushes its ring on connect, so the hub
    /// hears about an hour-old turn this second. Liveness is measured against
    /// the receipt (see `is_stuck`); this field is what the fleet view shows,
    /// because "when did it last do something" is the agent's answer, not the
    /// hub's.
    pub last_event_at: Option<String>,
    pub last_frame_at_ms: u64,
    pub connected_at_ms: u64,
    pub seconds_since_event: Option<u64>,
    pub event_count: u64,
}

/// An event fanned out to browser subscribers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubEvent {
    /// A full fleet snapshot. Small: the fleet is a handful of agents.
    Fleet { agents: Vec<AgentView> },
    Activity {
        agent_id: String,
        boot_id: String,
        entry: ActivityEntry,
    },
}

struct Agent {
    hello: Hello,
    connected: bool,
    connected_at_ms: u64,
    last_frame_at_ms: u64,
    last_event_at_ms: Option<u64>,
    last_event_at: Option<String>,
    last_seq: Option<u64>,
    event_count: u64,
    ring: VecDeque<ActivityEntry>,
    status: AgentStatus,
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    pending: PendingMap,
}

/// The hub. Cheap to clone behind an `Arc`; shared by every connection.
pub struct Hub {
    agents: std::sync::RwLock<HashMap<String, Agent>>,
    events: broadcast::Sender<HubEvent>,
    config: Config,
    next_request_id: AtomicU64,
}

impl Hub {
    pub fn new(config: Config) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            agents: std::sync::RwLock::new(HashMap::new()),
            events,
            config,
            next_request_id: AtomicU64::new(1),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Subscribe to browser-facing events.
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.events.subscribe()
    }

    /// Register (or refresh) an agent's tunnel. Returns the tracked boot id.
    pub fn register(
        &self,
        hello: Hello,
        outbound: mpsc::UnboundedSender<Outbound>,
        pending: PendingMap,
        now: u64,
    ) -> String {
        let agent_id = hello.agent_id.clone();
        let boot_id = hello.boot_id.clone();
        {
            let mut agents = self.agents.write().expect("fleet lock poisoned");
            match agents.get_mut(&agent_id) {
                Some(agent) => {
                    // A different boot is a new stream: reset the seq floor and
                    // the in-flight reading rather than reading the last boot
                    // going backwards (§5.2).
                    if agent.hello.boot_id != hello.boot_id {
                        agent.last_seq = None;
                        agent.last_event_at_ms = None;
                        agent.last_event_at = None;
                        agent.status = AgentStatus::default();
                    }
                    agent.hello = hello;
                    agent.connected = true;
                    agent.connected_at_ms = now;
                    agent.last_frame_at_ms = now;
                    agent.outbound = Some(outbound);
                    agent.pending = pending;
                }
                None => {
                    agents.insert(
                        agent_id,
                        Agent {
                            hello,
                            connected: true,
                            connected_at_ms: now,
                            last_frame_at_ms: now,
                            last_event_at_ms: None,
                            last_event_at: None,
                            last_seq: None,
                            event_count: 0,
                            ring: VecDeque::new(),
                            status: AgentStatus::default(),
                            outbound: Some(outbound),
                            pending,
                        },
                    );
                }
            }
        }
        self.broadcast_fleet(now);
        boot_id
    }

    /// Mark an agent offline. A disconnect from a *stale* boot (a reconnected
    /// agent's old socket closing late) must not clobber the new connection.
    pub fn disconnect(&self, agent_id: &str, boot_id: &str, now: u64) {
        {
            let mut agents = self.agents.write().expect("fleet lock poisoned");
            if let Some(agent) = agents.get_mut(agent_id) {
                if agent.hello.boot_id != boot_id || !agent.connected {
                    return;
                }
                agent.connected = false;
                agent.last_frame_at_ms = now;
                agent.outbound = None;
                agent.status.in_flight = 0;
            } else {
                return;
            }
        }
        self.broadcast_fleet(now);
    }

    /// Record a published activity frame, applying the `(bootId, seq)` floor.
    pub fn record_activity(&self, agent_id: &str, frame: ActivityFrame, now: u64) -> RecordOutcome {
        let entry = ActivityEntry {
            seq: frame.seq,
            event_type: frame.event_type().unwrap_or("unknown").to_string(),
            at: frame.at.clone(),
            received_at_ms: now,
            context_id: frame.context_id.clone(),
            session_id: frame.session_id.clone(),
            skill: frame.skill.clone(),
            event: frame.event,
        };
        let boot_id = frame.boot_id.clone();
        {
            let mut agents = self.agents.write().expect("fleet lock poisoned");
            let Some(agent) = agents.get_mut(agent_id) else {
                return RecordOutcome::UnknownAgent;
            };
            if !agent.connected {
                return RecordOutcome::UnknownAgent;
            }
            if frame.boot_id != agent.hello.boot_id {
                return RecordOutcome::StaleBoot;
            }
            if let Some(last) = agent.last_seq {
                if frame.seq <= last {
                    return RecordOutcome::Duplicate;
                }
            }
            agent.last_seq = Some(frame.seq);
            agent.last_event_at_ms = Some(now);
            agent.last_event_at = entry.at.clone();
            agent.last_frame_at_ms = now;
            agent.event_count += 1;
            agent.ring.push_back(entry.clone());
            let ring_size = self.config.ring_size;
            while agent.ring.len() > ring_size {
                agent.ring.pop_front();
            }
        }
        let _ = self.events.send(HubEvent::Activity {
            agent_id: agent_id.to_string(),
            boot_id,
            entry,
        });
        RecordOutcome::Accepted
    }

    /// Deliver a `response` to the `request`/`command` waiting on its id.
    pub fn deliver_response(&self, agent_id: &str, response: ResponseFrame) -> bool {
        let Some(handle) = self.handle(agent_id) else {
            return false;
        };
        let sender = handle
            .pending
            .lock()
            .expect("pending map poisoned")
            .remove(&response.id);
        match sender {
            Some(sender) => sender.send(response).is_ok(),
            None => false,
        }
    }

    /// Apply a `status.get` body.
    pub fn apply_status(&self, agent_id: &str, status: AgentStatus, now: u64) {
        {
            let mut agents = self.agents.write().expect("fleet lock poisoned");
            let Some(agent) = agents.get_mut(agent_id) else {
                return;
            };
            agent.status = status;
            agent.last_frame_at_ms = now;
        }
        self.broadcast_fleet(now);
    }

    /// The tunnel handles for an agent, if connected.
    pub fn handle(&self, agent_id: &str) -> Option<AgentHandle> {
        let agents = self.agents.read().expect("fleet lock poisoned");
        let agent = agents.get(agent_id)?;
        let outbound = agent.outbound.clone()?;
        Some(AgentHandle {
            agent_id: agent_id.to_string(),
            outbound,
            pending: Arc::clone(&agent.pending),
        })
    }

    /// Issue a `request` and await its `response` (§5.4).
    pub async fn request(
        &self,
        agent_id: &str,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let handle = self
            .handle(agent_id)
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} is not connected"))?;
        let id = format!("r-{}", self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        handle
            .pending
            .lock()
            .expect("pending map poisoned")
            .insert(id.clone(), tx);
        let frame = RequestFrame {
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        if handle
            .outbound
            .send(Outbound::Text(frame.to_value().to_string()))
            .is_err()
        {
            handle
                .pending
                .lock()
                .expect("pending map poisoned")
                .remove(&id);
            anyhow::bail!("agent {agent_id} tunnel is closed");
        }
        match tokio::time::timeout(Duration::from_millis(self.config.request_timeout_ms), rx).await
        {
            Ok(Ok(response)) => {
                if response.ok {
                    Ok(response.body.unwrap_or(Value::Null))
                } else {
                    Err(anyhow::anyhow!(
                        "{method} failed: {}",
                        response
                            .error
                            .unwrap_or_else(|| "unknown error".to_string())
                    ))
                }
            }
            Ok(Err(_)) => Err(anyhow::anyhow!("{method}: response channel dropped")),
            Err(_) => {
                handle
                    .pending
                    .lock()
                    .expect("pending map poisoned")
                    .remove(&id);
                Err(anyhow::anyhow!("{method}: timed out"))
            }
        }
    }

    /// A full fleet snapshot, sorted by agent id for stable output.
    pub fn snapshot(&self, now: u64) -> Vec<AgentView> {
        let agents = self.agents.read().expect("fleet lock poisoned");
        let mut views: Vec<AgentView> = agents
            .values()
            .map(|agent| view(agent, now, &self.config))
            .collect();
        views.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        views
    }

    /// One agent's view.
    pub fn agent_view(&self, agent_id: &str, now: u64) -> Option<AgentView> {
        let agents = self.agents.read().expect("fleet lock poisoned");
        agents
            .get(agent_id)
            .map(|agent| view(agent, now, &self.config))
    }

    /// An agent's recent-events ring (oldest first).
    pub fn recent(&self, agent_id: &str) -> Option<Vec<ActivityEntry>> {
        let agents = self.agents.read().expect("fleet lock poisoned");
        agents
            .get(agent_id)
            .map(|agent| agent.ring.iter().cloned().collect())
    }

    fn broadcast_fleet(&self, now: u64) {
        let agents = self.snapshot(now);
        let _ = self.events.send(HubEvent::Fleet { agents });
    }
}

fn view(agent: &Agent, now: u64, config: &Config) -> AgentView {
    let stuck = is_stuck(
        agent.connected,
        agent.status.in_flight,
        agent.last_event_at_ms,
        agent.connected_at_ms,
        now,
        config.stuck_after_ms,
    );
    let state = if !agent.connected {
        AgentStateName::Offline
    } else if stuck {
        AgentStateName::Stuck
    } else {
        AgentStateName::Live
    };
    AgentView {
        agent_id: agent.hello.agent_id.clone(),
        host: agent.hello.host.clone(),
        kind: agent.hello.kind.clone(),
        agent_version: agent.hello.agent_version.clone(),
        protocol_version: agent.hello.protocol_version,
        boot_id: agent.hello.boot_id.clone(),
        started_at: agent.hello.started_at.clone(),
        skills: agent.hello.skills.clone(),
        capabilities: agent.hello.capabilities.clone(),
        state,
        connected: agent.connected,
        stuck,
        in_flight: agent.status.in_flight,
        session_count: agent.status.session_count,
        activity_enabled: agent.status.activity_enabled,
        last_event_at_ms: agent.last_event_at_ms,
        last_event_at: agent.last_event_at.clone(),
        last_frame_at_ms: agent.last_frame_at_ms,
        connected_at_ms: agent.connected_at_ms,
        seconds_since_event: agent
            .last_event_at_ms
            .map(|at| now.saturating_sub(at) / 1000),
        event_count: agent.event_count,
    }
}

/// "Stuck" is `inFlight > 0` with no new event for `stuck_after_ms` (§6.3).
/// Before any event has arrived the connect time is the reference, so a fresh
/// agent reporting a turn is not instantly labelled stuck.
pub fn is_stuck(
    connected: bool,
    in_flight: u32,
    last_event_at_ms: Option<u64>,
    connected_at_ms: u64,
    now: u64,
    stuck_after_ms: u64,
) -> bool {
    connected
        && in_flight > 0
        && now.saturating_sub(last_event_at_ms.unwrap_or(connected_at_ms)) >= stuck_after_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hub() -> Arc<Hub> {
        Hub::new(Config {
            ring_size: 3,
            stuck_after_ms: 1_000,
            ..Config::default()
        })
    }

    fn hello(boot_id: &str) -> Hello {
        Hello {
            agent_id: "a2a-goose-dev".to_string(),
            host: "dev-container-3".to_string(),
            kind: "devcontainer".to_string(),
            agent_version: "0.9.1".to_string(),
            protocol_version: 1,
            boot_id: boot_id.to_string(),
            started_at: None,
            skills: vec!["ask".to_string()],
            capabilities: vec!["activity".to_string()],
        }
    }

    struct Tunnel {
        rx: mpsc::UnboundedReceiver<Outbound>,
    }

    fn connect(hub: &Arc<Hub>, boot_id: &str, now: u64) -> Tunnel {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        hub.register(hello(boot_id), tx, pending, now);
        Tunnel { rx }
    }

    fn activity(boot_id: &str, seq: u64) -> ActivityFrame {
        ActivityFrame {
            boot_id: boot_id.to_string(),
            seq,
            at: Some("2026-09-20T02:31:07.512Z".to_string()),
            context_id: Some("ctx-1".to_string()),
            task_id: None,
            session_id: None,
            skill: Some("ask".to_string()),
            event: json!({ "type": "tool_call", "id": "call_1" }),
        }
    }

    #[test]
    fn hello_registers_a_live_agent() {
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        let views = hub.snapshot(1_000);
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(view.agent_id, "a2a-goose-dev");
        assert_eq!(view.state, AgentStateName::Live);
        assert!(view.connected);
        assert!(!view.stuck);
        assert_eq!(view.event_count, 0);
    }

    #[test]
    fn activity_is_recorded_and_counted() {
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 1), 1_100),
            RecordOutcome::Accepted
        );
        let view = &hub.snapshot(1_100)[0];
        assert_eq!(view.event_count, 1);
        assert_eq!(view.last_event_at_ms, Some(1_100));
        // The view carries the agent's own stamp as well as the receipt: they
        // are the same instant here, and only diverge when a tunnel comes back
        // and the agent re-pushes an older ring.
        assert_eq!(
            view.last_event_at.as_deref(),
            Some("2026-09-20T02:31:07.512Z")
        );
        assert_eq!(
            hub.recent("a2a-goose-dev").unwrap()[0].event_type,
            "tool_call"
        );
    }

    #[test]
    fn a_re_pushed_ring_keeps_the_agents_own_timestamp() {
        // The failure this guards: the hub restarts, every agent re-pushes the
        // ring it was holding, and each frame is *received* now. If the fleet
        // view reported the receipt, a turn from an hour ago would read as
        // happening this second.
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 1), 3_600_000),
            RecordOutcome::Accepted
        );
        let view = &hub.snapshot(3_600_000)[0];
        assert_eq!(view.last_event_at_ms, Some(3_600_000));
        assert_eq!(
            view.last_event_at.as_deref(),
            Some("2026-09-20T02:31:07.512Z")
        );
    }

    #[test]
    fn duplicate_and_out_of_order_sequences_are_dropped() {
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 5), 1_100),
            RecordOutcome::Accepted
        );
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 5), 1_200),
            RecordOutcome::Duplicate
        );
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 4), 1_300),
            RecordOutcome::Duplicate
        );
        assert_eq!(hub.snapshot(1_300)[0].event_count, 1);
    }

    #[test]
    fn a_new_boot_resets_the_sequence_floor() {
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        hub.record_activity("a2a-goose-dev", activity("boot-a", 99), 1_100);
        // Reboot: seq restarts at 1, and must not read as "gone backwards".
        connect(&hub, "boot-b", 2_000);
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 100), 2_050),
            RecordOutcome::StaleBoot
        );
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-b", 1), 2_100),
            RecordOutcome::Accepted
        );
        assert_eq!(hub.snapshot(2_100)[0].event_count, 2);
        assert_eq!(hub.snapshot(2_100)[0].boot_id, "boot-b");
    }

    #[test]
    fn the_ring_is_bounded() {
        let hub = hub(); // ring_size = 3
        connect(&hub, "boot-a", 0);
        for seq in 1..=10 {
            hub.record_activity("a2a-goose-dev", activity("boot-a", seq), seq);
        }
        let ring = hub.recent("a2a-goose-dev").unwrap();
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.first().unwrap().seq, 8);
        assert_eq!(ring.last().unwrap().seq, 10);
    }

    #[test]
    fn unknown_event_types_are_stored_not_rejected() {
        let hub = hub();
        connect(&hub, "boot-a", 0);
        let mut frame = activity("boot-a", 1);
        frame.event = json!({ "type": "from_the_future" });
        assert_eq!(
            hub.record_activity("a2a-goose-dev", frame, 10),
            RecordOutcome::Accepted
        );
        assert_eq!(
            hub.recent("a2a-goose-dev").unwrap()[0].event_type,
            "from_the_future"
        );
    }

    #[test]
    fn disconnect_marks_offline_but_keeps_last_seen() {
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        hub.record_activity("a2a-goose-dev", activity("boot-a", 1), 1_100);
        hub.disconnect("a2a-goose-dev", "boot-a", 2_000);
        let view = &hub.snapshot(2_000)[0];
        assert_eq!(view.state, AgentStateName::Offline);
        assert!(!view.connected);
        assert_eq!(view.last_event_at_ms, Some(1_100));
        assert!(hub.handle("a2a-goose-dev").is_none());
        // Further activity from the dead tunnel is ignored.
        assert_eq!(
            hub.record_activity("a2a-goose-dev", activity("boot-a", 2), 2_100),
            RecordOutcome::UnknownAgent
        );
    }

    #[test]
    fn a_stale_disconnect_does_not_clobber_a_new_boot() {
        let hub = hub();
        connect(&hub, "boot-a", 1_000);
        connect(&hub, "boot-b", 2_000);
        hub.disconnect("a2a-goose-dev", "boot-a", 2_100);
        let view = &hub.snapshot(2_100)[0];
        assert!(
            view.connected,
            "the old socket closing must not evict boot-b"
        );
        assert_eq!(view.boot_id, "boot-b");
    }

    #[test]
    fn stuck_is_in_flight_with_no_movement() {
        assert!(
            !is_stuck(true, 0, Some(0), 0, 10_000, 1_000),
            "idle is not stuck"
        );
        assert!(
            !is_stuck(true, 1, Some(9_500), 0, 10_000, 1_000),
            "recent movement is not stuck"
        );
        assert!(
            is_stuck(true, 1, Some(1_000), 0, 10_000, 1_000),
            "in flight and quiet past the window is stuck"
        );
        assert!(
            !is_stuck(false, 1, Some(1_000), 0, 10_000, 1_000),
            "a disconnected agent is offline, not stuck"
        );
    }

    #[test]
    fn a_fresh_agent_with_no_events_is_not_instantly_stuck() {
        // inFlight reported, but nothing has happened yet: anchor on connect time.
        assert!(!is_stuck(true, 1, None, 12_000, 12_500, 120_000));
        assert!(is_stuck(true, 1, None, 1_000, 200_000, 120_000));
    }

    #[test]
    fn status_extraction_is_tolerant() {
        let body = json!({
            "activity": { "enabled": true, "backlog": 512 },
            "acp": { "state": "ready", "inFlight": 2 },
            "sessions": { "count": 3 }
        });
        let status = AgentStatus::from_body(&body);
        assert_eq!(status.in_flight, 2);
        assert_eq!(status.session_count, Some(3));
        assert_eq!(status.activity_enabled, Some(true));

        // A missing/renamed field degrades to None/0, never an error.
        let empty = AgentStatus::from_body(&json!({ "future": true }));
        assert_eq!(empty.in_flight, 0);
        assert_eq!(empty.session_count, None);
        assert_eq!(empty.activity_enabled, None);
    }

    #[tokio::test]
    async fn request_is_answered_by_a_response() {
        let hub = hub();
        let mut tunnel = connect(&hub, "boot-a", 0);
        let request = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.request("a2a-goose-dev", "status.get", json!({})).await }
        });
        // The outbound request frame carries a correlation id.
        let Outbound::Text(text) = tunnel.rx.recv().await.expect("a frame") else {
            panic!("expected text frame");
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "request");
        assert_eq!(value["method"], "status.get");
        let id = value["id"].as_str().unwrap().to_string();
        assert!(hub.deliver_response(
            "a2a-goose-dev",
            ResponseFrame {
                id,
                ok: true,
                body: Some(json!({ "acp": { "inFlight": 1 } })),
                error: None,
            }
        ));
        assert_eq!(request.await.unwrap().unwrap()["acp"]["inFlight"], 1);
    }

    #[tokio::test]
    async fn request_to_an_unknown_agent_fails() {
        let hub = hub();
        assert!(hub
            .request("nobody", "status.get", json!({}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn subscribers_receive_fleet_and_activity_events() {
        let hub = hub();
        let mut rx = hub.subscribe();
        connect(&hub, "boot-a", 0);
        assert!(matches!(rx.recv().await.unwrap(), HubEvent::Fleet { .. }));
        hub.record_activity("a2a-goose-dev", activity("boot-a", 1), 10);
        assert!(matches!(
            rx.recv().await.unwrap(),
            HubEvent::Activity { .. }
        ));
    }

    /// The browser reads these field names by hand, so the wire to `/events` is
    /// pinned rather than left to a rename that compiles fine on both sides and
    /// silently breaks the view. `agent_id` in particular: the drill-down
    /// matches a live event to the selected agent on it, and a `rename_all` on
    /// the enum would move it out from under that comparison without any test
    /// noticing.
    #[test]
    fn the_browser_event_shape_is_pinned() {
        let event = HubEvent::Activity {
            agent_id: "a2a-goose-dev".to_string(),
            boot_id: "boot-a".to_string(),
            entry: ActivityEntry {
                seq: 7,
                event_type: "tool_call".to_string(),
                at: Some("2026-09-22T10:00:00Z".to_string()),
                received_at_ms: 10,
                context_id: Some("ctx-1".to_string()),
                session_id: None,
                skill: Some("ask".to_string()),
                event: json!({ "type": "tool_call", "id": "call_1", "title": "shell · ls" }),
            },
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({
                "type": "activity",
                "agent_id": "a2a-goose-dev",
                "boot_id": "boot-a",
                "entry": {
                    "seq": 7,
                    "eventType": "tool_call",
                    "at": "2026-09-22T10:00:00Z",
                    "receivedAtMs": 10,
                    "contextId": "ctx-1",
                    "sessionId": null,
                    "skill": "ask",
                    "event": { "type": "tool_call", "id": "call_1", "title": "shell · ls" },
                },
            })
        );
        assert_eq!(
            serde_json::to_value(HubEvent::Fleet { agents: vec![] }).unwrap(),
            json!({ "type": "fleet", "agents": [] })
        );
    }
}
