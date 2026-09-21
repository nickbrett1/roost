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
 * The human label for a fleet state.
 *
 * @param {string} state
 * @returns {string}
 */
export function stateLabel(state) {
	if (state === "stuck") return "STUCK";
	if (state === "offline") return "offline";
	if (state === "live") return "live";
	return state ?? "unknown";
}

/** Shared JSON GET that raises on a non-2xx response. */
async function getJson(fetchImpl, path) {
	const response = await fetchImpl(path);
	if (!response.ok) throw new Error(`request failed: ${response.status}`);
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
