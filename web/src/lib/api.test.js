import { describe, expect, it, vi } from "vitest";
import {
	describeEvent,
	eventAtMs,
	fetchAgentActivity,
	fetchFleet,
	fetchHistoryMessages,
	fetchHistorySessions,
	fleetSummary,
	foldActivity,
	openEventStream,
	parseStamp,
	relativeAge,
	roleLabel,
	searchHistory,
	spokenMessages,
	stateLabel
} from "./api.js";

describe("fetchFleet", () => {
	it("returns the agents array from the hub", async () => {
		const fetchImpl = vi.fn(async () => ({
			ok: true,
			json: async () => ({ agents: [{ agentId: "a" }] })
		}));
		await expect(fetchFleet(fetchImpl)).resolves.toEqual([{ agentId: "a" }]);
		expect(fetchImpl).toHaveBeenCalledWith("/api/fleet");
	});

	it("returns an empty array when the hub reports none", async () => {
		const fetchImpl = vi.fn(async () => ({ ok: true, json: async () => ({}) }));
		await expect(fetchFleet(fetchImpl)).resolves.toEqual([]);
	});

	it("throws on a failed response", async () => {
		const fetchImpl = vi.fn(async () => ({ ok: false, status: 503, json: async () => ({}) }));
		await expect(fetchFleet(fetchImpl)).rejects.toThrow("fleet request failed: 503");
	});
});

describe("openEventStream", () => {
	class FakeEventSource {
		constructor(url) {
			this.url = url;
			this.listeners = {};
			this.closed = false;
			FakeEventSource.instance = this;
		}
		addEventListener(name, handler) {
			this.listeners[name] = handler;
		}
		emit(name, data) {
			this.listeners[name]?.({ data });
		}
		close() {
			this.closed = true;
		}
	}

	it("parses hub events and forwards them", () => {
		const onEvent = vi.fn();
		const close = openEventStream({ onEvent, EventSourceImpl: FakeEventSource });
		const source = FakeEventSource.instance;
		expect(source.url).toBe("/events");

		source.emit("hub", JSON.stringify({ type: "fleet", agents: [] }));
		expect(onEvent).toHaveBeenCalledWith({ type: "fleet", agents: [] });

		close();
		expect(source.closed).toBe(true);
	});

	it("reports malformed payloads through onError", () => {
		const onEvent = vi.fn();
		const onError = vi.fn();
		openEventStream({ onEvent, onError, EventSourceImpl: FakeEventSource });
		FakeEventSource.instance.emit("hub", "not json");
		expect(onEvent).not.toHaveBeenCalled();
		expect(onError).toHaveBeenCalled();
	});

	it("reports transport errors through onError", () => {
		const onError = vi.fn();
		openEventStream({ onEvent: vi.fn(), onError, EventSourceImpl: FakeEventSource });
		FakeEventSource.instance.emit("error", undefined);
		expect(onError).toHaveBeenCalled();
	});
});

describe("fleetSummary", () => {
	it("counts by state", () => {
		const agents = [
			{ state: "live" },
			{ state: "live" },
			{ state: "stuck" },
			{ state: "offline" }
		];
		expect(fleetSummary(agents)).toEqual({ total: 4, live: 2, stuck: 1, offline: 1 });
	});

	it("handles an empty fleet", () => {
		expect(fleetSummary([])).toEqual({ total: 0, live: 0, stuck: 0, offline: 0 });
	});

	it("treats an unrecognised state as offline", () => {
		expect(fleetSummary([{ state: "rebooting" }])).toEqual({
			total: 1,
			live: 0,
			stuck: 0,
			offline: 1
		});
	});
});

describe("relativeAge", () => {
	const now = 10_000_000;

	it("reports never for a missing timestamp", () => {
		expect(relativeAge(null, now)).toBe("never");
		expect(relativeAge(undefined, now)).toBe("never");
	});

	it("scales the unit with the age", () => {
		expect(relativeAge(now - 3_000, now)).toBe("3s ago");
		expect(relativeAge(now - 90_000, now)).toBe("1m ago");
		expect(relativeAge(now - 7_200_000, now)).toBe("2h ago");
		expect(relativeAge(now - 172_800_000, now)).toBe("2d ago");
	});

	it("never goes negative on clock skew", () => {
		expect(relativeAge(now + 5_000, now)).toBe("0s ago");
	});
});

