// Browser-side access to the hub, kept pure and injectable so it is testable
// without a DOM or a live server.

/**
 * Fetch the current fleet snapshot from the hub.
 *
 * @param {typeof fetch} [fetchImpl] injection point for tests
 * @returns {Promise<Array<object>>} agent views (empty if the hub reports none)
 */
export async function fetchFleet(fetchImpl = globalThis.fetch) {
	const response = await fetchImpl("/api/fleet");
	if (!response.ok) {
		throw new Error(`fleet request failed: ${response.status}`);
	}
	const body = await response.json();
	return Array.isArray(body.agents) ? body.agents : [];
}

/**
 * Subscribe to the hub's SSE fan-out.
 *
 * @param {object} handlers
 * @param {(event: object) => void} handlers.onEvent
 * @param {(error: unknown) => void} [handlers.onError]
 * @param {typeof EventSource} [handlers.EventSourceImpl] injection point for tests
 * @returns {() => void} a close function
 */
export function openEventStream({ onEvent, onError, EventSourceImpl = globalThis.EventSource }) {
	const source = new EventSourceImpl("/events");
	source.addEventListener("hub", (message) => {
		try {
			onEvent?.(JSON.parse(message.data));
		} catch (cause) {
			onError?.(cause);
		}
	});
	source.addEventListener("error", (event) => {
		onError?.(event);
	});
	return () => source.close();
}

/**
 * Count agents by fleet state.
 *
 * @param {Array<object>} agents
 * @returns {{ total: number, live: number, stuck: number, offline: number }}
 */
export function fleetSummary(agents) {
	const summary = { total: agents.length, live: 0, stuck: 0, offline: 0 };
	for (const agent of agents) {
		if (agent.state === "live") summary.live += 1;
		else if (agent.state === "stuck") summary.stuck += 1;
		else summary.offline += 1;
	}
	return summary;
}

/**
 * Render an age (in epoch milliseconds) relative to `now`.
 *
 * @param {number|null|undefined} atMs
 * @param {number} now
 * @returns {string}
 */
export function relativeAge(atMs, now) {
	if (atMs === null || atMs === undefined) return "never";
	const seconds = Math.max(0, Math.floor((now - atMs) / 1000));
	if (seconds < 60) return `${seconds}s ago`;
	if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
	if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
	return `${Math.floor(seconds / 86400)}d ago`;
}

/**
 * When an agent last did something, in epoch ms.
 *
 * The hub reports both the instant *it heard* about the newest event
 * (`lastEventAtMs`, which is what liveness and stuck detection are measured
 * against) and the agent's own stamp for that same frame (`lastEventAt`). The
 * fleet view wants the second one: after the hub restarts, every agent
 * re-pushes the ring it was holding, so the receipt would date an hour-old turn
 * to this second. Falls back to the receipt when the agent published no stamp,
 * and is null when it has never published anything at all.
 *
 * @param {object} agent an agent view from the hub
 * @returns {number|null}
 */
export function eventAtMs(agent) {
	const stamped = agent?.lastEventAt ? Date.parse(agent.lastEventAt) : Number.NaN;
	return Number.isFinite(stamped) ? stamped : (agent?.lastEventAtMs ?? null);
}

/**
 * A history stamp as epoch ms.
 *
 * The two sources disagree about zones. The activity wire is zone-aware ISO
 * (`...Z`, `+00:00`); history stamps are naive UTC (`2026-09-26 16:21:29`)
 * with no zone on them at all. `Date.parse` reads a naive stamp as the
 * *browser's* local time, so with the reader four hours behind UTC a session
 * updated minutes ago displayed as "0s ago" - in the future, and clamped. An
 * unzoned stamp here is UTC, because that is what the agent writes.
 *
 * @param {string|null|undefined} value
 * @returns {number|null}
 */
