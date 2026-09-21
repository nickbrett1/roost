<script>
	let count = $state(0);

	function increment() {
		count += 1;
	}
</script>

<main>
	<h1>Welcome to Svelte</h1>
	<button onclick={increment}>
		Clicks: {count}
	</button>
</main>
