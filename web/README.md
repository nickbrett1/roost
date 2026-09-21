# Frontend (Svelte)

This directory holds the project's Svelte app. It is a **sub-build**: the
project's primary language is `rust`, and this app is built to **static
assets** that the `rust` server serves. There is no adapter and no server
of its own here — a plain Vite build, or SvelteKit's `adapter-static`.

## Where the build output lands

`npm run build` writes the static assets to `web/dist`.

## Who serves it

The **`rust` server is the web server**, and it owns `/healthz`. Point it
at `web/dist`, either at **compile time** (e.g.
`rust-embed` or `include_dir!`) or at **runtime** from disk — both are copied
into the image by the Dockerfile. There is **no Node runtime** in the image.
genproj scaffolds the assets and this seam, not the serving code, so implement
`/healthz` (and the static file route) in the `rust` app.

## How it is built

- **CI** builds this directory in its own `.buildkite/pipeline.yml` step (on a
  Node image), before the `rust` build, so a broken frontend fails CI
  rather than being discovered at image-build time.
- **Docker** builds it in a `frontend` stage in the root `Dockerfile` and copies
  `web/dist` into the `rust` build stage and the
  runtime image.

## Working on it locally

```sh
cd web
npm install
npm run dev
```