export function parseStamp(value) {
	if (!value) return null;
	const text = String(value).trim();
	// Naive: no trailing zone designator for the parser to trust.
	const naive = /^\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?)?$/.test(text);
	const iso = naive
		? `${text.replace(" ", "T")}${text.length <= 10 ? "T00:00:00Z" : "Z"}`
		: text;
	const ms = Date.parse(iso);
	return Number.isFinite(ms) ? ms : null;
}

/**
 * The human label for a fleet state.
 *
 * The `live` state is the tunnel being *up*, not the agent doing anything: an
 * agent can hold a healthy connection while idle for a day. Calling it "live"
 * made those two ideas read as one, so it is labelled for what it measures -
 * the connection.
 *
 * @param {string} state
 * @returns {string}
 */
export function stateLabel(state) {
	if (state === "stuck") return "STUCK";
	if (state === "offline") return "offline";
	if (state === "live") return "connected";
	return state ?? "unknown";
}

/**
 * Whether an agent advertises that it can answer history.
 *
 * The drill-down's History panel proxies `history.*` over the agent's tunnel
 * (`history.sessions`, `history.messages`, `history.search`), and a roost agent
 * answers only the methods it advertised in its `hello` capabilities. So the
 * panel is offered exactly when the fleet entry carries a history capability,
 * and the token is what is gated on, not the method name.
 *
 * Two tokens mean it, because the fleet speaks both:
 *   - `sessions` — every a2a-goose agent, and the token the pydantic-agent
 *     generator is standardising on, so a future pydantic-agent is covered.
 *   - `history`  — the Rust fake agent in this repo.
 * Neither present means a history request is a guaranteed 502 (`unsupported
 * method`), so the panel must not be offered at all.
 *
 * @param {object|null|undefined} agent a fleet entry from the hub
 * @returns {boolean}
 */
export function supportsHistory(agent) {
	const capabilities = agent?.capabilities;
	if (!Array.isArray(capabilities)) return false;
	return capabilities.includes("sessions") || capabilities.includes("history");
}

/**
 * Whether a history failure means the agent does not answer history at all.
 *
 * The hub wraps an agent's own refusal in a 502 whose message carries the
 * agent's words: `history.sessions failed: unsupported method
 * 'history.sessions'`. That is an agent honouring its contract — it advertised
 * no history, and it says so rather than inventing data — so the panel should
 * collapse to one honest line, not paint the raw transport error. A blip or a
 * genuinely broken agent does not match, and still surfaces as an error.
 *
 * @param {unknown} message an error message from a history call
 * @returns {boolean}
 */
export function isUnsupportedMethodError(message) {
	return typeof message === "string" && /unsupported method/i.test(message);
}

/**
 * The human label for a transcript role.
 *
 * History messages carry the wire's own role names, where the model's turns are
 * `assistant`. roost names that actor an *agent* everywhere else - the fleet,
 * the activity feed - so the transcript uses the same word rather than
 * switching vocabulary one view in. Other roles (`user`, `tool`) are passed
 * through unchanged.
 *
 * @param {string} role
 * @returns {string}
 */
export function roleLabel(role) {
	if (role === "assistant") return "agent";
	return role ?? "unknown";
}

/**
 * The messages in a transcript that have something to say.
 *
 * A session's history is every turn, tool calls and tool results included, and
 * those steps carry a role but no words. Drawn as-is they are a run of blank
 * rows under a bare role chip, which reads as a broken page rather than a
 * choice by the agent. The transcript shows the conversation, so it draws the
 * messages with text; the raw list is kept whole because paging counts
 * messages, not utterances.
 *
 * @param {Array<object>} [messages]
 * @returns {Array<object>}
 */
export function spokenMessages(messages) {
	return (messages ?? []).filter(
		(message) => typeof message?.text === "string" && message.text.trim() !== ""
	);
}

/**
 * The hub's history routes proxy the request over the agent's tunnel, so a
 * momentary tunnel blip shows up as 502 (`agent_error`) or 503
 * (`agent_unreachable`) even though the very next attempt succeeds. One quiet
 * retry turns a blip into a slow answer instead of an error - and every route
 * that goes through here (`history.*`, `activity`) is a read, so it is safe.
 */
