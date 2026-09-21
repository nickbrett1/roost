<script>
	import { onMount } from "svelte";
	import { fetchFleet, openEventStream, fleetSummary, relativeAge, stateLabel } from "./lib/api.js";

	let agents = $state([]);
	let connected = $state(false);
	let error = $state(null);
	let nowMs = $state(Date.now());

	async function load() {
		try {
			agents = await fetchFleet();
			error = null;
		} catch (cause) {
			error = cause.message;
		}
	}

	onMount(() => {
		load();

		// The fleet snapshot is authoritative; live deltas keep ages honest.
		const close = openEventStream({
			onEvent(event) {
				connected = true;
				if (event.type === "fleet") {
					agents = event.agents;
				} else if (event.type === "activity") {
					agents = agents.map((agent) =>
						agent.agentId === event.agentId
							? { ...agent, lastEventAtMs: Date.now(), eventCount: agent.eventCount + 1 }
							: agent
					);
				}
			},
			onError() {
				connected = false;
			}
		});

		// A one-second tick re-renders the age column even when nothing arrives,
		// which is exactly what makes a stalled agent visible.
		const tick = setInterval(() => (nowMs = Date.now()), 1000);

		return () => {
			close();
			clearInterval(tick);
		};
	});

	const summary = $derived(fleetSummary(agents));
</script>

<main>
	<header>
		<h1>mission control</h1>
		<p class="summary">
			{summary.total} agents
			{#if summary.live > 0}· {summary.live} live{/if}
			{#if summary.stuck > 0}· <span class="warn">{summary.stuck} stuck</span>{/if}
			{#if summary.offline > 0}· {summary.offline} offline{/if}
		</p>
		<span class="link" class:up={connected}>{connected ? "live" : "disconnected"}</span>
	</header>

	{#if error}
		<p class="error">Could not reach the hub API: {error}</p>
	{/if}

	{#if agents.length === 0}
		<p class="empty">No agents connected. No tunnel client has dialed in yet.</p>
	{:else}
		<table>
			<thead>
				<tr>
					<th>agent</th>
					<th>kind</th>
					<th>version</th>
					<th>in flight</th>
					<th>last event</th>
					<th>state</th>
				</tr>
			</thead>
			<tbody>
				{#each agents as agent (agent.agentId)}
					<tr class={agent.state}>
						<td>{agent.agentId}</td>
						<td>{agent.kind}</td>
						<td>{agent.agentVersion}</td>
						<td>{agent.inFlight}</td>
						<td>{relativeAge(agent.lastEventAtMs, nowMs)}</td>
						<td>{stateLabel(agent.state)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
</main>

<style>
	main {
		font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
		max-width: 56rem;
		margin: 0 auto;
		padding: 2rem 1rem;
	}
	header {
		display: flex;
		align-items: baseline;
		gap: 1rem;
		border-bottom: 1px solid #333;
		margin-bottom: 1rem;
	}
	h1 {
		font-size: 1.1rem;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		margin: 0;
	}
	.summary {
		margin: 0;
		color: #999;
	}
	.link {
		margin-left: auto;
		color: #a33;
	}
	.link.up {
		color: #3a3;
	}
	.warn {
		color: #c80;
	}
	.empty,
	.error {
		color: #999;
	}
	.error {
		color: #a33;
	}
	table {
		width: 100%;
		border-collapse: collapse;
	}
	th,
	td {
		text-align: left;
		padding: 0.35rem 0.6rem;
		border-bottom: 1px solid #222;
	}
	th {
		color: #777;
		font-weight: normal;
		text-transform: uppercase;
		font-size: 0.75rem;
	}
	tr.stuck td {
		color: #c80;
	}
	tr.offline td {
		color: #777;
	}
</style>
