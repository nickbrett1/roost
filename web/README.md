# Frontend (SvelteKit)

This directory holds the project's Svelte app. It is a **sub-build**: the
project's primary language is `rust`, and this app is built separately
and consumed by the `rust` binary.

## Where the build output lands

`npm run build` writes the static assets to `web/build`.

## How it is built

- **CI** builds this directory in its own `.buildkite/pipeline.yml` step (on a
  Node image), before the `rust` build, so a broken frontend fails CI
  rather than being discovered at image-build time.
- **Docker** builds it in a `frontend` stage in the root `Dockerfile` and copies
  `web/build` into the `rust` build stage and the
  runtime image.

## How the `rust` binary consumes it

That is your decision, and the seam is deliberate: read
`web/build` either at **compile time** (e.g. `rust-embed`
or `include_dir!`) or at **runtime** from disk. Both are already copied into the
image by the Dockerfile.

## Working on it locally

```sh
cd web
npm install
npm run dev
```
