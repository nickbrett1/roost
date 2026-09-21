<script>
	import { onMount } from "svelte";
	import {
		fetchFleet,
		fetchHistoryMessages,
		fetchHistorySessions,
		fleetSummary,
		openEventStream,
		relativeAge,
		searchHistory,
		stateLabel
	} from "./lib/api.js";

	let agents = $state([]);
	let connected = $state(false);
	let error = $state(null);
	let nowMs = $state(Date.now());

	// Drill-down state (memo §6.2): history is proxied to the agent on demand.
	let selected = $state(null);
	let historyError = $state(null);
	let search = $state("");
	let sessions = $state(null);
	let matches = $state(null);
	let transcript = $state(null);

	async function load() {
		try {
			agents = await fetchFleet();
			error = null;
		} catch (cause) {
			error = cause.message;
		}
	}

	function select(agentId) {
		selected = selected === agentId ? null : agentId;
		search = "";
		matches = null;
		transcript = null;
		historyError = null;
		sessions = null;
		if (selected) loadSessions(selected);
	}

	async function loadSessions(agentId) {
		try {
			sessions = await fetchHistorySessions(agentId, { limit: 20 });
			historyError = null;
		} catch (cause) {
			historyError = cause.message;
			sessions = [];
		}
	}

	async function runSearch() {
		const query = search.trim();
		if (!selected || query === "") {
			matches = null;
			return;
		}
		try {
			matches = await searchHistory(selected, query);
			historyError = null;
		} catch (cause) {
			historyError = cause.message;
		}
	}

	async function openSession(sessionId) {
		try {
			const page = await fetchHistoryMessages(selected, sessionId, { limit: 20 });
			transcript = { sessionId, ...page };
			historyError = null;
		} catch (cause) {
			historyError = cause.message;
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
					<tr
						class={agent.state}
						class:selected={selected === agent.agentId}
						onclick={() => select(agent.agentId)}
					>
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

	{#if selected}
		<section class="drilldown">
			<h2>
				{selected}
				<span class="muted">· history</span>
			</h2>

			<div class="searchbar">
				<input
					type="search"
					bind:value={search}
					placeholder="search this agent's conversations"
					onkeydown={(event) => event.key === "Enter" && runSearch()}
				/>
				<button onclick={runSearch}>search</button>
				{#if matches !== null}
					<button onclick={() => (matches = null)}>clear</button>
				{/if}
			</div>

			{#if historyError}
				<p class="error">history unavailable: {historyError}</p>
			{/if}

			{#if matches !== null}
				<h3>matches</h3>
				{#if matches.length === 0}
					<p class="muted">no matches</p>
				{:else}
					{#each matches as match (match.sessionId)}
						<p>
							<button class="link" onclick={() => openSession(match.sessionId)}>
								{match.name}
							</button>
							<span class="muted">{match.matches.length} match(es)</span>
						</p>
					{/each}
				{/if}
			{:else}
				<h3>sessions</h3>
				{#if sessions === null}
					<p class="muted">loading…</p>
				{:else if sessions.length === 0}
					<p class="muted">no sessions</p>
				{:else}
					{#each sessions as session (session.sessionId)}
						<p>
							<button class="link" onclick={() => openSession(session.sessionId)}>
								{session.name}
							</button>
							<span class="muted">
								{session.messageCount} messages · {session.tokens} tokens · {relativeAge(
									Date.parse(session.updatedAt),
									nowMs
								)}
							</span>
						</p>
					{/each}
				{/if}
			{/if}

			{#if transcript}
				<h3>transcript · {transcript.sessionId}</h3>
				{#each transcript.messages as message, i (i)}
					<p class="message">
						<span class="role">{message.role}</span>
						{message.text}
					</p>
				{/each}
				{#if transcript.nextCursor}
					<p class="muted">more messages available</p>
				{/if}
			{/if}
		</section>
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
	h2 {
		font-size: 0.95rem;
		margin: 1.5rem 0 0.5rem;
	}
	h3 {
		font-size: 0.75rem;
		text-transform: uppercase;
		color: #777;
		margin: 1rem 0 0.25rem;
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
	.muted {
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
	tbody tr {
		cursor: pointer;
	}
	tbody tr:hover {
		background: #161616;
	}
	tr.stuck td {
		color: #c80;
	}
	tr.offline td {
		color: #777;
	}
	tr.selected td {
		color: #6cf;
	}
	.drilldown {
		border-top: 1px solid #333;
	}
	.searchbar {
		display: flex;
		gap: 0.5rem;
	}
	.searchbar input {
		flex: 1;
		background: #111;
		border: 1px solid #333;
		color: inherit;
		padding: 0.3rem 0.5rem;
		font: inherit;
	}
	button {
		background: #222;
		border: 1px solid #444;
		color: inherit;
		padding: 0.3rem 0.6rem;
		font: inherit;
		cursor: pointer;
	}
	button.link {
		background: none;
		border: none;
		padding: 0;
		color: #6cf;
		text-decoration: underline;
	}
	.message {
		margin: 0.25rem 0;
	}
	.role {
		color: #777;
		margin-right: 0.5rem;
	}
</style>
