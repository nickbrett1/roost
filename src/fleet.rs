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
use crate::protocol::{ActivityFrame, CommandFrame, Hello, RequestFrame, ResponseFrame};

/// The `hello` capability that makes an agent commandable (§5.2, §5.5).
pub const REBOOT_CAPABILITY: &str = "reboot";

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
    /// The hub commanded a reboot and the agent is inside the reconnect window
    /// (§5.5). Distinct from `Offline`: a restart we asked for is not a crash.
    Rebooting,
    Offline,
}

/// A reboot the hub commanded, kept so a restart it asked for renders as
/// "rebooting" rather than "crashed" (§5.5, §6.3). This is a record of the
/// hub's *own* action, not of the agent's data (§5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandedReboot {
    pub at_ms: u64,
    /// Past this, the absence is a crash again — the record has expired.
    pub deadline_ms: u64,
    pub would_install: Option<String>,
}

/// The answer to a `reboot`/`preflight` (§5.5): what the agent *would* install,
/// whether a supervisor would bring it back, and how many turns are in flight.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebootPreflight {
    pub would_install: Option<String>,
    pub supervised: bool,
    pub in_flight: u32,
}

/// The answer to a `reboot` command that was actually sent (§5.5).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebootAck {
    pub would_install: Option<String>,
    pub in_flight: u32,
    pub restarting: bool,
}

/// Why a reboot was refused. Each maps to a distinct HTTP status *and* a
/// machine-readable `error` string in the body, so the UI can say why rather
/// than offering a control that silently fails (§5.7, §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootErrorKind {
    /// The agent does not advertise the `reboot` capability.
    Unsupported,
    /// No tunnel, or the agent is not connected.
    Unreachable,
    /// `preflight` reports no supervisor: a reboot would be a silent fleet loss.
    Unsupervised,
    /// Turns are in flight and the command was not forced (§5.5 rule 3).
    InFlight,
    /// The agent answered the command with an error, or did not answer in time.
    Agent,
}

impl RebootErrorKind {
    /// The machine-readable `error` string carried in the response body.
    pub fn code(self) -> &'static str {
        match self {
            RebootErrorKind::Unsupported => "reboot_unsupported",
            RebootErrorKind::Unreachable => "agent_unreachable",
            RebootErrorKind::Unsupervised => "reboot_unsupervised",
            RebootErrorKind::InFlight => "turns_in_flight",
            RebootErrorKind::Agent => "agent_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RebootError {
    pub kind: RebootErrorKind,
    pub message: String,
}

impl RebootError {
    fn new(kind: RebootErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
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
    pub last_frame_at_ms: u64,
    pub connected_at_ms: u64,
    pub seconds_since_event: Option<u64>,
    pub event_count: u64,
    /// True while a commanded reboot is inside its reconnect window (§5.5).
    pub rebooting: bool,
    /// The version a commanded reboot would install, when one is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub would_install: Option<String>,
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
    last_seq: Option<u64>,
    event_count: u64,
    ring: VecDeque<ActivityEntry>,
    status: AgentStatus,
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    pending: PendingMap,
    /// The last reboot the hub commanded, if any (§5.5).
    commanded_reboot: Option<CommandedReboot>,
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
                        agent.status = AgentStatus::default();
                    }
                    agent.hello = hello;
                    agent.connected = true;
                    agent.connected_at_ms = now;
                    agent.last_frame_at_ms = now;
                    agent.outbound = Some(outbound);
                    agent.pending = pending;
                    // It came back: a commanded reboot is over, and a later
                    // absence is a fresh question, not this one continuing.
                    agent.commanded_reboot = None;
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
                            last_seq: None,
                            event_count: 0,
                            ring: VecDeque::new(),
                            status: AgentStatus::default(),
                            outbound: Some(outbound),
                            pending,
                            commanded_reboot: None,
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

