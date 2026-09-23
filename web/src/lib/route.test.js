import { describe, expect, it } from "vitest";

import { FLEET_HASH, agentHash, parseRoute, sessionHash } from "./route.js";

describe("parseRoute", () => {
	it("reads the fleet from an empty or bare hash", () => {
		for (const hash of ["", "#", FLEET_HASH, "#/something-else", "#/agent"]) {
			expect(parseRoute(hash)).toEqual({ agentId: null, sessionId: null });
		}
	});

	it("reads an agent", () => {
		expect(parseRoute("#/agent/roost-dev")).toEqual({
			agentId: "roost-dev",
			sessionId: null,
		});
	});

	it("reads a session inside an agent", () => {
		expect(parseRoute("#/agent/roost-dev/session/20260923_1")).toEqual({
			agentId: "roost-dev",
			sessionId: "20260923_1",
		});
	});

	it("keeps a session id that contains slashes", () => {
		expect(parseRoute("#/agent/roost-dev/session/a/b")).toEqual({
			agentId: "roost-dev",
			sessionId: "a/b",
		});
	});

	it("round-trips ids that need escaping", () => {
		for (const [agentId, sessionId] of [
			["roost-dev", "20260923_1"],
			["agent with space", "session/with/slash"],
			["a@b.c", "100%"],
		]) {
			const hash = sessionHash(agentId, sessionId);
			expect(hash).not.toContain(" ");
			expect(parseRoute(hash)).toEqual({ agentId, sessionId });
			expect(parseRoute(agentHash(agentId))).toEqual({ agentId, sessionId: null });
		}
	});

	it("returns what it was given rather than throwing on a bad escape", () => {
		// A truncated link is the operator's typo, not a crash.
		expect(parseRoute("#/agent/%")).toEqual({ agentId: "%", sessionId: null });
		expect(parseRoute("#/agent/roost-dev/session/%zz")).toEqual({
			agentId: "roost-dev",
			sessionId: "%zz",
		});
	});

	it("survives a hash that is not a string", () => {
		expect(parseRoute(undefined)).toEqual({ agentId: null, sessionId: null });
		expect(parseRoute(null)).toEqual({ agentId: null, sessionId: null });
	});
});