describe("eventAtMs", () => {
	const received = 1790335618815;

	it("prefers the agent's own stamp over the hub's receipt", () => {
		// The hub restart case: the frame is from 11:00:22Z, the hub heard it at
		// 11:26:58Z, and the table must not claim the turn happened at 11:26.
		expect(
			eventAtMs({ lastEventAt: "2026-09-25T11:00:22.507Z", lastEventAtMs: received })
		).toBe(Date.parse("2026-09-25T11:00:22.507Z"));
	});

	it("falls back to the receipt when the agent published no stamp", () => {
		expect(eventAtMs({ lastEventAtMs: received })).toBe(received);
		expect(eventAtMs({ lastEventAt: "", lastEventAtMs: received })).toBe(received);
	});

	it("falls back to the receipt when the stamp is unparseable", () => {
		expect(eventAtMs({ lastEventAt: "whenever", lastEventAtMs: received })).toBe(received);
	});

	it("reports nothing for an agent that has never published", () => {
		expect(eventAtMs({ lastEventAt: null, lastEventAtMs: null })).toBeNull();
		expect(eventAtMs({})).toBeNull();
		expect(eventAtMs(undefined)).toBeNull();
	});
});

describe("stateLabel", () => {
	it("labels each known state", () => {
		// "live" is the tunnel being up, so it is named for the connection
		// rather than dressed up as the agent being active.
		expect(stateLabel("live")).toBe("connected");
		expect(stateLabel("stuck")).toBe("STUCK");
		expect(stateLabel("offline")).toBe("offline");
	});

	it("passes an unknown state through and handles undefined", () => {
		expect(stateLabel("rebooting")).toBe("rebooting");
		expect(stateLabel(undefined)).toBe("unknown");
	});
});

describe("roleLabel", () => {
	it("names the assistant role the way the rest of the view does", () => {
		expect(roleLabel("assistant")).toBe("agent");
	});

	it("passes other roles through and handles a missing one", () => {
		expect(roleLabel("user")).toBe("user");
		expect(roleLabel("tool")).toBe("tool");
		expect(roleLabel(undefined)).toBe("unknown");
	});
});

describe("spokenMessages", () => {
	it("keeps a message that has text and drops the turn that has none", () => {
		const messages = [
			{ index: 0, role: "user", text: "Add history." },
			{ index: 1, role: "assistant", text: "" },
			{ index: 2, role: "tool", text: "Read src/main.rs" },
			{ index: 3, role: "user", text: "" }
		];
		expect(spokenMessages(messages)).toEqual([messages[0], messages[2]]);
	});

	it("treats whitespace-only text as no text", () => {
		const messages = [{ index: 0, role: "assistant", text: "  \n\t " }];
		expect(spokenMessages(messages)).toEqual([]);
	});

	it("handles a missing text field and no list at all", () => {
		expect(spokenMessages([{ index: 0, role: "tool" }])).toEqual([]);
		expect(spokenMessages(undefined)).toEqual([]);
		expect(spokenMessages(null)).toEqual([]);
	});
});

describe("parseStamp", () => {
	it("reads a naive history stamp as UTC, not the reader's zone", () => {
		expect(parseStamp("2026-09-26 16:21:29")).toBe(Date.parse("2026-09-26T16:21:29Z"));
	});

	it("leaves a zone-aware stamp alone", () => {
		expect(parseStamp("2026-09-25T11:00:22.507Z")).toBe(
			Date.parse("2026-09-25T11:00:22.507Z")
		);
		expect(parseStamp("2026-09-25T11:00:22+01:00")).toBe(
			Date.parse("2026-09-25T11:00:22+01:00")
		);
	});

	it("treats a bare date as midnight UTC", () => {
		expect(parseStamp("2026-09-26")).toBe(Date.parse("2026-09-26T00:00:00Z"));
	});

	it("returns null when there is nothing to read", () => {
		expect(parseStamp(null)).toBeNull();
		expect(parseStamp("")).toBeNull();
		expect(parseStamp("whenever")).toBeNull();
	});
});

