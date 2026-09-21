import { describe, expect, it, vi } from "vitest";
import { fetchFleet, fleetSummary, openEventStream, relativeAge, stateLabel } from "./api.js";

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

describe("stateLabel", () => {
	it("labels each known state", () => {
		expect(stateLabel("live")).toBe("live");
		expect(stateLabel("stuck")).toBe("STUCK");
		expect(stateLabel("offline")).toBe("offline");
	});

	it("passes an unknown state through and handles undefined", () => {
		expect(stateLabel("rebooting")).toBe("rebooting");
		expect(stateLabel(undefined)).toBe("unknown");
	});
});
