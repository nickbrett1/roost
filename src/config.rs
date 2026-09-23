//! Hub configuration, read from the environment with sane defaults.

use std::collections::HashMap;
use std::env;
use std::time::Duration;

/// Default port the hub binds inside its container (the compose file publishes
/// it host-side).
pub const DEFAULT_PORT: u16 = 3000;
/// Where the built Svelte bundle lives, relative to the working directory.
pub const DEFAULT_STATIC_DIR: &str = "web/dist";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub static_dir: String,
    /// Bounded per-agent recent-events ring (memo §5.6: a *view*, not a store).
    pub ring_size: usize,
    /// "Stuck" is `inFlight > 0` with no new event for this long (§6.3).
    /// Deliberately generous: a long tool call is not a stall.
    pub stuck_after_ms: u64,
    /// How often the hub asks a connected agent for `status.get`.
    pub status_poll_ms: u64,
    /// An explicit "yes, I meant it": when authentication is off, the hub warns
    /// at startup so an operator cannot mistake an ungated `/agent/ws` for a
    /// gated one. Setting `ROOST_AUTH_OFF_ACK` says the trade is understood, and
    /// the same message is logged as ordinary information rather than a warning.
    pub auth_off_ack: bool,
    /// How long an agent's tunnel may be **silent** before the hub drops it and
    /// marks it offline. `None` derives it from `status_poll_ms` (see
    /// [`Config::tunnel_idle`]); `Some(ms)` pins it via `ROOST_TUNNEL_IDLE_MS`.
    ///
    /// A TCP connection can die without either end noticing — a half-open
    /// socket leaves `read()` parked forever, so an agent that has silently
    /// vanished would otherwise stay "connected" until the process exits. With
    /// no heartbeat on the wire (§5), inbound silence is the only signal the
    /// hub has, and dropping the socket is what forces the agent to reconnect.
    pub tunnel_idle_ms: Option<u64>,
    /// How long a `request` waits for its `response` before giving up.
    pub request_timeout_ms: u64,
    /// How long after the hub commands a reboot an agent may stay offline before
    /// it is read as *crashed* rather than *restarting* (§5.5, §6.3). A
    /// commanded reboot is a known, bounded absence — within this window the
    /// fleet view shows `rebooting`, and past it the agent is genuinely gone.
    pub reboot_reconnect_ms: u64,
    /// Per-agent credentials, keyed by `agentId`. Empty means authentication is
    /// **off** (§7.1): the tailnet is then the only boundary, which is a choice
    /// an operator makes by not configuring any.
    pub agent_tokens: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            static_dir: DEFAULT_STATIC_DIR.to_string(),
            ring_size: 512,
            stuck_after_ms: 120_000,
            status_poll_ms: 15_000,
            auth_off_ack: false,
            tunnel_idle_ms: None,
            request_timeout_ms: 10_000,
            reboot_reconnect_ms: 120_000,
            agent_tokens: HashMap::new(),
        }
    }
}

impl Config {
    /// Read configuration from the environment, falling back to [`Config::default`].
    pub fn from_env() -> Self {
        let mut config = Config::default();
        if let Some(port) = read_u64("PORT") {
            config.port = port as u16;
        }
        if let Ok(dir) = env::var("ROOST_STATIC_DIR") {
            if !dir.is_empty() {
                config.static_dir = dir;
            }
        }
        if let Some(size) = read_u64("ROOST_RING_SIZE") {
            config.ring_size = size as usize;
        }
        if let Some(ms) = read_u64("ROOST_STUCK_AFTER_MS") {
            config.stuck_after_ms = ms;
        }
        if let Some(ms) = read_u64("ROOST_STATUS_POLL_MS") {
            config.status_poll_ms = ms;
        }
        config.auth_off_ack = read_truthy("ROOST_AUTH_OFF_ACK");
        if let Some(ms) = read_u64("ROOST_TUNNEL_IDLE_MS") {
            config.tunnel_idle_ms = Some(ms);
        }
        if let Some(ms) = read_u64("ROOST_REQUEST_TIMEOUT_MS") {
            config.request_timeout_ms = ms;
        }
        if let Some(ms) = read_u64("ROOST_REBOOT_RECONNECT_MS") {
            config.reboot_reconnect_ms = ms;
        }
        if let Ok(tokens) = env::var("ROOST_AGENT_TOKENS") {
            config.agent_tokens = parse_agent_tokens(&tokens);
        }
        config
    }

