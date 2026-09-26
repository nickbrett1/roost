//! `roost` — mission control for the a2a-goose fleet.
//!
//! The hub is one process with one exposed surface (memo §2). Agents dial out
//! and publish a live feed; the hub renders the fleet, and (later) proxies
//! history and commands. It is a **router, not a store**: no transcripts here.

pub mod config;
pub mod fake_agent;
pub mod fleet;
pub mod httpget;
pub mod protocol;
pub mod server;
