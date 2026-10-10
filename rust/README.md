# mousehole, but in Rust

A drop-in Rust rewrite of the backend. The React frontend is reused verbatim —
the Vite bundle is built exactly as before and embedded into the binary, so
the UI is pixel-identical because it *is* identical. Same port, same env vars,
same volume, same wire bytes; your compose file shouldn't notice the swap.

Why bother? Memory, mostly. The Bun backend idles around 45 MB of resident
memory (plus a scary-looking ~70 GB of virtual address space courtesy of
JavaScriptCore — harmless, but it tops every htop). The Rust backend idles at
**~3.3 MB resident** in a **3.9 MB static binary**. For a thing that phones
MAM once every five minutes, that felt more proportionate.

## Building

```
docker build -f Dockerfile.rust -t mousehole-rs --build-arg GIT_HASH=$(git rev-parse --short HEAD) .
```

That builds the web UI with Bun (same stage as the original image), compiles
the backend against musl, and ships both in a small Alpine image. The
container healthcheck is `mousehole healthcheck` — same timings as before.

For a dev loop without Docker there's `build.sh` (`check` / `test` /
`release`), which runs cargo inside a `rust:1-alpine` container so you're
always building the artifact you'll actually ship. A placeholder `dist/` is
generated when the frontend hasn't been built yet.

## Parity

The rewrite was done against the spec set in `docs/rust-rewrite/` (extracted
from the TypeScript source, wire detail by wire detail) and is gated by
`parity.sh`, which boots the Bun image and the Rust binary side by side with
identical config and diffs their answers — status codes, headers, error
bodies, cookies, SSE frames, even the bytes of `state.json`. Run it yourself:

```
bash rust/parity.sh
```

### Known deliberate differences

All of these are invisible to the UI:

- A timeout while reading a request body is classified `timeout-error` (Bun gives `unhandled-error`).
- Timestamps carry full nanosecond precision from jiff (Bun/Temporal gives milliseconds).
- Two corners keep serde's wording: the `json-parse-error` cause and the corrupt-`state.json` `schema-error` detail.
- Error logs print the message only, with no stack.
- Session expiry runs on a tokio sleep task.
- The final image is based on `alpine:3`.
- Not carried over: Bun's dev proxy mode, `.env` / `.env.development` auto-loading and `NODE_ENV` switching (the Rust binary always runs as production).

Image tags (in `tmmrtn/mousehole`): `:rust` (stable pointer), `:<version>-rust`, `:edge-rust` (master) and `:pr-<n>-rust`. `latest` and the semver tags stay on the Bun image. The arm64 build has only been verified on amd64.

Dev loop: `rust/build.sh check|test|release` runs cargo inside `rust:1-alpine`; `bash rust/parity.sh` diffs both backends; `rust-ci.yaml` runs `cargo test` and clippy on changes under `rust/`.

## For maintainers: the zero-effort investigation path

```
docker build -f Dockerfile.rust -t mousehole-rs .   # build it
bash rust/parity.sh                                  # watch 42 probes agree
```

That's the whole audit loop — parity.sh detects the image automatically and
boots it against `tmmrtn/mousehole:edge`.

CI is included and needs nothing beyond a merge:

- `rust-ci.yaml` runs `cargo test` + clippy on anything touching `rust/`.
- `docker-push-rust.yaml` mirrors the existing publish workflow with
  `Dockerfile.rust` and **variant tags in the same repository**: releases
  publish `:rust` (the stable pointer) and `:<version>-rust`, master pushes
  `:edge-rust`, PRs get `:pr-<n>-rust`. Same `linux/amd64,linux/arm64`
  platforms, same secrets/vars as the existing workflow. Existing users on
  `latest`/semver tags see nothing change until you decide `latest` should
  point at the Rust build — if you ever do.

## What's NOT here

The TypeScript dev workflow. `bun dev` (Vite dev server + proxy),
`.env.development` auto-loading, and `NODE_ENV` switching are Bun-side
conveniences the Rust binary doesn't replicate — it always behaves like
production. Frontend development is unchanged (it's the same frontend).
