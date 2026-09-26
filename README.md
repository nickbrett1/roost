# roost

Mission control for the a2a-goose fleet: one hub that shows every agent, what
each one is doing right now, and (later) lets you act on one without visiting
its host. The design is the memo *"Mission control for the a2a-goose fleet"*;
this repository is its implementation, built hub-first against a **fake agent**
so the wire contract is defined before any real agent changes.

## The shape

- **The hub** (this repo, `roost`) — one process, one exposed surface, the only
  UI: the fleet view, history browsing, and control.
- **The agent** — provides data and can be restarted. It renders nothing.

Every agent **dials out** to the hub and keeps one long-lived WebSocket open
(`/agent/ws`). That gives agent→hub push (the live feed) and hub→agent
request/response (history, reboot) on one connection, with no per-host listen
surface. The hub is a **router, not a store**: transcripts stay in goose's
`sessions.db` on their own host and are only ever proxied.

### The wire (three verbs, memo §5)

| direction | frame | purpose |
| --- | --- | --- |
| agent → hub | `hello` | identity: `agentId`, `host`, `kind`, `agentVersion`, `protocolVersion`, `bootId`, `skills`, `capabilities` |
| agent → hub | `activity` | one §3.1 `/events` envelope, forwarded verbatim, tagged with `bootId` |
| agent → hub | `log` | bounded log tail (opt-in; not wired in M0) |
| hub → agent | `request` | `status.get`, `sessions.list`, `history.*`, `logs.tail` — answered by `response` |
| hub → agent | `command` | `reboot` with `mode: "preflight"` (M4) — answered by `response` |

Ordering is `(agentId, bootId, seq)`: `seq` resets when an agent restarts, so a
reboot is a new stream, never the last one going backwards. Unknown frame and
event types are ignored, never fatal — version skew is the normal state.

## The hub's surfaces

- `/agent/ws` — agents tunnel in (outbound from the agent).
- `/api/fleet` — the fleet snapshot (JSON).
- `/api/agents/{id}` and `/api/agents/{id}/activity` — one agent, and its recent
  bounded ring.
- `/events` — the browser's SSE fan-out (`event: hub`).
- `/healthz` — JSON healthcheck (Homepage parses the body).
- everything else — the built Svelte bundle in `web/dist`.

## Running it locally

```sh
# terminal 1 — the hub (serves web/dist, so build it first)
cd web && npm install && npm run build && cd ..
cargo run

# terminal 2 — a fake agent (memo §8.1)
cargo run --bin fake_agent -- --scenario happy
cargo run --bin fake_agent -- --scenario stuck --id a2a-goose-nas
```

Then open <http://127.0.0.1:3000/>. Configuration is environment-driven:
`PORT`, `ROOST_STATIC_DIR`, `ROOST_STUCK_AFTER_MS`, `ROOST_STATUS_POLL_MS`,
`ROOST_RING_SIZE`, `ROOST_TUNNEL_IDLE_MS`, `ROOST_AUTH_OFF_ACK`. The fake agent reads `ROOST_HUB_URL`.

`ROOST_AGENT_TOKENS` gates the agent tunnel (§7.1). It is a comma-separated map
of `agentId=token`, e.g.:

```sh
ROOST_AGENT_TOKENS='mac-studio-goose=s3cret,nas-goose=other' cargo run
```

When it is set, a dial to `/agent/ws` must carry `Authorization: Bearer <token>`
and the token must belong to the `agentId` the `hello` claims — one agent's token
does not authenticate another's identity. When it is **unset**, authentication is
off: the tailnet is then the only boundary, and the hub says so on startup—
a `WARNING` by default, or a plain line if `ROOST_AUTH_OFF_ACK` is set to an
acknowledgement (`1`/`true`/`yes`/`on`), which records the decision without
hiding it. An unrecognised value is not an acknowledgement, so a typo still
warns.

