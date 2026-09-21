//! Hub configuration, read from the environment with sane defaults.

use std::env;

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
    /// How long a `request` waits for its `response` before giving up.
    pub request_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            static_dir: DEFAULT_STATIC_DIR.to_string(),
            ring_size: 512,
            stuck_after_ms: 120_000,
            status_poll_ms: 15_000,
            request_timeout_ms: 10_000,
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
        if let Some(ms) = read_u64("ROOST_REQUEST_TIMEOUT_MS") {
            config.request_timeout_ms = ms;
        }
        config
    }
}

fn read_u64(key: &str) -> Option<u64> {
    env::var(key).ok().and_then(|value| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_design() {
        let config = Config::default();
        assert_eq!(config.port, 3000);
        assert_eq!(config.static_dir, "web/dist");
        assert_eq!(config.stuck_after_ms, 120_000);
    }
}