const TRANSIENT_STATUS = new Set([502, 503]);

/** The hub puts the real reason in the body; keep it instead of dropping it. */
async function errorDetail(response) {
	try {
		const body = await response.json();
		const detail = body?.message ?? body?.error;
		return detail ? `: ${detail}` : "";
	} catch {
		return "";
	}
}

/** Shared JSON GET that raises on a non-2xx response. */
async function getJson(fetchImpl, path) {
	let response = await fetchImpl(path);
	if (TRANSIENT_STATUS.has(response.status)) {
		response = await fetchImpl(path);
	}
	if (!response.ok) {
		throw new Error(`request failed: ${response.status}${await errorDetail(response)}`);
	}
	return response.json();
}

/**
 * An agent's past sessions (memo §4.5). The hub proxies this over the tunnel.
 *
 * @param {string} agentId
 * @param {{ cwd?: string, q?: string, limit?: number }} [options]
 * @param {typeof fetch} [fetchImpl]
 * @returns {Promise<Array<object>>}
 */
export async function fetchHistorySessions(agentId, options = {}, fetchImpl = globalThis.fetch) {
	const params = new URLSearchParams();
	if (options.cwd) params.set("cwd", options.cwd);
	if (options.q) params.set("q", options.q);
	if (options.limit) params.set("limit", String(options.limit));
	const suffix = params.toString() ? `?${params}` : "";
	const body = await getJson(
		fetchImpl,
		`/api/agents/${encodeURIComponent(agentId)}/history/sessions${suffix}`
	);
	return Array.isArray(body.body?.sessions) ? body.body.sessions : [];
}

/**
 * Search an agent's transcripts.
 *
 * @param {string} agentId
 * @param {string} q
 * @param {typeof fetch} [fetchImpl]
 */
export async function searchHistory(agentId, q, fetchImpl = globalThis.fetch) {
	const params = new URLSearchParams({ q });
	const body = await getJson(
		fetchImpl,
		`/api/agents/${encodeURIComponent(agentId)}/history/search?${params}`
	);
	return Array.isArray(body.body?.matches) ? body.body.matches : [];
}

/**
 * One session's transcript, paginated.
 *
 * @param {string} agentId
 * @param {string} sessionId
 * @param {{ cursor?: string, limit?: number }} [options]
 * @param {typeof fetch} [fetchImpl]
 */
export async function fetchHistoryMessages(
	agentId,
	sessionId,
	options = {},
	fetchImpl = globalThis.fetch
) {
	const params = new URLSearchParams();
	if (options.cursor) params.set("cursor", options.cursor);
	if (options.limit) params.set("limit", String(options.limit));
	const suffix = params.toString() ? `?${params}` : "";
	const body = await getJson(
		fetchImpl,
		`/api/agents/${encodeURIComponent(agentId)}/history/sessions/${encodeURIComponent(sessionId)}/messages${suffix}`
	);
	const inner = body.body ?? {};
	return { messages: inner.messages ?? [], nextCursor: inner.nextCursor ?? null };
}

/**
 * An agent's recent activity ring (memo §5.3). Unlike `history.*`, this does not
 * cross the tunnel: the hub already holds the last few hundred frames it was
 * pushed, so the drill-down opens with a populated view instead of an empty one
 * that fills in as the agent happens to speak.
 *
 * @param {string} agentId
 * @param {typeof fetch} [fetchImpl]
 * @returns {Promise<Array<object>>} raw activity entries, oldest first
 */
export async function fetchAgentActivity(agentId, fetchImpl = globalThis.fetch) {
	const body = await getJson(
		fetchImpl,
		`/api/agents/${encodeURIComponent(agentId)}/activity`
	);
	return Array.isArray(body.events) ? body.events : [];
}