    /// Whether `/agent/ws` requires a credential (§7.1). Off exactly when no
    /// tokens are configured.
    pub fn agent_auth_enabled(&self) -> bool {
        !self.agent_tokens.is_empty()
    }

    /// How long an agent's tunnel may be silent before the hub drops it.
    ///
    /// Derived by default as three `status_poll_ms` intervals: the hub asks
    /// every connected agent for `status.get` on that cadence, so an agent that
    /// is alive answers within one interval. Missing three in a row means the
    /// tunnel is not carrying traffic, whatever the socket claims.
    pub fn tunnel_idle(&self) -> Duration {
        let ms = self
            .tunnel_idle_ms
            .unwrap_or_else(|| self.status_poll_ms.saturating_mul(3));
        Duration::from_millis(ms.max(1))
    }
    /// Whether `presented` authenticates `agent_id`.
    ///
    /// When authentication is off this is always `true` — an operator who
    /// configured no tokens has chosen the tailnet as the only boundary. When it
    /// is on, the agent must be in the map *and* the token must match: knowing
    /// some valid token does not let an agent claim another's `agentId`.
    ///
    /// The comparison is constant-time so a wrong token cannot be discovered one
    /// byte at a time. Token *length* is not hidden; length is not a secret worth
    /// hiding for a random fleet credential.
    pub fn agent_token_matches(&self, agent_id: &str, presented: Option<&str>) -> bool {
        if !self.agent_auth_enabled() {
            return true;
        }
        let Some(expected) = self.agent_tokens.get(agent_id) else {
            return false;
        };
        let Some(presented) = presented else {
            return false;
        };
        constant_time_eq(expected.as_bytes(), presented.as_bytes())
    }
}

/// Parse `ROOST_AGENT_TOKENS`: `agentId=token` entries separated by commas or
/// newlines, e.g. `mac-studio-goose=s3cret,nas-goose=other`.
///
/// An entry with no `=`, or with an empty `agentId` or token, is skipped: a
/// malformed entry must not invent an agent that authenticates with an empty
/// string. Whitespace around entries and around the `=` is ignored.
fn parse_agent_tokens(raw: &str) -> HashMap<String, String> {
    let mut tokens = HashMap::new();
    for entry in raw.split(['\n', ',']) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((agent_id, token)) = entry.split_once('=') else {
            continue;
        };
        let (agent_id, token) = (agent_id.trim(), token.trim());
        if agent_id.is_empty() || token.is_empty() {
            continue;
        }
        tokens.insert(agent_id.to_string(), token.to_string());
    }
    tokens
}

/// Compare two byte strings without an early return on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

fn read_u64(key: &str) -> Option<u64> {
    env::var(key).ok().and_then(|value| value.parse().ok())
}