## Tests

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked            # unit + end-to-end (real hub + real fake agent)
cd web && npm test             # vitest, coverage-gated
```

The end-to-end tests in `tests/hub_e2e.rs` stand a real hub up on an ephemeral
port and drive it with the fake agent over a real WebSocket.

## Status

M0 — the hub, standing up: the wire's `hello` + `activity` + `request` frames,
the fleet view with liveness and stuck detection, and the fake agent. Read-only.

M1 — history: `history.*` over the agent's tunnel, the sessions/search/transcript
drill-down, and the hub-side proxy routes.

M2a — real-agent conformance: the `a2a-goose` tunnel client (dial out, `hello`,
publish the activity feed, answer `status.get` / `sessions.list`), and **agent
authentication** here on the hub: `/agent/ws` requires a per-agent `Bearer` token
when `ROOST_AGENT_TOKENS` is set.

M2b — the real agent answers `history.*` from goose's `sessions.db` on its own
host (`a2a-goose` side); the hub's proxy routes were already in place.

Still to come: commands (M4).

A tunnel is only trusted while it is **carrying traffic**. TCP can die without a
FIN, and a half-open socket parks `read()` forever — an agent that vanished would
otherwise read as `connected` until its process exited, which is the exact
failure mission control exists to surface. The hub already asks every agent for
`status.get` on a cadence (`ROOST_STATUS_POLL_MS`, default 15s), so a live agent
answers within one interval; after three of them with nothing inbound the hub
drops the socket and marks the agent offline (`ROOST_TUNNEL_IDLE_MS` pins the
deadline; it defaults to 3× the poll interval). Closing the socket is what pushes
the agent to reconnect.

The live turn view is in: a drill-down opens on the agent's activity ring
(`/api/agents/{id}/activity`) and then follows the `/events` fan-out, so it is
populated on arrival rather than waiting for the agent to happen to speak. The
wire is deliberately high-frequency — a turn is thousands of single-token
`thought` and `answer` frames — so the view folds them: consecutive deltas in one
context join into one line, a `tool_call` and its later updates are one row, and
any other frame ends the current run. The hub stays a router: coalescing is the
view's job and the hub holds no more than its bounded ring.

## Capabilities

This project includes the following capabilities:

- **Docker**: Adds Docker support for containerised builds and tooling.
- **Node.js DevContainer**: Sets up a VS Code DevContainer with Node.js environment.
- **Svelte**: Initializes a Svelte 5 + Vite frontend built to static assets. Frontend only: no adapter, no server routes, no server of any kind. The project's primary-language server serves the built files.
- **Rust DevContainer**: Sets up a VS Code DevContainer with Rust environment.
- **Docker Container**: Containerize the project and publish to the GitHub Container Registry (GHCR) for deployment to a NAS or self-hosted host via Docker Compose. Mutually exclusive with other deployment systems.
- **Buildkite Integration**: Runs CI on a self-hosted Buildkite agent (Apple silicon) instead of a metered cloud fleet. The pipeline and its GitHub webhook are created during generation, so there is no manual "set up project" step. Can run alongside CircleCI, so a repository can migrate without a flag day.
- **Doppler Secrets Management**: Integrates Doppler for secure secrets management. Enables the various MCP servers that rely on privileged tokens to access their services (e.g. Buildkite, CircleCI, GitHub, SonarQube).
- **Clippy (Rust code quality)**: Adds fast, zero-configuration Rust linting and formatting via cargo clippy and cargo fmt. Lint locally with `cargo clippy --all-targets -- -D warnings`. Requires a Rust devcontainer.
- **Dependabot**: Configures Dependabot for automated dependency updates.

## Doppler

This project uses Doppler for secrets from the shared `common` project
(config `dev`) — no per-repo Doppler project is created. First use (links
the shared project and `dev` config):

```bash
doppler setup --project common --config dev
```

If your repo needs app-specific secrets that shouldn't live in the shared
`common` project, regenerate it with the doppler capability set to
`projectStrategy: "new"` to get a dedicated project.

The Doppler CLI is installed in the devcontainer — it must be on PATH for the
VS Code extension and `doppler run` to work. Auth is persisted via the host
`~/.doppler` bind-mount.

### Env-var precedence (read this if `doppler run` hits the wrong project)

Doppler resolves its target as **environment variables > `doppler.yaml` >
`~/.doppler` scoped config**. If your shell — or the session that launched
the devcontainer (e.g. an agent runtime) — exports `DOPPLER_PROJECT` /
`DOPPLER_CONFIG` / `DOPPLER_ENVIRONMENT`, those silently override this
repo's `doppler.yaml` and every `doppler` command targets the wrong
project. The devcontainer's post-create setup pins this repo's context
(`common`/`dev`) in `~/.bashrc` and `~/.zshrc` and warns at
setup if resolution still mismatches. To force the correct context manually:

```bash
unset DOPPLER_PROJECT DOPPLER_CONFIG DOPPLER_ENVIRONMENT
doppler setup --no-interactive --project common --config dev
```

## Deployment

See `deploy/README.md` for the deployment runbook. Deploy with:

```bash
docker compose up -d
```

## Generated by genproj

This project was generated using the genproj tool.