    /// Send one outbound frame and await the `response` carrying its id.
    ///
    /// Both `request` and `command` are the same round trip on the wire (§5.4,
    /// §5.5): correlate by id, wait up to `request_timeout_ms`, and surface the
    /// agent's own error rather than inventing one.
    async fn send_await(
        &self,
        handle: &AgentHandle,
        id: String,
        text: String,
        what: &str,
    ) -> anyhow::Result<Value> {
        let (tx, rx) = oneshot::channel();
        handle
            .pending
            .lock()
            .expect("pending map poisoned")
            .insert(id.clone(), tx);
        if handle.outbound.send(Outbound::Text(text)).is_err() {
            handle
                .pending
                .lock()
                .expect("pending map poisoned")
                .remove(&id);
            anyhow::bail!("agent {} tunnel is closed", handle.agent_id);
        }
        match tokio::time::timeout(Duration::from_millis(self.config.request_timeout_ms), rx).await
        {
            Ok(Ok(response)) => {
                if response.ok {
                    Ok(response.body.unwrap_or(Value::Null))
                } else {
                    Err(anyhow::anyhow!(
                        "{what} failed: {}",
                        response
                            .error
                            .unwrap_or_else(|| "unknown error".to_string())
                    ))
                }
            }
            Ok(Err(_)) => Err(anyhow::anyhow!("{what}: response channel dropped")),
            Err(_) => {
                handle
                    .pending
                    .lock()
                    .expect("pending map poisoned")
                    .remove(&id);
                Err(anyhow::anyhow!("{what}: timed out"))
            }
        }
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
        let frame = RequestFrame {
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        self.send_await(&handle, id, frame.to_value().to_string(), method)
            .await
    }

    /// Issue a `command` and await its `response` (§5.5). Commands share the
    /// correlation map and id space with requests; only the tag differs.
    pub async fn command(
        &self,
        agent_id: &str,
        action: &str,
        mode: Option<&str>,
        force: bool,
    ) -> anyhow::Result<Value> {
        let handle = self
            .handle(agent_id)
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} is not connected"))?;
        let id = format!("c-{}", self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let frame = CommandFrame {
            id: id.clone(),
            action: action.to_string(),
            mode: mode.map(str::to_string),
            force,
        };
        self.send_await(&handle, id, frame.to_value().to_string(), action)
            .await
    }

    /// Whether an agent can be commanded to reboot at all (§5.7). A capability
    /// the agent does not advertise is a rendered state, not a failed call.
    fn ensure_rebootable(&self, agent_id: &str) -> Result<(), RebootError> {
        let agents = self.agents.read().expect("fleet lock poisoned");
        let Some(agent) = agents.get(agent_id) else {
            return Err(RebootError::new(
                RebootErrorKind::Unreachable,
                format!("unknown agent {agent_id}"),
            ));
        };
        if !agent.connected {
            return Err(RebootError::new(
                RebootErrorKind::Unreachable,
                format!("agent {agent_id} is not connected"),
            ));
        }
        if !agent
            .hello
            .capabilities
            .iter()
            .any(|c| c == REBOOT_CAPABILITY)
        {
            return Err(RebootError::new(
                RebootErrorKind::Unsupported,
                format!("agent {agent_id} does not advertise the {REBOOT_CAPABILITY} capability"),
            ));
        }
        Ok(())
    }

    /// Ask an agent what a reboot would do, without doing it (§5.5 rule 1).
    pub async fn reboot_preflight(&self, agent_id: &str) -> Result<RebootPreflight, RebootError> {
        self.ensure_rebootable(agent_id)?;
        let body = self
            .command(agent_id, "reboot", Some("preflight"), false)
            .await
            .map_err(|error| RebootError::new(RebootErrorKind::Agent, error.to_string()))?;
        Ok(preflight_from_body(&body))
    }

    /// Command a reboot, after the three rules of §5.5:
    ///
    /// 1. `preflight` first, so the version is known before anything irreversible;
    /// 2. refuse if there is no supervisor (`supervised: false`), because a reboot
    ///    the agent cannot come back from is a silent fleet loss;
    /// 3. default-refuse when turns are in flight, unless `force`.
    pub async fn reboot(&self, agent_id: &str, force: bool) -> Result<RebootAck, RebootError> {
        let preflight = self.reboot_preflight(agent_id).await?;
        if !preflight.supervised {
            return Err(RebootError::new(
                RebootErrorKind::Unsupervised,
                format!("agent {agent_id} reports no supervisor: a reboot would not come back"),
            ));
        }
        if preflight.in_flight > 0 && !force {
            return Err(RebootError::new(
                RebootErrorKind::InFlight,
                format!(
                    "agent {agent_id} has {} turn(s) in flight; force to reboot anyway",
                    preflight.in_flight
                ),
            ));
        }
        let body = self
            .command(agent_id, "reboot", Some("now"), force)
            .await
            .map_err(|error| RebootError::new(RebootErrorKind::Agent, error.to_string()))?;
        let restarting = body
            .get("restarting")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let now = now_ms();
        {
            let mut agents = self.agents.write().expect("fleet lock poisoned");
            if let Some(agent) = agents.get_mut(agent_id) {
                agent.commanded_reboot = Some(CommandedReboot {
                    at_ms: now,
                    deadline_ms: now.saturating_add(self.config.reboot_reconnect_ms),
                    would_install: preflight.would_install.clone(),
                });
            }
        }
        self.broadcast_fleet(now);
        Ok(RebootAck {
            would_install: preflight.would_install,
            in_flight: preflight.in_flight,
            restarting,
        })
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
    // A commanded reboot is a known absence: offline *within* the reconnect
    // window it asked for, and a crash only past it (§5.5, §6.3).
    let rebooting = !agent.connected
        && agent
            .commanded_reboot
            .as_ref()
            .is_some_and(|reboot| now < reboot.deadline_ms);
    let state = if !agent.connected {
        if rebooting {
            AgentStateName::Rebooting
        } else {
            AgentStateName::Offline
        }
    } else if stuck {
        AgentStateName::Stuck
    } else {
        AgentStateName::Live
    };
    let would_install = if rebooting {
        agent
            .commanded_reboot
            .as_ref()
            .and_then(|reboot| reboot.would_install.clone())
    } else {
        None
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
        last_frame_at_ms: agent.last_frame_at_ms,
        connected_at_ms: agent.connected_at_ms,
        seconds_since_event: agent
            .last_event_at_ms
            .map(|at| now.saturating_sub(at) / 1000),
        event_count: agent.event_count,
        rebooting,
        would_install,
    }
}

/// Read a `reboot`/`preflight` body into a [`RebootPreflight`] (§5.5).
///
/// Tolerant like every other agent body: a missing or renamed field degrades to
/// the *safe* answer, not an error. `supervised` defaults to **false** — an
/// agent too old to answer must not be commanded to reboot on the assumption it
/// will come back.
fn preflight_from_body(body: &Value) -> RebootPreflight {
    RebootPreflight {
        would_install: body
            .get("wouldInstall")
            .and_then(Value::as_str)
            .map(str::to_string),
        supervised: body
            .get("supervised")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        in_flight: body
            .get("inFlight")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
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

    fn hello_with(boot_id: &str, capabilities: &[&str]) -> Hello {
        let mut hello = hello(boot_id);
        hello.capabilities = capabilities.iter().map(|c| c.to_string()).collect();
        hello
    }

    fn connect_with(hub: &Arc<Hub>, hello: Hello, now: u64) -> Tunnel {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        hub.register(hello, tx, pending, now);
        Tunnel { rx }
    }

    async fn next_frame(tunnel: &mut Tunnel) -> Value {
        let Outbound::Text(text) = tunnel.rx.recv().await.expect("a frame") else {
            panic!("expected a text frame");
        };
        serde_json::from_str(&text).expect("a JSON frame")
    }

    fn answer_ok(hub: &Arc<Hub>, id: &str, body: Value) {
        assert!(hub.deliver_response(
            "a2a-goose-dev",
            ResponseFrame {
                id: id.to_string(),
                ok: true,
                body: Some(body),
                error: None,
            }
        ));
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
        assert_eq!(
            hub.recent("a2a-goose-dev").unwrap()[0].event_type,
            "tool_call"
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
    async fn command_is_answered_by_a_response() {
        let hub = hub();
        let mut tunnel = connect_with(&hub, hello_with("boot-a", &["activity", "reboot"]), 0);
        let task = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move {
                hub.command("a2a-goose-dev", "reboot", Some("preflight"), false)
                    .await
            }
        });
        let frame = next_frame(&mut tunnel).await;
        assert_eq!(frame["type"], "command");
        assert_eq!(frame["action"], "reboot");
        assert_eq!(frame["mode"], "preflight");
        let id = frame["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("c-"), "commands use their own id space");
        answer_ok(&hub, &id, json!({ "wouldInstall": "0.9.2" }));
        assert_eq!(task.await.unwrap().unwrap()["wouldInstall"], "0.9.2");
    }

    #[tokio::test]
    async fn reboot_is_refused_without_the_capability() {
        // The default test hello advertises only `activity`.
        let hub = hub();
        connect(&hub, "boot-a", 0);
        let error = hub.reboot_preflight("a2a-goose-dev").await.unwrap_err();
        assert_eq!(error.kind, RebootErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn reboot_is_refused_when_unsupervised() {
        let hub = hub();
        let mut tunnel = connect_with(&hub, hello_with("boot-a", &["reboot"]), 0);
        let task = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.reboot("a2a-goose-dev", false).await }
        });
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(
            &hub,
            &id,
            json!({ "wouldInstall": "0.9.2", "supervised": false, "inFlight": 0 }),
        );
        assert_eq!(
            task.await.unwrap().unwrap_err().kind,
            RebootErrorKind::Unsupervised
        );
    }

    #[tokio::test]
    async fn reboot_is_default_refused_in_flight_but_force_goes_through() {
        let hub = hub();
        let mut tunnel = connect_with(&hub, hello_with("boot-a", &["reboot"]), 0);

        let refused = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.reboot("a2a-goose-dev", false).await }
        });
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(
            &hub,
            &id,
            json!({ "wouldInstall": "0.9.2", "supervised": true, "inFlight": 2 }),
        );
        assert_eq!(
            refused.await.unwrap().unwrap_err().kind,
            RebootErrorKind::InFlight
        );

        let forced = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.reboot("a2a-goose-dev", true).await }
        });
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(
            &hub,
            &id,
            json!({ "wouldInstall": "0.9.2", "supervised": true, "inFlight": 2 }),
        );
        let frame = next_frame(&mut tunnel).await;
        assert_eq!(frame["mode"], "now");
        assert_eq!(frame["force"], true);
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(&hub, &id, json!({ "restarting": true }));
        let ack = forced.await.unwrap().unwrap();
        assert_eq!(ack.in_flight, 2);
        assert!(ack.restarting);
        assert_eq!(ack.would_install.as_deref(), Some("0.9.2"));
    }

    #[tokio::test]
    async fn a_commanded_reboot_reads_as_rebooting_then_expires() {
        let hub = hub();
        let mut tunnel = connect_with(&hub, hello_with("boot-a", &["reboot"]), 0);
        let task = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.reboot("a2a-goose-dev", false).await }
        });
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(
            &hub,
            &id,
            json!({ "wouldInstall": "0.9.2", "supervised": true, "inFlight": 0 }),
        );
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(&hub, &id, json!({ "restarting": true }));
        task.await.unwrap().unwrap();

        // The agent goes away to restart.
        let now = now_ms();
        hub.disconnect("a2a-goose-dev", "boot-a", now);
        let view = hub.agent_view("a2a-goose-dev", now).unwrap();
        assert_eq!(view.state, AgentStateName::Rebooting);
        assert!(view.rebooting);
        assert_eq!(view.would_install.as_deref(), Some("0.9.2"));

        // Past the window, the same absence is a crash again.
        let later = now + hub.config().reboot_reconnect_ms + 1;
        let view = hub.agent_view("a2a-goose-dev", later).unwrap();
        assert_eq!(view.state, AgentStateName::Offline);
        assert!(!view.rebooting);
    }

    #[tokio::test]
    async fn a_reconnect_clears_the_commanded_reboot() {
        let hub = hub();
        let mut tunnel = connect_with(&hub, hello_with("boot-a", &["reboot"]), 0);
        let task = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.reboot("a2a-goose-dev", false).await }
        });
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(
            &hub,
            &id,
            json!({ "wouldInstall": "0.9.2", "supervised": true, "inFlight": 0 }),
        );
        let frame = next_frame(&mut tunnel).await;
        let id = frame["id"].as_str().unwrap().to_string();
        answer_ok(&hub, &id, json!({ "restarting": true }));
        task.await.unwrap().unwrap();
        let now = now_ms();
        hub.disconnect("a2a-goose-dev", "boot-a", now);
        assert!(hub.agent_view("a2a-goose-dev", now).unwrap().rebooting);

        // It comes back on a new boot: live again, no reboot on record.
        connect_with(&hub, hello_with("boot-b", &["reboot"]), now + 10);
        let view = hub.agent_view("a2a-goose-dev", now + 10).unwrap();
        assert_eq!(view.state, AgentStateName::Live);
        assert!(!view.rebooting);
        assert_eq!(view.would_install, None);
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