/// A boolean env flag. Anything unrecognised (or absent) is false, so a typo
/// leaves the default — which for `ROOST_AUTH_OFF_ACK` means the warning still
/// prints. Fail loud, never accidentally quiet.
fn read_truthy(key: &str) -> bool {
    matches!(
        env::var(key).map(|value| value.trim().to_ascii_lowercase()),
        Ok(ref value) if matches!(value.as_str(), "1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_auth_off_warning_is_only_quiet_when_it_is_acknowledged() {
        // Absent or unrecognised: the warning still prints. A typo must not
        // silently turn an ungated /agent/ws into a quiet one.
        for value in ["", "0", "false", "no", "maybe", "y"] {
            env::set_var("ROOST_AUTH_OFF_ACK", value);
            assert!(
                !read_truthy("ROOST_AUTH_OFF_ACK"),
                "{value:?} is not an ack"
            );
        }
        // The four spellings an operator would actually reach for.
        for value in ["1", "true", "YES", " on "] {
            env::set_var("ROOST_AUTH_OFF_ACK", value);
            assert!(read_truthy("ROOST_AUTH_OFF_ACK"), "{value:?} is an ack");
        }
        env::remove_var("ROOST_AUTH_OFF_ACK");
    }

    #[test]
    fn defaults_match_the_design() {
        let config = Config::default();
        assert_eq!(config.port, 3000);
        assert_eq!(config.static_dir, "web/dist");
        assert_eq!(config.stuck_after_ms, 120_000);
    }

    #[test]
    fn the_idle_deadline_is_three_polls_by_default_and_pinnable() {
        let config = Config::default();
        // 15s poll -> 45s of silence tolerated, matching the design note.
        assert_eq!(config.tunnel_idle(), Duration::from_secs(45));

        // A pinned value wins outright.
        let pinned = Config {
            tunnel_idle_ms: Some(500),
            ..Config::default()
        };
        assert_eq!(pinned.tunnel_idle(), Duration::from_millis(500));

        // With no pin, the deadline tracks the poll interval — a hub that polls
        // rarely must not drop an agent faster than it asks.
        let slow = Config {
            status_poll_ms: 1_000,
            ..Config::default()
        };
        assert_eq!(slow.tunnel_idle(), Duration::from_secs(3));

        // Never zero: a deadline that has already expired would drop every
        // tunnel the instant it registered.
        let degenerate = Config {
            status_poll_ms: 0,
            ..Config::default()
        };
        assert_eq!(degenerate.tunnel_idle(), Duration::from_millis(1));
    }

    #[test]
    fn auth_is_off_until_a_token_is_configured() {
        let config = Config::default();
        assert!(!config.agent_auth_enabled());
        // With auth off, every agent is let through — the tailnet is the door.
        assert!(config.agent_token_matches("anything", None));
        assert!(config.agent_token_matches("anything", Some("whatever")));
    }

    #[test]
    fn a_configured_agent_needs_its_own_token() {
        let config = Config {
            agent_tokens: parse_agent_tokens("mac-studio-goose=s3cret,nas-goose=other"),
            ..Config::default()
        };
        assert!(config.agent_auth_enabled());
        assert!(config.agent_token_matches("mac-studio-goose", Some("s3cret")));
        assert!(config.agent_token_matches("nas-goose", Some("other")));
        // The wrong token, no token, and the wrong agent are all refused.
        assert!(!config.agent_token_matches("mac-studio-goose", Some("wrong")));
        assert!(!config.agent_token_matches("mac-studio-goose", None));
        assert!(!config.agent_token_matches("unknown-host", Some("s3cret")));
        // One agent's token does not authenticate another agent.
        assert!(!config.agent_token_matches("nas-goose", Some("s3cret")));
    }

    #[test]
    fn token_parsing_tolerates_whitespace_and_skips_junk() {
        let tokens = parse_agent_tokens("  a = 1 \n b=2 ,, c = , =x, d=  ");
        // `c` and `d` (empty tokens) and the empty-key entry are dropped.
        assert_eq!(tokens.get("a").map(String::as_str), Some("1"));
        assert_eq!(tokens.get("b").map(String::as_str), Some("2"));
        assert!(!tokens.contains_key("c"));
        assert!(!tokens.contains_key("d"));
        assert!(!tokens.contains_key(""));
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn an_empty_or_junk_setting_leaves_auth_off() {
        assert!(parse_agent_tokens("").is_empty());
        assert!(parse_agent_tokens("   \n  ").is_empty());
        assert!(parse_agent_tokens("no-equals-sign").is_empty());
        // A config whose only entry is malformed is a config with auth off, not
        // one that authenticates everyone against an empty token.
        let config = Config {
            agent_tokens: parse_agent_tokens("=s3cret"),
            ..Config::default()
        };
        assert!(!config.agent_auth_enabled());
    }

    #[test]
    fn the_comparison_does_not_short_circuit_on_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
