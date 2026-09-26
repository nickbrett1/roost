<script>
	import { onMount } from "svelte";
	import {
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
	} from "./lib/api.js";
	import { FLEET_HASH, agentHash, parseRoute, sessionHash } from "./lib/route.js";

	let agents = $state([]);
	let connected = $state(false);
	let error = $state(null);
	let nowMs = $state(Date.now());

	// Drill-down state (memo §6.2): history is proxied to the agent on demand.
	// Two levels of it: the open agent, and the open session inside it. Both
	// come from the hash (see lib/route.js), never from a click handler alone.
	let selected = $state(null);
	let sessionId = $state(null);
	let historyError = $state(null);
	let search = $state("");
	let sessions = $state(null);
	let matches = $state(null);
	let transcript = $state(null);

	// History carries every turn, tool calls and tool results included, and
	// those steps have no words: drawn as-is they are a run of blank rows under
	// a bare role. The transcript is the conversation, so it draws the messages
	// that have text. The raw list stays whole because paging counts messages,
	// not utterances.
	const spoken = $derived(spokenMessages(transcript?.messages));

	// The live turn view (memo §6.3). Activity does not come from history: the
	// hub already holds the frames it was pushed, so this opens populated and
	// then follows the fan-out. It is stored raw and folded at render time,
	// which keeps the wire shape and the presentation from drifting apart.
	let activityEntries = $state([]);
	let activityError = $state(null);
	let activity = $derived(foldActivity(activityEntries));

	// The instant of the newest frame in the ring: the agent's own timestamp
	// when it sent one, the hub's receipt when it did not. Everything below
	// dates the panel from this, so the ring's clock and its age can never
	// disagree.
	const activityAtMs = $derived.by(() => {
		const newest = activityEntries[activityEntries.length - 1];
		if (!newest) return null;
		const stamped = newest.at ? Date.parse(newest.at) : Number.NaN;
		return Number.isFinite(stamped) ? stamped : newest.receivedAtMs;
	});

	// How long ago that was.
	const feedAge = $derived(activityAtMs === null ? null : relativeAge(activityAtMs, nowMs));

	// How recent a frame has to be for the panel to read as live. Beyond it the
	// ring is history - the last turn the agent streamed, however long ago - and
	// the panel says so instead of calling a day-old turn "live".
	const ACTIVITY_FRESH_MS = 10 * 60 * 1000;

	// A turn running right now is live whatever the last frame's age; otherwise
	// the newest frame has to be recent. A connected agent that has been idle
	// for a day is not live activity, and dressing it up as such is exactly how
	// an hour-old clock came to look like a broken page.
	const feedLive = $derived(
		(selectedAgent?.inFlight ?? 0) > 0 ||
			(activityAtMs !== null && nowMs - activityAtMs <= ACTIVITY_FRESH_MS)
	);

	// The newest session on disk. Session stamps arrive naive and are read as
	// the UTC they are (see parseStamp), so "newer than the feed" means the
	// same thing to the note as it does to the list.
	const newestSessionAtMs = $derived.by(() => {
		let newest = null;
		for (const entry of sessions ?? []) {
			const ms = parseStamp(entry.updatedAt);
			if (ms !== null && (newest === null || ms > newest)) newest = ms;
		}
		return newest;
	});

	// The confusing case: the agent has work the activity wire never carried,
	// so "last frame 1d ago" sits beside "session 6h ago". Worth explaining
	// rather than leaving to look like a stopped clock.
	const newerWorkInSessions = $derived(
		activityAtMs !== null &&
			newestSessionAtMs !== null &&
			newestSessionAtMs - activityAtMs > 60 * 1000
	);

	// The hub's ring is bounded (512); mirror that bound so a long-lived view
	// holds no more than the hub would have sent it anyway.
	const ACTIVITY_CAP = 512;

	function label(line) {
		if (line.type === "thought") return "think";
		if (line.type === "answer") return "say";
		if (line.type === "tool_call" || line.type === "tool_call_update") return "tool";
		return line.type.replaceAll("_", " ");
	}

	/// A frame's instant as a wall clock in the reader's own zone. The wire
	/// stamps everything UTC, which is right for comparing against a build log
	/// and wrong for reading: sliced straight out of the ISO string it looked
	/// four hours off, because nothing said which zone it was. The exact UTC
	/// instant is on the row's title for when precision matters.
	function clock(at) {
		const ms = at ? Date.parse(at) : Number.NaN;
		if (!Number.isFinite(ms)) return "";
		const when = new Date(ms);
		return [when.getHours(), when.getMinutes(), when.getSeconds()]
			.map((part) => String(part).padStart(2, "0"))
			.join(":");
	}

	/// A byte count at phone size. Deltas arrive a token at a time, so the
	/// numbers are small and get a decimal; anything bigger is rounded.
	function size(bytes) {
		if (bytes < 1024) return `${bytes} B`;
		return `${(bytes / 1024).toFixed(1)} KiB`;
	}

	/// A folded run of `answer`/`thought` frames whose agent published no text -
	/// only `deltaBytes` reached the wire. Say what did arrive, because an empty
	/// row reads as a broken page rather than as an agent's choice.
	function unspoken(line) {
		const parts = [`${line.chunks} delta${line.chunks === 1 ? "" : "s"}`];
		if (line.bytes > 0) parts.push(size(line.bytes));
		parts.push("text not published");
		return parts.join(" · ");
	}

	// The build stamp, baked in at bundle time by vite (`define` in
	// vite.config.js). Shown on the page because the only way to tell a stale
	// page from a quiet fleet is for the page to say which build it is.
	const build = {
		time: stampTime(__BUILD_TIME__),
		// CI hands us the full 40-character sha, which wraps to three lines on a
		// phone. Seven characters is what a commit is called in conversation.
		commit: __BUILD_COMMIT__.slice(0, 7),
		fullCommit: __BUILD_COMMIT__,
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
		location.hash = selected === agentId ? FLEET_HASH : agentHash(agentId);
	}

	/// Back one level. From a session that is the agent; from the agent that is
	/// the fleet. Same string either way, so one button serves both views.
	function back() {
		location.hash = selected && sessionId ? agentHash(selected) : FLEET_HASH;
	}

	// The hash is the single source of truth for which view is open, so a row
	// tap, a session tap and the back button all take this one path.
	function syncFromHash() {
		const route = parseRoute(location.hash);
		if (route.agentId === selected && route.sessionId === sessionId) return;
		if (route.agentId !== selected) {
			resetDrilldown();
			selected = route.agentId;
			if (route.agentId) {
				loadSessions(route.agentId);
				loadActivity(route.agentId);
			}
		}
		if (route.sessionId !== sessionId) transcript = null;
		sessionId = route.sessionId;
		if (route.sessionId) loadTranscript(route.agentId, route.sessionId);
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

	// The agent pages history two messages at a time and only forward, so a
	// session's first page is its opening prompt - which is how a 269-message
	// conversation opened on two lines and read as empty. Aim for this many
	// messages, and start that far from the end.
	const TRANSCRIPT_WINDOW = 20;

	/// The session's message count, from whichever list already has it. Null
	/// when the session was deep-linked before its list arrived.
	function sessionCountFor(id) {
		const row = [...(sessions ?? []), ...(matches ?? [])].find(
			(entry) => entry.sessionId === id
		);
		return typeof row?.messageCount === "number" ? row.messageCount : null;
	}

	/// A run of messages from `start`, following the agent's forward-only
	/// cursors until the window is full or the session runs out. Cursors are
	/// message offsets, so seeking to the tail is one cursor, not a walk.
	async function fetchWindow(agentId, id, start) {
		let cursor = start > 0 ? String(start) : null;
		const messages = [];
		let nextCursor = null;
		for (let page = 0; page < TRANSCRIPT_WINDOW; page += 1) {
			const got = await fetchHistoryMessages(agentId, id, cursor ? { cursor } : {});
			if (got.messages.length === 0) break;
			messages.push(...got.messages);
			nextCursor = got.nextCursor;
			if (!nextCursor) break;
			cursor = nextCursor;
		}
		return { messages, nextCursor };
	}

	async function loadTranscript(agentId, id) {
		try {
			const count = sessionCountFor(id);
			const start = count === null ? 0 : Math.max(0, count - TRANSCRIPT_WINDOW);
			let window = await fetchWindow(agentId, id, start);
			// A seek the agent did not honour must not read as an empty session.
			if (window.messages.length === 0 && start > 0) {
				window = await fetchWindow(agentId, id, 0);
			}
			transcript = { sessionId: id, ...window };
			historyError = null;
		} catch (cause) {
			historyError = cause.message;
		}
	}

	/// Reach back for the messages before the window, keeping what is on screen
	/// where it is.
	async function loadEarlier() {
		if (!selected || !sessionId || !transcript) return;
		const first = transcript.messages[0]?.index ?? 0;
		if (first <= 0) return;
		try {
			const start = Math.max(0, first - TRANSCRIPT_WINDOW);
			const earlier = await fetchWindow(selected, sessionId, start);
			const head = earlier.messages.filter((message) => (message.index ?? 0) < first);
			transcript = { ...transcript, messages: [...head, ...transcript.messages] };
			historyError = null;
		} catch (cause) {
			historyError = cause.message;
		}
	}

	/// Opening a session replaces the session list with its transcript: the
	/// list is what you were reading, so it is not kept underneath.
	function openSession(id) {
		location.hash = sessionHash(selected, id);
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
							? {
									...agent,
									lastEventAtMs: Date.now(),
									// The table shows the agent's own stamp, so a live frame
									// has to carry it too - otherwise the row keeps the age
									// of the previous event until the next snapshot arrives.
									lastEventAt: event.entry.at ?? agent.lastEventAt,
									eventCount: agent.eventCount + 1
								}
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
	// ordered by the last event, newest first - by the same number the column
	// shows, so the rows are never in an order the ages disagree with. An agent
	// that has never published anything (null) sorts to the bottom rather than
	// the top, and the agentId tiebreak keeps the order stable as the ages tick.
	const sortedAgents = $derived(
		[...agents].sort((a, b) => {
			const at = eventAtMs(a) ?? Number.NEGATIVE_INFINITY;
			const bt = eventAtMs(b) ?? Number.NEGATIVE_INFINITY;
			if (bt !== at) return bt - at;
			return a.agentId.localeCompare(b.agentId);
		})
	);

	// Every dev box runs the same kind of agent, so the `kind` column is dead
	// width on a phone. Show it only when the fleet actually disagrees about
	// what kind of thing it holds.
	const showKind = $derived(new Set(agents.map((agent) => agent.kind)).size > 1);

	// A session's own name when we have it (from the list or a search match),
	// else its id: a deep link must still be able to title the view it opened.
	const sessionName = $derived(
		[...(sessions ?? []), ...(matches ?? [])].find((entry) => entry.sessionId === sessionId)
			?.name ??
			sessionId ??
			""
	);
</script>

<main>
	<header>
		{#if selected}
			<button class="back" onclick={back}>{sessionId ? `← ${selected}` : "← fleet"}</button>
		{/if}
		<h1>{selected ? (sessionId ? sessionName : selected) : "mission control"}</h1>
		{#if selected}
			{#if sessionId}
				<span class="muted">{sessionId}</span>
			{:else if selectedAgent}
				<span class="chip {selectedAgent.state}">{stateLabel(selectedAgent.state)}</span>
			{/if}
		{:else}
			<p class="summary">
				{summary.total} agents
				{#if summary.live > 0}· {summary.live} connected{/if}
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
		{#if sessionId}
			<!-- The session's own view. The session list it came from is not kept
			     underneath: on a phone that is what pushed the transcript off the
			     bottom of the screen. -->
			<section class="drilldown">
				{#if historyError}
					<p class="error">history unavailable: {historyError}</p>
				{:else if transcript === null}
					<p class="muted">loading…</p>
				{:else if transcript.messages.length === 0}
					<p class="muted">no messages in this session</p>
				{:else}
					{#if (transcript.messages[0]?.index ?? 0) > 0}
						<p>
							<button onclick={loadEarlier}>load earlier messages</button>
						</p>
					{/if}
					{#if spoken.length === 0}
						<p class="muted">no messages with text in this window</p>
					{:else}
						{#each spoken as message, i (message.index ?? i)}
							<p class="message">
								<span class="role">{roleLabel(message.role)}</span>
								{message.text}
							</p>
						{/each}
					{/if}
					{#if transcript.nextCursor}
						<p class="muted">more messages available</p>
					{/if}
				{/if}
			</section>
		{:else}
		<section class="drilldown">
			<div class="columns">
				{#snippet feedList()}
					<ol class="feed">
						{#each activity as line, i (i)}
							<li class={line.type}>
								<span class="at" title={stampTime(line.at)}>{clock(line.at)}</span>
								<span class="kind">{label(line)}</span>
								{#if line.status}
									<span class="status" class:done={line.status === "completed"}>{line.status}</span>
								{/if}
								<span class="text" class:unspoken={!line.text && line.bytes > 0}>
									{line.text || (line.bytes > 0 ? unspoken(line) : "")}
								</span>
							</li>
						{/each}
					</ol>
				{/snippet}
				<div class="live">
					<h3>
						activity
						{#if selectedAgent && selectedAgent.stuck}
							<span class="warn">stuck</span>
						{:else if selectedAgent && selectedAgent.inFlight > 0}
							<span class="busy">in flight</span>
						{/if}
						{#if feedAge}
							<!-- Wall clocks are in the reader's zone (the build stamp is
							     the one thing on the page that stays UTC, and it says so);
							     and the age of the newest frame is stated here so that a
							     stale ring says so instead of looking live. -->
							<span class="note">last frame {feedAge} · local time</span>
						{/if}
					</h3>
					{#if activityError}
						<p class="error">activity unavailable: {activityError}</p>
					{:else if activity.length === 0}
						<p class="muted">nothing published yet</p>
					{:else if feedLive}
						{@render feedList()}
					{:else}
						<!-- The ring is history, not a running turn. Say what it is and
						     keep it one click away, rather than dressing a day-old turn as
						     live or hiding it as if it were not there. -->
						<p class="muted">
							no streamed activity in the last {Math.round(ACTIVITY_FRESH_MS / 60000)} minutes - this
							is the agent's last turn, {feedAge}.
						</p>
						{#if newerWorkInSessions}
							<p class="muted">
								newer work is in <strong>sessions</strong>: the activity wire only carries turns
								streamed through roost.
							</p>
						{/if}
						<details>
							<summary>show the last {activity.length} frames</summary>
							{@render feedList()}
						</details>
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
											parseStamp(session.updatedAt),
											nowMs
										)}
									</span>
								</p>
							{/each}
						{/if}
					{/if}
				</div>
			</div>
		</section>
		{/if}
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
				</tr>
			</thead>
			<tbody>
				{#each sortedAgents as agent (agent.agentId)}
					<tr class:selected={selected === agent.agentId} onclick={() => openAgent(agent.agentId)}>
						<!-- The state is a dot rather than its own column: the chip said
						     "offline" in as much width as a long agentId needs to be
						     readable on a phone, and a coloured dot says the same thing
						     in seven pixels. The word lives in the title/aria-label for
						     anyone who needs it spelled out. -->
						<td class="agent">
							<span
								class="state-dot {agent.state}"
								role="img"
								aria-label={stateLabel(agent.state)}
								title={stateLabel(agent.state)}
							></span>{agent.agentId}
						</td>
						{#if showKind}
							<td>{agent.kind}</td>
						{/if}
						<td>{agent.agentVersion}</td>
						<td class="num">{agent.inFlight}</td>
						<td class="when">{relativeAge(eventAtMs(agent), nowMs)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	<footer class="build">
		{#if build.number}<span class="tag">#{build.number}</span>{/if}
		<span class="when">{build.time}</span>
		<span class="sha" title={build.fullCommit}>{build.commit}</span>
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
	/* The state column, folded into the agent cell: seven pixels of the same
	   palette the chip used, so liveness still reads at a glance on a phone. */
	.state-dot {
		display: inline-block;
		width: 7px;
		height: 7px;
		margin-right: 6px;
		border-radius: 50%;
		vertical-align: middle;
		background: #94a3b8;
	}
	.state-dot.live {
		background: #34d399;
	}
	.state-dot.stuck {
		background: #fdba74;
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
	/* Both the panel note and a delta run with no text are meta, not content:
	   quiet enough to skip, present enough to explain the row. The note keeps
	   the heading's own dimness; it is a sentence, not a label, so it drops the
	   uppercase and the letter-spacing. */
	.note {
		text-transform: none;
		letter-spacing: 0;
		font-weight: 400;
	}
	/* The gated feed: the frames are kept, but folded away until asked for, so
	   the drill-down opens on the sessions it should have been read against. */
	.live details {
		margin-top: 0.5rem;
	}
	.live summary {
		cursor: pointer;
		font-size: 11px;
		opacity: 0.7;
	}
	.live summary:hover {
		opacity: 1;
	}
	.feed .text.unspoken {
		font-weight: 400;
		opacity: 0.6;
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
