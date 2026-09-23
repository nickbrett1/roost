// The hash is the router.
//
// Three levels, in one string, because a phone has three views and only one
// screen:
//
//   #/                       the fleet
//   #/agent/<id>             one agent: its live feed and its sessions
//   #/agent/<id>/session/<s> one session: its transcript
//
// Each level is a view rather than a panel below the previous list, so opening
// something never asks the reader to scroll past the list to find it. Keeping
// the route in the location also means the browser's back button, a reload and
// a shared link all mean the same thing.
//
// Pure and DOM-free on purpose: this is the logic that decides what the
// operator is looking at, so it is the part worth testing directly.

const AGENT = /^#\/agent\/([^/]+)$/;
const SESSION = /^#\/agent\/([^/]+)\/session\/(.+)$/;

export const FLEET_HASH = "#/";

/**
 * The route for one agent's view.
 *
 * @param {string} agentId
 * @returns {string}
 */
export function agentHash(agentId) {
	return `#/agent/${encodeURIComponent(agentId)}`;
}

/**
 * The route for one session, inside one agent.
 *
 * @param {string} agentId
 * @param {string} sessionId
 * @returns {string}
 */
export function sessionHash(agentId, sessionId) {
	return `${agentHash(agentId)}/session/${encodeURIComponent(sessionId)}`;
}

// A hand-edited or truncated hash must not throw: a bad `%` escape is a decode
// error, and the honest answer is "what you typed", not a blank page.
function decode(value) {
	try {
		return decodeURIComponent(value);
	} catch {
		return value;
	}
}

/**
 * Which view a hash names. Anything unrecognised is the fleet: there is no 404
 * for a hash, and the fleet is always a safe place to land.
 *
 * @param {string} [hash] the location hash, `#` included
 * @returns {{agentId: string|null, sessionId: string|null}}
 */
export function parseRoute(hash) {
	const text = typeof hash === "string" ? hash : "";
	const session = SESSION.exec(text);
	if (session) {
		return { agentId: decode(session[1]), sessionId: decode(session[2]) };
	}
	const agent = AGENT.exec(text);
	if (agent) {
		return { agentId: decode(agent[1]), sessionId: null };
	}
	return { agentId: null, sessionId: null };
}
