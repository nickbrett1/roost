<script>
	import { onMount } from "svelte";
	import {
		fetchAgentActivity,
		fetchFleet,
		fetchHistoryMessages,
		fetchHistorySessions,
		fleetSummary,
		foldActivity,
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

	// The live turn view (memo §6.3). Activity does not come from history: the
	// hub already holds the frames it was pushed, so this opens populated and
	// then follows the fan-out. It is stored raw and folded at render time,
	// which keeps the wire shape and the presentation from drifting apart.
	let activityEntries = $state([]);
	let activityError = $state(null);
	let activity = $derived(foldActivity(activityEntries));

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

	// The build stamp, baked in at bundle time by vite (`define` in
	// vite.config.js). Shown on the page because the only way to tell a stale
	// page from a quiet fleet is for the page to say which build it is.
	const build = {
		time: stampTime(__BUILD_TIME__),
		commit: __BUILD_COMMIT__,
		message: __BUILD_MESSAGE__,
		number: __BUILD_NUMBER__
	};

	/// ISO instant (UTC) -> "YYYY-MM-DD HH:MM UTC". Sliced, not localised: on a
	/// phone the locale and timezone would make this harder to compare against a
	/// build log, which is the only thing it is for.
	function stampTime(iso) {
		return iso && iso.length >= 16 ? `${iso.slice(0, 10)} ${iso.slice(11, 16)} UTC` : "";
	}

	async function load() {
		try {
			agents = await fetchFleet();
			error = null;
		} catch (cause) {
			error = cause.message;
		}
	}

	// The drill-down is its own view, not a panel hung below the table: on a
	// phone the table plus the panel put the conversation off the bottom of the
	// screen. Selecting an agent replaces the fleet view with it, and the hash
	// carries the choice so the browser's back button and a reload both work.
	function resetDrilldown() {
		search = "";
		matches = null;
		transcript = null;
		historyError = null;
		sessions = null;
		activityEntries = [];
		activityError = null;
	}

	function openAgent(agentId) {
		if (selected === agentId) {
			location.hash = "#/";
			return;
		}
		location.hash = `#/agent/${encodeURIComponent(agentId)}`;
	}

	function back() {
		location.hash = "#/";
	}

	function agentFromHash() {
		const match = /^#\/agent\/(.+)$/.exec(location.hash);
		return match ? decodeURIComponent(match[1]) : null;
	}

	// The hash is the single source of truth for which view is open, so both
	// the row click and the back button take the same path through here.
	function syncFromHash() {
		const agentId = agentFromHash();
		if (agentId === selected) return;
		resetDrilldown();
		selected = agentId;
		if (agentId) {
			loadSessions(agentId);
			loadActivity(agentId);
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

	onMount(() => {
		load();
		syncFromHash();
		window.addEventListener("hashchange", syncFromHash);

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
			window.removeEventListener("hashchange", syncFromHash);
		};
	});

	const summary = $derived(fleetSummary(agents));
	const selectedAgent = $derived(
		agents.find((agent) => agent.agentId === selected) ?? null
	);

	// The operator's first question is "who is doing something", so the fleet is
	// ordered by the last event, newest first. An agent that has never published
	// anything (lastEventAtMs null) sorts to the bottom rather than the top, and
	// the agentId tiebreak keeps the order stable as the ages tick.
	const sortedAgents = $derived(
		[...agents].sort((a, b) => {
			const at = a.lastEventAtMs ?? Number.NEGATIVE_INFINITY;
			const bt = b.lastEventAtMs ?? Number.NEGATIVE_INFINITY;
			if (bt !== at) return bt - at;
			return a.agentId.localeCompare(b.agentId);
		})
	);

	// Every dev box runs the same kind of agent, so the `kind` column is dead
	// width on a phone. Show it only when the fleet actually disagrees about
	// what kind of thing it holds.
	const showKind = $derived(new Set(agents.map((agent) => agent.kind)).size > 1);
</script>

<main>
	<header>
		{#if selected}
			<button class="back" onclick={back}>← fleet</button>
		{/if}
		<h1>{selected ?? "mission control"}</h1>
		{#if selected}
			{#if selectedAgent}
				<span class="chip {selectedAgent.state}">{stateLabel(selectedAgent.state)}</span>
			{/if}
		{:else}
			<p class="summary">
				{summary.total} agents
				{#if summary.live > 0}· {summary.live} live{/if}
				{#if summary.stuck > 0}· <span class="warn">{summary.stuck} stuck</span>{/if}
				{#if summary.offline > 0}· {summary.offline} offline{/if}
			</p>
		{/if}
		<span class="link" class:up={connected}>{connected ? "live" : "disconnected"}</span>
	</header>

	{#if error}
		<p class="error">Could not reach the hub API: {error}</p>
	{/if}

	{#if selected}
		<section class="drilldown">
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
	{:else if agents.length === 0}
		<p class="empty">No agents connected. No tunnel client has dialed in yet.</p>
	{:else}
		<table>
			<thead>
				<tr>
					<th>agent</th>
					{#if showKind}
						<th>kind</th>
					{/if}
					<th>version</th>
					<th class="num">in flight</th>
					<th>last event</th>
					<th>state</th>
				</tr>
			</thead>
			<tbody>
				{#each sortedAgents as agent (agent.agentId)}
					<tr class:selected={selected === agent.agentId} onclick={() => openAgent(agent.agentId)}>
						<td class="agent">{agent.agentId}</td>
						{#if showKind}
							<td>{agent.kind}</td>
						{/if}
						<td>{agent.agentVersion}</td>
						<td class="num">{agent.inFlight}</td>
						<td class="when">{relativeAge(agent.lastEventAtMs, nowMs)}</td>
						<td><span class="chip {agent.state}">{stateLabel(agent.state)}</span></td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	<footer class="build">
		{#if build.number}<span class="tag">#{build.number}</span>{/if}
		<span class="when">{build.time}</span>
		<span class="sha">{build.commit}</span>
		{#if build.message}<span class="subject">{build.message}</span>{/if}
	</footer>
</main>

<style>
	/* House style, borrowed from deepseek-balance: a system UI sans on a slate
	   ground, soft translucent borders, and small uppercase micro-labels. */
	:global(html) {
		background: #0f172a;
		color-scheme: dark;
	}
	:global(body) {
		margin: 0;
		background: #0f172a;
		color: #e2e8f0;
		font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
		font-size: 13px;
		line-height: 1.4;
	}
	main {
		max-width: 56rem;
		margin: 0 auto;
		padding: 1.25rem 1rem 3rem;
	}
	header {
		display: flex;
		align-items: baseline;
		gap: 0.75rem;
		border-bottom: 1px solid rgba(148, 163, 184, 0.12);
		padding-bottom: 0.5rem;
		margin-bottom: 0.75rem;
	}
	h1 {
		font-size: 15px;
		font-weight: 600;
		margin: 0;
	}
	h3 {
		font-size: 10px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		opacity: 0.6;
		margin: 1rem 0 0.35rem;
	}
	.summary {
		margin: 0;
		font-size: 12px;
		opacity: 0.6;
	}
	.link {
		margin-left: auto;
		font-size: 12px;
		color: #f87171;
	}
	.link.up {
		color: #34d399;
	}
	.warn {
		color: #fdba74;
	}
	.empty,
	.muted {
		opacity: 0.55;
	}
	.error {
		color: #f87171;
	}
	table {
		width: 100%;
		border-collapse: collapse;
	}
	th,
	td {
		text-align: left;
		padding: 5px 6px;
		border-bottom: 1px solid rgba(148, 163, 184, 0.12);
		font-size: 12px;
		vertical-align: baseline;
	}
	th {
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-size: 10px;
		opacity: 0.6;
		white-space: nowrap;
	}
	td.num,
	th.num {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	td.when {
		white-space: nowrap;
		font-variant-numeric: tabular-nums;
		opacity: 0.8;
	}
	td.agent {
		font-weight: 600;
		white-space: nowrap;
	}
	tbody tr {
		cursor: pointer;
	}
	tbody tr:hover {
		background: rgba(148, 163, 184, 0.06);
	}
	tr.selected {
		background: rgba(56, 189, 248, 0.08);
	}
	.chip {
		display: inline-block;
		font-size: 10px;
		font-weight: 600;
		padding: 1px 7px;
		border-radius: 999px;
		white-space: nowrap;
		background: rgba(148, 163, 184, 0.15);
		color: #94a3b8;
	}
	.chip.live {
		background: rgba(52, 211, 153, 0.15);
		color: #6ee7b7;
	}
	.chip.stuck {
		background: rgba(251, 146, 60, 0.15);
		color: #fdba74;
	}
	.drilldown {
		border-top: 1px solid rgba(148, 163, 184, 0.12);
		margin-top: 1rem;
		padding-top: 0.5rem;
	}
	/* The drill-down is a view, not a panel: this is the way back to the fleet. */
	.back {
		padding: 1px 8px;
		font-size: 11px;
		color: #7dd3fc;
	}
	.searchbar {
		display: flex;
		gap: 0.5rem;
		margin: 0.35rem 0;
	}
	.searchbar input {
		flex: 1;
		min-width: 0;
		background: rgba(15, 23, 42, 0.6);
		border: 1px solid rgba(148, 163, 184, 0.25);
		border-radius: 6px;
		color: inherit;
		padding: 5px 8px;
		font: inherit;
	}
	.searchbar input:focus {
		outline: none;
		border-color: rgba(125, 211, 252, 0.6);
	}
	button {
		background: transparent;
		border: 1px solid rgba(148, 163, 184, 0.25);
		border-radius: 6px;
		color: #cbd5e1;
		padding: 5px 12px;
		font: inherit;
		cursor: pointer;
	}
	button:hover {
		background: rgba(148, 163, 184, 0.1);
	}
	button.link {
		background: none;
		border: none;
		padding: 0;
		color: #7dd3fc;
		text-decoration: underline;
	}
	.message {
		margin: 0.25rem 0;
	}
	.role {
		opacity: 0.55;
		margin-right: 0.5rem;
	}
	.columns {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
		gap: 1.25rem;
		align-items: start;
	}
	@media (max-width: 46rem) {
		.columns {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	.busy {
		color: #7dd3fc;
	}
	.feed {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 24rem;
		overflow-y: auto;
		border: 1px solid rgba(148, 163, 184, 0.13);
		border-radius: 8px;
		background: rgba(15, 23, 42, 0.35);
	}
	.feed li {
		display: flex;
		gap: 0.4rem;
		padding: 0.2rem 0.5rem;
		border-bottom: 1px solid rgba(148, 163, 184, 0.08);
		font-size: 12px;
	}
	.feed li:last-child {
		border-bottom: none;
	}
	.feed .at {
		color: #60a5fa;
		opacity: 0.7;
		flex: 0 0 4.5rem;
		font-variant-numeric: tabular-nums;
	}
	.feed .kind {
		opacity: 0.6;
		text-transform: uppercase;
		font-size: 10px;
		font-weight: 600;
		letter-spacing: 0.04em;
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
		color: #7dd3fc;
	}
	.feed li.answer .text {
		color: #e2e8f0;
	}
	.feed li.failed,
	.feed li.failed .text {
		color: #f87171;
	}
	.status {
		color: #fdba74;
		font-size: 10px;
		font-weight: 600;
	}
	.status.done {
		color: #34d399;
	}
	.text {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	/* The build stamp. Deliberately the dullest thing on the page: it is read
	   when something looks wrong, not when it looks right. */
	.build {
		display: flex;
		flex-wrap: wrap;
		gap: 0.4rem;
		align-items: baseline;
		margin-top: 2rem;
		padding-top: 0.5rem;
		border-top: 1px solid rgba(148, 163, 184, 0.12);
		font-size: 10px;
		opacity: 0.45;
	}
	.build .when {
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
	}
	.build .sha {
		font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
	}
	.build .subject {
		overflow-wrap: anywhere;
	}
</style>
