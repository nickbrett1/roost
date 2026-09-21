//! Run the fake agent against a hub (memo §8.1).
//!
//! ```sh
//! cargo run --bin fake_agent -- --scenario happy
//! cargo run --bin fake_agent -- --scenario stuck --id a2a-goose-nas
//! ```

use std::time::Duration;

use roost::fake_agent::{FakeAgent, Scenario, DEFAULT_HUB_URL, HUB_URL_ENV};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = FakeAgent::new("a2a-goose-dev");
    let mut url = std::env::var(HUB_URL_ENV).unwrap_or_else(|_| DEFAULT_HUB_URL.to_string());

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => agent.agent_id = next(&mut args, "--id")?,
            "--host" => agent.host = next(&mut args, "--host")?,
            "--kind" => agent.kind = next(&mut args, "--kind")?,
            "--boot" => agent.boot_id = next(&mut args, "--boot")?,
            "--scenario" => {
                let name = next(&mut args, "--scenario")?;
                agent.scenario = Scenario::from_name(&name).ok_or_else(|| {
                    anyhow::anyhow!("unknown scenario {name:?} (happy|stuck|idle)")
                })?;
            }
            "--hub" => url = next(&mut args, "--hub")?,
            other => anyhow::bail!("unrecognised argument {other:?}"),
        }
    }

    // Fail open, always (§5.1): an unreachable hub is a retry, not a crash.
    loop {
        match agent.run(&url).await {
            Ok(()) => println!("fake agent {}: tunnel closed", agent.agent_id),
            Err(error) => eprintln!("fake agent {}: {error}", agent.agent_id),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn next(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}
