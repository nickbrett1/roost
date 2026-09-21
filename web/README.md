# Frontend (Svelte)

This directory holds the project's Svelte app. It is a **sub-build**: the
project's primary language is `rust`, and this app is built to **static
assets** that the `rust` server serves. There is no adapter and no server
of its own here — a plain Vite build, or SvelteKit's `adapter-static`.

## Where the build output lands

`npm run build` writes the static assets to `web/dist`.

## Who serves it

The **`rust` server is the web server**. There is **no Node runtime** in
the image: the frontend is built here and its `web/dist`
output is copied into the image by the Dockerfile.

genproj scaffolds a **minimal serving harness** as the `rust`
entry point (`src/main.rs`): it binds `0.0.0.0` on the container port
(3000 unless `docker-container.exposePort` says otherwise), serves
`web/dist`, answers the declared healthcheck path with
200, and 404s everything else. That is what makes the container come up and pass
its own `HEALTHCHECK` - the harness half of the project.

The **domain half is yours**: the wire protocol, the business endpoints, and
anything you want the server to do beyond serving the UI. Replace the harness
(and its standard-library-only constraint) with your application whenever you
are ready.

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