/**
 * How full the context window is, as a percentage that reads at a glance.
 *
 * The point of the usage row is "how close to the ceiling are we", so the
 * percentage comes first and is coarse on the low end: anything above zero but
 * under a tenth of a percent says `<0.1%` rather than rounding to a flat `0.0%`
 * and reading as "nothing used".
 *
 * @param {number} used
 * @param {number} size
 * @returns {string} e.g. "4.2%"
 */
export function contextPercent(used, size) {
	if (!(size > 0)) return "0%";
	const pct = (used / size) * 100;
	if (used > 0 && pct < 0.1) return "<0.1%";
	return `${pct.toFixed(1)}%`;
}

/**
 * A stop reason as a word about the turn's outcome.
 *
 * `end_turn` is the model saying it was done: a normal, successful completion,
 * which is a fact about the turn rather than the name of a protocol enum. The
 * other reasons (`cancel`, an error, a length cap) are already self-describing
 * and are passed through unchanged, because the distinction the line is drawing
 * is exactly success versus not.
 *
 * @param {string} reason
 * @returns {string}
 */
export function stopReasonLabel(reason) {
	return reason === "end_turn" ? "success" : reason;
}

/**
 * A span of milliseconds as a compact duration.
 *
 * Rendered in the sessions list, where the useful reading is the scale of the
 * sitting - "a bit", "an hour", "most of a day" - not a stopwatch. So it keeps
 * only the two largest units that apply and drops seconds as soon as there are
 * minutes.
 *
 * @param {number} ms
 * @returns {string} e.g. "48s", "31m", "3h 20m", "2d 4h"
 */
export function elapsedLabel(ms) {
	if (!Number.isFinite(ms) || ms < 0) return "";
	const seconds = Math.round(ms / 1000);
	if (seconds < 60) return `${seconds}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m`;
	const hours = Math.floor(minutes / 60);
	if (hours < 24) {
		const rem = minutes % 60;
		return rem > 0 ? `${hours}h ${rem}m` : `${hours}h`;
	}
	const days = Math.floor(hours / 24);
	const rem = hours % 24;
	return rem > 0 ? `${days}d ${rem}h` : `${days}d`;
}

/**
 * How long a session ran, from its first to its last message.
 *
 * The two stamps are the naive-UTC strings the agent writes, read the same way
 * the list reads them (see `parseStamp`). An unknown or inverted pair yields an
 * empty string rather than a nonsense duration.
 *
 * @param {string|null|undefined} from
 * @param {string|null|undefined} to
 * @returns {string}
 */
export function sessionDuration(from, to) {
	const start = parseStamp(from);
	const end = parseStamp(to);
	if (start === null || end === null || end < start) return "";
	return elapsedLabel(end - start);
}

/**
 * A one-line gist of an event the view has no special rendering for. Unknown
 * types are shown, not dropped: version skew is normal (memo §3.4), and an
 * operator is better served by seeing a frame they do not recognise than by the
 * view silently agreeing with itself.
 *
 * The two frame types a turn always ends with are spelled out, because their
 * payload is a dump of counters and `key=value` for `usage`/`finished` reads as
 * debugging output. The `usage` line leads with the percentage of the context
 * window already spent, then the raw counts, so "how close are we" needs no
 * arithmetic; the `finished` line leads with the outcome as a word (`success`
 * for a normal completion) rather than the enum that produced it.
 *
 * @param {object} event
 * @returns {string}
 */
export function describeEvent(event) {
	if (typeof event?.text === "string") return event.text;
	const { type, ...rest } = event ?? {};
	if (type === "usage" && typeof rest.used === "number" && typeof rest.size === "number") {
		return `${contextPercent(rest.used, rest.size)} used · ${rest.used} of ${rest.size} tokens`;
	}
	if (type === "finished") {
		const parts = [];
		if (rest.stopReason) parts.push(stopReasonLabel(rest.stopReason));
		if (typeof rest.inputTokens === "number" || typeof rest.outputTokens === "number") {
			parts.push(`${rest.inputTokens ?? 0} in / ${rest.outputTokens ?? 0} out`);
		}
		if (typeof rest.totalTokens === "number") parts.push(`${rest.totalTokens} total`);
		if (parts.length > 0) return parts.join(" · ");
	}
	const detail = Object.entries(rest)
		.map(([key, value]) => `${key}=${value !== null && typeof value === "object" ? JSON.stringify(value) : value}`)
		.join(" ");
	return detail || (type ?? "event");
}