describe("history helpers", () => {
	const ok = (payload) => async () => ({ ok: true, json: async () => payload });

	it("builds the sessions path with filters and unwraps the body", async () => {
		let called;
		const fetchImpl = async (path) => {
			called = path;
			return { ok: true, json: async () => ({ body: { sessions: [{ sessionId: "s1" }] } }) };
		};
		const sessions = await fetchHistorySessions(
			"a2a-goose-dev",
			{ cwd: "/workspaces/roost", limit: 2 },
			fetchImpl
		);
		expect(called).toBe(
			"/api/agents/a2a-goose-dev/history/sessions?cwd=%2Fworkspaces%2Froost&limit=2"
		);
		expect(sessions).toEqual([{ sessionId: "s1" }]);
	});

	it("returns an empty array when the agent has no sessions", async () => {
		await expect(fetchHistorySessions("a", {}, ok({ body: {} }))).resolves.toEqual([]);
	});

	it("encodes the search query", async () => {
		let called;
		const fetchImpl = async (path) => {
			called = path;
			return { ok: true, json: async () => ({ body: { matches: [] } }) };
		};
		await searchHistory("a2a-goose-dev", "docker publish", fetchImpl);
		expect(called).toBe("/api/agents/a2a-goose-dev/history/search?q=docker+publish");
	});

	it("unwraps messages and the cursor", async () => {
		const fetchImpl = ok({ body: { messages: [{ role: "user" }], nextCursor: "2" } });
		await expect(fetchHistoryMessages("a", "s1", {}, fetchImpl)).resolves.toEqual({
			messages: [{ role: "user" }],
			nextCursor: "2"
		});
	});

	it("raises on a failed response", async () => {
		const fetchImpl = async () => ({ ok: false, status: 503 });
		await expect(fetchHistorySessions("a", {}, fetchImpl)).rejects.toThrow("503");
	});
});

describe("fetchAgentActivity", () => {
	it("returns the events array from the hub's ring", async () => {
		const fetchImpl = vi.fn(async () => ({ ok: true, json: async () => ({ events: [{ seq: 1 }] }) }));
		await expect(fetchAgentActivity("a b", fetchImpl)).resolves.toEqual([{ seq: 1 }]);
		// The id is encoded: agent ids are host-shaped, not URL-safe by contract.
		expect(fetchImpl).toHaveBeenCalledWith("/api/agents/a%20b/activity");
	});

	it("returns an empty array when the agent has published nothing", async () => {
		const fetchImpl = vi.fn(async () => ({ ok: true, json: async () => ({ agentId: "a" }) }));
		await expect(fetchAgentActivity("a", fetchImpl)).resolves.toEqual([]);
	});

	it("throws when the hub does not know the agent, so the view can say so", async () => {
		const fetchImpl = vi.fn(async () => ({ ok: false, status: 404, json: async () => ({}) }));
		await expect(fetchAgentActivity("ghost", fetchImpl)).rejects.toThrow("request failed: 404");
	});
});

describe("describeEvent", () => {
	it("prefers a text field when the event carries one", () => {
		expect(describeEvent({ type: "plan", text: "do the thing" })).toBe("do the thing");
	});

	it("falls back to the remaining fields, so an unknown type still says something", () => {
		expect(describeEvent({ type: "usage", total: 42 })).toBe("total=42");
	});

	it("names a bare event rather than rendering it empty", () => {
		expect(describeEvent({ type: "finished" })).toBe("finished");
		expect(describeEvent(undefined)).toBe("event");
	});

	it("spells out a usage frame instead of dumping its counters", () => {
		expect(describeEvent({ type: "usage", size: 1000000, used: 124864 })).toBe(
			"context 124864 of 1000000"
		);
	});

	it("falls back for a usage frame shaped differently", () => {
		expect(describeEvent({ type: "usage", used: 5 })).toBe("used=5");
	});

	it("spells out how a turn finished", () => {
		expect(
			describeEvent({
				type: "finished",
				contextTokens: 124864,
				inputTokens: 123827,
				outputTokens: 1037,
				stopReason: "end_turn",
				totalTokens: 124864
			})
		).toBe("end_turn · 123827 in / 1037 out · 124864 total");
	});

	it("says what it can when a finished frame is incomplete", () => {
		expect(describeEvent({ type: "finished", stopReason: "end_turn" })).toBe("end_turn");
		expect(describeEvent({ type: "finished", totalTokens: 42 })).toBe("42 total");
		// Nothing recognisable left: the generic gist still shows what came,
		// rather than silently rendering the word "finished".
		expect(describeEvent({ type: "finished", stopReason: null })).toBe("stopReason=null");
	});
});

