import adapter from "@sveltejs/adapter-node";

/** @type {import('@sveltejs/kit').Config} */
const config = {
  kit: {
    // adapter-node outputs a standalone Node server (build/index.js) for the Docker container
    // See https://kit.svelte.dev/docs/adapter-node for more information.
    adapter: adapter(),
  },
};

export default config;