/**
 * Fold the hub's raw activity ring into renderable lines.
 *
 * The wire is deliberately high-frequency and the hub invents no schema for it:
 * thoughts and answers arrive one token at a time, so a single turn is thousands
 * of entries and the hub's ring is deliberately bounded. Coalescing therefore
 * belongs to the view. Three rules do the work:
 *
 *   - consecutive `thought` / `answer` deltas in the same context join into one
 *     line, because they are one utterance that happened to be chopped up;
 *   - a `tool_call` and its later `tool_call_update`s are one row, matched by id;
 *   - any other frame ends the current text run, so a thought that resumes after
 *     a tool call starts a fresh line rather than reading as one breath.
 *
 * Each folded text line also carries `chunks` and `bytes`, summed from the
 * `deltaBytes` the frames published. Some agents stream the shape of a turn
 * without its words; the byte count is then the only thing left to render, and
 * it keeps a folded run from collapsing to a silent blank line. `chunks` is
 * kept for completeness but is no longer shown: a frame count is an artefact of
 * how the wire chops an utterance up, and it told the reader nothing about the
 * turn.
 *
 * @param {Array<object>} entries raw entries, oldest first
 * @param {{ limit?: number }} [options] how many trailing lines to keep
 * @returns {Array<object>} folded lines, oldest first
 */
export function foldActivity(entries, { limit = 120 } = {}) {
	const lines = [];
	const tools = new Map();
	for (const entry of entries) {
		const event = entry?.event ?? {};
		const type = entry?.eventType ?? event.type ?? "unknown";
		const at = entry?.at ?? null;
		const closes = () => {
			const last = lines[lines.length - 1];
			if (last) last.open = false;
		};
		if (type === "thought" || type === "answer") {
			// Not every agent puts words on this wire: a delta may carry only
			// `deltaBytes`, so the run folds to a line with `text: ""` and a
			// count. The view needs that count to say what arrived - an empty
			// row would read as a bug in the page rather than a choice by the
			// agent.
			const text = event.text ?? "";
			const bytes = typeof event.deltaBytes === "number" ? event.deltaBytes : 0;
			const last = lines[lines.length - 1];
			if (last && last.type === type && last.open && last.contextId === (entry.contextId ?? null)) {
				last.text += text;
				last.chunks += 1;
				last.bytes += bytes;
				last.at = at ?? last.at;
				continue;
			}
			closes();
			lines.push({
				type,
				contextId: entry.contextId ?? null,
				text,
				chunks: 1,
				bytes,
				at,
				open: true
			});
			continue;
		}
		closes();
		if (type === "tool_call") {
			const line = {
				type,
				id: event.id ?? null,
				text: event.title ?? "",
				status: event.status ?? "pending",
				at
			};
			if (line.id !== null) tools.set(line.id, line);
			lines.push(line);
		} else if (type === "tool_call_update") {
			const existing = event.id !== undefined && event.id !== null ? tools.get(event.id) : undefined;
			if (existing) {
				existing.status = event.status ?? existing.status;
				if (event.title) existing.text = event.title;
				existing.at = at ?? existing.at;
			} else {
				// An update for a call the ring no longer holds (it aged out, or
				// this is a reconnect): show it rather than dropping it.
				lines.push({
					type,
					id: event.id ?? null,
					text: event.title ?? "",
					status: event.status ?? "unknown",
					at
				});
			}
		} else {
			lines.push({ type, id: null, text: describeEvent(event), at, status: null });
		}
	}
	const kept = lines.slice(-limit);
	for (const line of kept) delete line.open;
	return kept;
}
