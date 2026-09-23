<script>
	import { onMount } from "svelte";
	import {
		fetchAgentActivity,
		fetchFleet,
		fetchHistoryMessages,
		fetchHistorySessions,
		fetchRebootPreflight,
		fleetSummary,
		foldActivity,
		openEventStream,
		rebootAgent,
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

	// The live turn view (memo §6.3). Activity does not come from history: the
	// hub already holds the frames it was pushed, so this opens populated and
	// then follows the fan-out. It is stored raw and folded at render time,
	// which keeps the wire shape and the presentation from drifting apart.
	let activityEntries = $state([]);
	let activityError = $state(null);
	let activity = $derived(foldActivity(activityEntries));

	// The reboot control (memo §5.5). It is armed by a *preflight*, which names
	// the version that would be installed; nothing irreversible happens until the
	// operator confirms against that version in the prompt below.
	let rebootPrompt = $state(null); // { agentId, preflight }
	let rebootError = $state(null);
	let rebootNotice = $state(null);

	// The hub's ring is bounded (512); mirror that bound so a long-lived view
	// holds no more than the hub would have sent it anyway.
	const ACTIVITY_CAP = 512;

	function label(line) {
		if (line.type === "thought") return "think";
		if (line.type === "answer") return "say";
		if (line.type === "tool_call" || line.type === "tool_call_update") return "tool";
		return line.type.replaceAll("_", " ");
	}

	function clock(at) {
		return at ? at.slice(11, 19) : "";
	}

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
		activityEntries = [];
		activityError = null;
		if (selected) {
			loadSessions(selected);
			loadActivity(selected);
		}
	}

	async function loadActivity(agentId) {
		try {
			activityEntries = await fetchAgentActivity(agentId);
			activityError = null;
		} catch (cause) {
			activityError = cause.message;
			activityEntries = [];
		}
	}

	function pushActivity(agentId, entry) {
		// Only the open drill-down is tracked: the fleet table already carries
		// every agent's liveness, and keeping a ring per agent would be the
		// storage the memo says the hub is not (§5.6).
		if (agentId !== selected) return;
		const next = activityEntries.concat([entry]);
		activityEntries = next.length > ACTIVITY_CAP ? next.slice(-ACTIVITY_CAP) : next;
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

	async function startReboot(agentId) {
		rebootError = null;
		rebootNotice = null;
		try {
			const preflight = await fetchRebootPreflight(agentId);
			rebootPrompt = { agentId, preflight };
		} catch (cause) {
			rebootError = cause.message;
		}
	}

	async function confirmReboot(force) {
		if (!rebootPrompt) return;
		const { agentId } = rebootPrompt;
		try {
			const ack = await rebootAgent(agentId, { force });
			rebootPrompt = null;
			rebootNotice = `${agentId} rebooting${ack.wouldInstall ? ` to ${ack.wouldInstall}` : ""}…`;
		} catch (cause) {
			rebootError = cause.message;
		}
	}

	function dismissReboot() {
		rebootPrompt = null;
		rebootError = null;
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
					pushActivity(event.agent_id, event.entry);
					agents = agents.map((agent) =>
						agent.agentId === event.agent_id
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
	const selectedAgent = $derived(
		agents.find((agent) => agent.agentId === selected) ?? null
	);
</script>

<main>
	<header>
		<h1>mission control</h1>
		<p class="summary">
			{summary.total} agents
			{#if summary.live > 0}· {summary.live} live{/if}
			{#if summary.stuck > 0}· <span class="warn">{summary.stuck} stuck</span>{/if}
			{#if summary.rebooting > 0}· <span class="busy">{summary.rebooting} rebooting</span>{/if}
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
					<th>control</th>
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
						<td class="control">
							{#if agent.capabilities?.includes("reboot")}
								<button
									onclick={(event) => {
										event.stopPropagation();
										startReboot(agent.agentId);
									}}
									disabled={!agent.connected || agent.rebooting}
								>
									reboot
								</button>
							{:else}
								<span class="muted">—</span>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	{#if rebootError}
		<p class="error">reboot: {rebootError}</p>
	{/if}
	{#if rebootNotice}
		<p class="muted">{rebootNotice}</p>
	{/if}
	{#if rebootPrompt}
		<div class="confirm">
			<p>
				Reboot <strong>{rebootPrompt.agentId}</strong>? It would install
				<strong>{rebootPrompt.preflight.wouldInstall ?? "an unknown version"}</strong>.
			</p>
			{#if !rebootPrompt.preflight.supervised}
				<p class="error">No supervisor: this agent would not come back. Refused — use SSH.</p>
				<button onclick={dismissReboot}>dismiss</button>
			{:else if rebootPrompt.preflight.inFlight > 0}
				<p class="warn">
					{rebootPrompt.preflight.inFlight} turn(s) in flight — rebooting kills them.
				</p>
				<button onclick={() => confirmReboot(true)}>reboot anyway</button>
				<button onclick={dismissReboot}>cancel</button>
			{:else}
				<button onclick={() => confirmReboot(false)}>reboot now</button>
				<button onclick={dismissReboot}>cancel</button>
			{/if}
		</div>
	{/if}

	{#if selected}
		<section class="drilldown">
			<h2>
				{selected}
				<span class="muted">· {selectedAgent ? stateLabel(selectedAgent.state) : ""}</span>
			</h2>

			<div class="columns">
				<div class="live">
					<h3>
						live
						{#if selectedAgent && selectedAgent.stuck}
							<span class="warn">stuck</span>
						{:else if selectedAgent && selectedAgent.inFlight > 0}
							<span class="busy">in flight</span>
						{/if}
					</h3>
					{#if activityError}
						<p class="error">activity unavailable: {activityError}</p>
					{:else if activity.length === 0}
						<p class="muted">nothing published yet</p>
					{:else}
						<ol class="feed">
							{#each activity as line, i (i)}
								<li class={line.type}>
									<span class="at">{clock(line.at)}</span>
									<span class="kind">{label(line)}</span>
									{#if line.status}
										<span class="status" class:done={line.status === "completed"}>{line.status}</span>
									{/if}
									<span class="text">{line.text}</span>
								</li>
							{/each}
						</ol>
					{/if}
				</div>

				<div class="history">
					<h3>history</h3>
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
				</div>
			</div>
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
	.columns {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 1.5rem;
		align-items: start;
	}
	@media (max-width: 46rem) {
		.columns {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.busy {
		color: #6cf;
	}
	.feed {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 24rem;
		overflow-y: auto;
		border: 1px solid #222;
	}
	.feed li {
		display: flex;
		gap: 0.4rem;
		padding: 0.15rem 0.4rem;
		border-bottom: 1px solid #181818;
		font-size: 0.8rem;
	}
	.feed li:last-child {
		border-bottom: none;
	}
	.feed .at {
		color: #555;
		flex: 0 0 4.5rem;
	}
	.feed .kind {
		color: #777;
		text-transform: uppercase;
		font-size: 0.7rem;
		flex: 0 0 3.5rem;
	}
	.feed .status {
		flex: 0 0 4.5rem;
	}
	.feed .text {
		flex: 1 1 auto;
		min-width: 0;
	}
	.feed li.tool_call .text,
	.feed li.tool_call_update .text {
		color: #6cf;
	}
	.feed li.answer .text {
		color: #ded;
	}
	.feed li.failed,
	.feed li.failed .text {
		color: #a33;
	}
	.status {
		color: #c80;
		font-size: 0.7rem;
	}
	.status.done {
		color: #3a3;
	}
	.text {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
</style>