describe("foldActivity", () => {
	const entry = (seq, eventType, event, extra = {}) => ({
		seq,
		eventType,
		at: `2026-09-22T10:00:${String(seq).padStart(2, "0")}Z`,
		contextId: "ctx-1",
		event,
		...extra
	});

	it("coalesces a streamed thought into a single line", () => {
		const lines = foldActivity([
			entry(1, "thought", { type: "thought", text: "Hel" }),
			entry(2, "thought", { type: "thought", text: "lo " }),
			entry(3, "thought", { type: "thought", text: "world" })
		]);
		expect(lines).toHaveLength(1);
		expect(lines[0]).toMatchObject({ type: "thought", text: "Hello world" });
	});

	it("does not merge one context's thought into another's", () => {
		const lines = foldActivity([
			entry(1, "thought", { type: "thought", text: "a" }),
			entry(2, "thought", { type: "thought", text: "b" }, { contextId: "ctx-2" })
		]);
		expect(lines).toHaveLength(2);
		expect(lines.map((line) => line.text)).toEqual(["a", "b"]);
	});

	it("does not merge an answer into a preceding thought", () => {
		const lines = foldActivity([
			entry(1, "thought", { type: "thought", text: "thinking" }),
			entry(2, "answer", { type: "answer", text: "answer" })
		]);
		expect(lines.map((line) => line.type)).toEqual(["thought", "answer"]);
	});

	it("keeps a thought that resumes after a tool call as a fresh line", () => {
		const lines = foldActivity([
			entry(1, "thought", { type: "thought", text: "before" }),
			entry(2, "tool_call", { type: "tool_call", id: "c1", title: "shell · ls" }),
			entry(3, "thought", { type: "thought", text: "after" })
		]);
		expect(lines.map((line) => line.type)).toEqual(["thought", "tool_call", "thought"]);
		expect(lines[0].text).toBe("before");
		expect(lines[2].text).toBe("after");
	});

	it("folds a tool call and its updates into one row", () => {
		const lines = foldActivity([
			entry(1, "tool_call", { type: "tool_call", id: "c1", title: "shell · ls" }),
			entry(2, "tool_call_update", { type: "tool_call_update", id: "c1", status: "in_progress" }),
			entry(3, "tool_call_update", { type: "tool_call_update", id: "c1", status: "completed" })
		]);
		expect(lines).toHaveLength(1);
		expect(lines[0]).toMatchObject({ id: "c1", text: "shell · ls", status: "completed" });
	});

	it("shows an update whose call has aged out of the ring", () => {
		const lines = foldActivity([
			entry(1, "tool_call_update", { type: "tool_call_update", id: "gone", status: "completed" })
		]);
		expect(lines).toHaveLength(1);
		expect(lines[0].status).toBe("completed");
	});

	it("keeps an unknown frame type instead of dropping it (version skew is normal)", () => {
		const lines = foldActivity([entry(1, "quantum_entangled", { type: "quantum_entangled", x: 1 })]);
		expect(lines).toHaveLength(1);
		expect(lines[0].type).toBe("quantum_entangled");
	});

	it("keeps only the most recent lines", () => {
		const many = Array.from({ length: 10 }, (_, i) => entry(i + 1, "tool_call", { type: "tool_call", id: `c${i}`, title: "t" }));
		const lines = foldActivity(many, { limit: 3 });
		expect(lines).toHaveLength(3);
		expect(lines.map((line) => line.id)).toEqual(["c7", "c8", "c9"]);
	});

	it("carries no internal bookkeeping into the render", () => {
		const lines = foldActivity([entry(1, "thought", { type: "thought", text: "x" })]);
		expect(lines[0]).not.toHaveProperty("open");
	});

	it("counts a delta run whose agent published no text", () => {
		const lines = foldActivity([
			entry(1, "answer", { type: "answer", deltaBytes: 5 }),
			entry(2, "answer", { type: "answer", deltaBytes: 6 }),
			entry(3, "answer", { type: "answer", deltaBytes: 1 })
		]);
		expect(lines).toHaveLength(1);
		expect(lines[0]).toMatchObject({ type: "answer", text: "", chunks: 3, bytes: 12 });
	});

	it("counts a delta run that does carry text", () => {
		const lines = foldActivity([
			entry(1, "answer", { type: "answer", text: "Hel", deltaBytes: 3 }),
			entry(2, "answer", { type: "answer", text: "lo", deltaBytes: 2 })
		]);
		expect(lines[0]).toMatchObject({ text: "Hello", chunks: 2, bytes: 5 });
	});

	it("starts the count again when a tool call breaks the run", () => {
		const lines = foldActivity([
			entry(1, "answer", { type: "answer", deltaBytes: 5 }),
			entry(2, "tool_call", { type: "tool_call", id: "c1", title: "shell · ls" }),
			entry(3, "answer", { type: "answer", deltaBytes: 7 })
		]);
		expect(lines.map((line) => line.bytes)).toEqual([5, undefined, 7]);
	});

	it("survives an empty ring", () => {
		expect(foldActivity([])).toEqual([]);
	});
});
