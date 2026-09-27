import { describe, expect, it } from "vitest";
import { readCache, writeCache } from "./cache.js";

/** A Storage stand-in, with the ways real sessionStorage can misbehave. */
function fakeStorage({ failOn } = {}) {
	const map = new Map();
	return {
		map,
		getItem(key) {
			if (failOn === "get") throw new Error("blocked");
			return map.has(key) ? map.get(key) : null;
		},
		setItem(key, value) {
			if (failOn === "set") throw new Error("quota");
			map.set(key, value);
		}
	};
}

describe("cache", () => {
	it("round-trips a value", () => {
		const storage = fakeStorage();
		writeCache(storage, "k", [{ sessionId: "a" }], 1000);
		expect(readCache(storage, "k")).toEqual([{ sessionId: "a" }]);
	});

	it("misses on an empty or absent store", () => {
		expect(readCache(fakeStorage(), "nope")).toBeNull();
		expect(readCache(null, "k")).toBeNull();
		expect(readCache(undefined, "k")).toBeNull();
	});

	it("does not write when there is no store", () => {
		expect(() => writeCache(null, "k", 1)).not.toThrow();
	});

	it("retires a copy older than the caller's patience", () => {
		const storage = fakeStorage();
		writeCache(storage, "k", 42, 1_000_000);
		expect(readCache(storage, "k", { maxAgeMs: 60_000, now: 1_030_000 })).toBe(42);
		expect(readCache(storage, "k", { maxAgeMs: 60_000, now: 1_060_001 })).toBeNull();
	});

	it("keeps a copy indefinitely when no age is given", () => {
		const storage = fakeStorage();
		writeCache(storage, "k", 42, 0);
		expect(readCache(storage, "k", { now: 10 ** 12 })).toBe(42);
	});

	it("treats a foreign or truncated value as a miss, not an error", () => {
		const storage = fakeStorage();
		storage.map.set("k", "not json");
		expect(readCache(storage, "k")).toBeNull();

		storage.map.set("k", JSON.stringify({ value: 1 }));
		expect(readCache(storage, "k")).toBeNull();

		storage.map.set("k", JSON.stringify({ at: 1 }));
		expect(readCache(storage, "k")).toBeNull();

		storage.map.set("k", JSON.stringify("just a string"));
		expect(readCache(storage, "k")).toBeNull();
	});

	it("survives a storage that refuses to read or write", () => {
		expect(readCache(fakeStorage({ failOn: "get" }), "k")).toBeNull();
		expect(() => writeCache(fakeStorage({ failOn: "set" }), "k", 1)).not.toThrow();
	});

	it("caches an empty list as a value, not as a miss", () => {
		// An agent with no sessions is a real answer; showing it beats asking
		// again, so falsy values have to survive the round trip.
		const storage = fakeStorage();
		writeCache(storage, "k", [], 5);
		expect(readCache(storage, "k")).toEqual([]);
	});
});
