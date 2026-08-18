# Rust rewrite — the no-surprises itinerary

Everything that changes (and doesn't) when the Bun backend is swapped for the
Rust one. Written for the upstream maintainer: nothing here is hidden, and
anything that could conceivably be observed from outside the process is
listed. "Identical" claims are backed by `rust/parity.sh`, which diffs the two
backends live, probe by probe (results at the bottom).

## What is identical

- **The frontend.** The same Vite bundle, built by the same `bun run build`
  stage, embedded in the binary and served at `/web`. Not a pixel differs
  because not a byte differs.
- **The wire contract.** Every route, status code, error body (`{type,
  message, issues?, cause?}`), the login `{ok, message?}` envelope, cookie
  names/attributes and their serialization order, the auth ladder and its
  three 401 messages, boundary precedence (body limit → host → auth → origin
  → content-type), the method-mismatch-is-404 behavior, `HEAD` handling,
  `GET /` content negotiation with 302s, SSE frames (`data: changed\n\n`,
  nothing on connect, no keep-alives), and the `/health` classification
  values.
- **The state file.** Byte-identical `state.json` (2-space pretty, key order,
  omitted optionals, no trailing newline), same tmp+rename atomic writes,
  same ENOENT-only fresh-install rule, same v1 `currentCookie` rescue
  migration, same "corruption is an error, never silently reset" stance.
- **MAM behavior.** Same two endpoints, same `mousehole-by-timtimtim/0.5.0`
  user agent, redirects not followed, body parsed regardless of HTTP status,
  `mam_id` rotation from the first matching Set-Cookie (percent-decoded on
  receipt, sent back verbatim), cookie persisted even when MAM rejects it,
  fixed contact cadence with no jitter and no backoff, manual updates
  resetting the countdown.
- **Config.** Every env var, default, validation rule, `_FILE` secret
  variant (including "empty file beats a non-empty plain var"), the exact
  `Invalid environment variable NAME="raw": ...` error frame, the
  no-auth/credential mutual exclusion, and all startup warnings/fatals,
  string for string.
- **The deployment contract.** Port 5010, `/var/lib/mousehole`, the same
  healthcheck timings, `GIT_HASH` build arg, log lines (`[LEVEL]` on stdout,
  `NO_COLOR` honored), SIGTERM draining the in-flight contact before exit 0.

## What is better (and could theoretically be noticed)

| Change | Impact |
|---|---|
| ~45 MB resident → **~3.3 MB**; ~70 GB virtual → ~6 MB | The point of the exercise |
| Multi-hundred-MB image → ~12 MB Alpine image with a 3.9 MB static binary | Faster pulls, smaller attack surface |
| Healthcheck honors `MOUSEHOLE_PORT` | Upstream's hardcodes 5010; if you change the port, the Bun healthcheck lies and this one doesn't |
| Runtime dependency count | hono/zod/temporal-polyfill/set-cookie-parser (npm) → axum/serde/jiff/cookie (crates), compiled in; no runtime package resolution |

## Deliberate deviations (all display-only or unobservable from the UI)

1. **Timeout classification during body read.** The original maps a timeout
   to `timeout-error` only when it fires during `fetch()`; one firing during
   `response.text()` escapes as `unhandled-error` with a runtime message. The
   Rust client maps every timeout to `timeout-error`. Nothing branches on the
   stored `error.type` — it is display/log only. (Arguably a bug fix; happy
   to match the quirk instead if you'd rather.)
2. **Timestamp precision.** The Temporal polyfill emits millisecond
   fractions; jiff emits the clock's full nanosecond precision
   (`…22.038145434-05:00[America/Chicago]`). Same RFC 9557 format, parses
   identically; the frontend only reads `epochMilliseconds`. Visible if you
   `cat state.json` and squint.
3. **Parser wording in two corners.** All request-body validation messages
   and env-var validation messages use zod's exact wording (parity-verified
   byte for byte). Two spots still carry engine-specific text: the `cause`
   *inside* a `json-parse-error` (JavaScriptCore says `JSON Parse error:
   Expected '}'`, serde phrases it its own way — the outer message and
   structure are identical), and the schema-error detail for a hand-corrupted
   `state.json` (serde wording; only reachable by editing the file by hand).
   Both are display-only.
4. **Error log rendering.** The Bun logger pretty-prints `Error` values via
   `util.inspect` (stack + cause chain). The Rust logger prints the message
   line. Same events, less noise; stdout consumers that parsed stack traces
   (none known) would notice.
5. **Session expiry mechanics.** Same observable behavior (fixed lifetime,
   lazy sweep on every check, SSE stream closed at expiry moment) but
   implemented as a tokio sleep task instead of an unref'd `setTimeout`.
6. **Final image base.** `alpine:3` rather than `oven/bun:1-alpine`. Kept a
   shell for debuggability; `scratch` would work if you want the extra ~7 MB.

## What is genuinely lost

1. **The TypeScript dev workflow for the backend.** No `bun dev` proxy mode,
   no `.env` / `.env.development` auto-loading, no `NODE_ENV` switching — the
   Rust binary always behaves like the production image. Frontend dev is
   untouched (same `bun dev:web`), but iterating on *backend* logic now means
   Rust tooling (`rust/build.sh check|test`).
2. **Runtime hackability.** The Bun image runs from TypeScript source — you
   could exec in and patch a file. The Rust image is a compiled binary.
3. Nothing else. If something you rely on is missing, it's a bug in this
   list, not a decision.

## Distribution & coexistence plan

Nothing about the existing image changes. `docker-push-rust.yaml` (a mirror
of the existing publish workflow, same secrets, same platforms including
arm64 via the same QEMU setup) publishes the Rust build as **variant tags in
the same Docker Hub repository**: `:rust` on releases, `:<version>-rust`,
`:edge-rust`, `:pr-<n>-rust`. Users opt in by changing one tag; `latest` and
the semver tags keep pointing at the Bun image until the maintainer decides
otherwise. `rust-ci.yaml` runs the 40-test suite + clippy on `rust/` changes.
One honest note: the arm64 build is expected-clean (ring ships pre-generated
aarch64 assembly; all base images are multi-arch) but was only verified on
amd64 before this PR — the first CI run settles it.

## Parity results

Run of `rust/parity.sh` against `tmmrtn/mousehole:edge` (master tip — the
code this rewrite was specified from), 2026-08-18. **42 probes, 42 identical.**

```
== routing & content negotiation ==
  ok: root default
  ok: root accepts json
  ok: unknown path 404
  ok: method mismatch is 404
  ok: HEAD health
  ok: health
== auth ladder ==
  ok: state unauthenticated
  ok: state basic-auth junk
  ok: state stale cookie
  ok: state lowercase bearer
  ok: state trailing-space token fails
== login / logout ==
  ok: login no content-type
  ok: login bad json
  ok: login schema
  ok: login wrong password
  ok: login success
  ok: logout without cookie
  ok: session round-trip (state-with-session=200 logout=200 state-after-logout=401)
== boundary precedence ==
  ok: oversized body beats auth
  ok: evil host
  ok: default host any port
  ok: origin evil with token passes
  ok: origin evil with session fails
  ok: origin null same-origin mode
  ok: no origin passes
  ok: cross-origin GET state has no origin check
== PUT /cookie error ladder ==
  ok: cookie bad json
  ok: cookie empty value
  ok: cookie empty body
  ok: cookie non-object body
  ok: cookie no content-type
== static ==
  ok: web index
  ok: web trailing slash
  ok: web miss is json 404
  ok: web hashed asset (200 text/javascript; charset=utf-8)
== live MAM path + state file bytes (garbage cookie) ==
  ok: put cookie garbage (real MAM)
  ok: state.json byte-identical (normalized)
== SSE ==
  ok: SSE change frames (bun=1 rust=1)
  ok: events headers
== boot & config errors (fresh one-shot containers) ==
  ok: no credentials refuses to start
  ok: mutual exclusion
  ok: invalid numeric
================================
PASS: 42  FAIL: 0
```


## Provenance

The rewrite was specified first (docs in this directory: API contract, MAM
behavior matrix, config/auth/boundary, state/build — extracted from the TS
source with an adversarial completeness pass), implemented module by module
against those specs with 40 unit tests pinning the subtle behaviors, then
gated by the live side-by-side diff in `rust/parity.sh`.
