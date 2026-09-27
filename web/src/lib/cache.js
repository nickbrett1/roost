// A tiny stale-while-revalidate store for the drill-down.
//
// The hub proxies history over the agent's tunnel, and an agent answers those
// requests one at a time, so a panel opened while the agent is mid-turn waits
// for the turn to end - seconds, occasionally many. The read itself is cheap
// once it lands (a 41 MB sqlite DB answers in tens of milliseconds), so the
// wait is the whole problem.
//
// Keeping the last answer per key lets the view paint it at once and refresh
// behind it, so a revisit never opens on an empty panel. The store is
// sessionStorage rather than memory: a reload is exactly when a warm copy is
// worth the most, and a tab's own lifetime is the right scope - two tabs
// should not fight over it, and it should not outlive the session.
//
// Every storage access is wrapped: sessionStorage throws outright in some
// privacy modes and when the quota is full, and a cache that can break the
// page it is accelerating is worse than no cache.

/** Run a storage call, yielding `fallback` if the storage refuses. */
function attempt(fn, fallback) {
	try {
		return fn();
	} catch {
		return fallback;
	}
}

/**
 * Remember a value under a key.
 *
 * @param {Storage|null|undefined} storage
 * @param {string} key
 * @param {any} value
 * @param {number} [now] injection point for tests
 */
export function writeCache(storage, key, value, now = Date.now()) {
	if (!storage) return;
	attempt(() => storage.setItem(key, JSON.stringify({ at: now, value })), undefined);
}

/**
 * The value last stored under a key, or null if there is nothing usable.
 *
 * A `maxAgeMs` above zero retires a copy older than that - the caller's own
 * sense of when data has gone stale enough not to show. Zero (the default)
 * means no expiry: the background refresh is what keeps the copy honest, so a
 * slightly old list is still a better first paint than an empty panel.
 *
 * Anything malformed reads as null rather than throwing: the store is shared
 * with other tabs and other versions of this page, and a foreign or truncated
 * value is a miss, not an error.
 *
 * @param {Storage|null|undefined} storage
 * @param {string} key
 * @param {{ maxAgeMs?: number, now?: number }} [options]
 * @returns {any|null}
 */
export function readCache(storage, key, { maxAgeMs = 0, now = Date.now() } = {}) {
	if (!storage) return null;
	const raw = attempt(() => storage.getItem(key), null);
	if (typeof raw !== "string" || raw === "") return null;
	let parsed;
	try {
		parsed = JSON.parse(raw);
	} catch {
		return null;
	}
	if (parsed === null || typeof parsed !== "object") return null;
	if (!("value" in parsed) || typeof parsed.at !== "number") return null;
	if (maxAgeMs > 0 && now - parsed.at > maxAgeMs) return null;
	return parsed.value;
}
