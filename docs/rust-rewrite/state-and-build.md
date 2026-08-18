# State Persistence + Build/Packaging Spec (Rust rewrite)

Scope: the persisted state file (schema, serde, migrations, atomicity, error
behavior), the frontend build and how the backend serves it, the Docker
packaging, the process startup/shutdown sequence, and the Rust packaging plan.

Sources of truth read for this spec (all paths repo-relative):

- `src/backend/state/store.ts`, `src/backend/state/serde.ts`,
  `src/backend/state/migrate.ts`
- `src/backend/config.ts` (state-dir env), `src/backend/error.ts` (error types
  raised by the store), `src/backend/contact.ts` (who reads/writes state)
- `src/index.ts`, `src/backend/server.ts`, `src/backend/app.ts`,
  `src/backend/context.ts`, `src/backend/logger.ts`
- `vite.config.ts`, `package.json`, `Dockerfile`, `.dockerignore`,
  `.env.development`, `src/shared/git-hash.ts`, `src/shared/time.ts`,
  `src/shared/public-state.ts`
- `docs/compose-setups.md`, `docs/docker-compose-examples/*.md`,
  `contrib/unraid/my-mousehole.xml`, `.github/workflows/docker-push.yaml`,
  `CHANGELOG.md`

Related specs: `api-contract.md` (HTTP surface), `config-auth-boundary.md`
(full env/auth spec), `mam-behavior.md` (contact cycle). This file only repeats
what it needs.

---

## 1. State persistence

### 1.1 Location

- Env var: `MOUSEHOLE_STATE_DIR_PATH` (a **directory**). Value is read with
  `env[name]?.trim() || undefined` — i.e. surrounding whitespace is trimmed and
  an empty/whitespace-only value counts as **unset**.
- Default: `/var/lib/mousehole` (`DEFAULT_STATE_DIR` in `config.ts`). This is
  the documented Docker volume mount point in every compose example and the
  Unraid template. (History: `/srv/mousehole` was the pre-v0.3.0 default; the
  legacy *fallback* was removed in v0.5.0 — the Rust rewrite must NOT probe
  `/srv/mousehole`.)
- File inside the dir: `state.json` (`path.join(stateDirPath, "state.json")`).
- Temp file used for atomic writes: `state.json.tmp` (same directory, literal
  `${statePath}.tmp` suffix).
- Dev default (via Bun auto-loaded `.env.development`, NOT shipped in the
  image): `MOUSEHOLE_STATE_DIR_PATH=./.state`.

There is exactly one state file. Sessions, SSE registrations, and the contact
timer are in-memory only and are lost on restart (by design).

### 1.2 On-disk JSON schema (version 2, current)

`STATE_VERSION = 2`. The zod schema (`serializedStateSchema` in `serde.ts`):

```jsonc
{
  "version": 2,                    // REQUIRED, literal number 2
  "cookie": "…",                   // OPTIONAL string (the raw MAM mam_id value)
  "lastMamContact": { … }          // OPTIONAL, discriminated union on "reached"
}
```

`lastMamContact` — a discriminated union on the boolean `reached`:

Unreached variant:

```jsonc
{
  "at": "2026-08-18T14:05:00.123-05:00[America/Chicago]",  // string (RFC 9557)
  "reached": false,
  "error": { "type": "timeout-error", "message": "…" }      // both strings, both required
}
```

Reached variant:

```jsonc
{
  "at": "2026-08-18T14:05:00.123-05:00[America/Chicago]",
  "reached": true,
  "ip": "203.0.113.7",     // string — structure NOT validated here (no IPv4 check)
  "asn": 64496,             // number
  "as": "EXAMPLE-AS",       // string
  "ipUpdate": {             // OPTIONAL — present only when a cookie drove a
    "success": true,        //   dynamicSeedbox update
    "msg": "Completed",    // MAM's msg verbatim — display only, never branch on it
    "httpStatus": 200       // number
  }
}
```

Validation semantics that MUST be preserved:

- **Structure only, not semantics.** The disk schema deliberately does not
  validate that `ip` is an IPv4 address etc. — comment in `serde.ts`: "a
  stricter read schema than the write path can make states we already persisted
  unreadable." In Rust: plain `String`/`i64`/`bool` fields, no format checks.
- **Unknown keys are ignored** (zod object default strips unknown keys). In
  Rust: do **not** use `#[serde(deny_unknown_fields)]` anywhere in the state
  structs.
- `version` must be exactly the number `2` for the strict parse path (see
  migrations below for everything else).
- `cookie` may legitimately be an empty string on disk and still parse (only
  the *migration* cookie-rescue requires non-empty). Downstream, an
  empty-string cookie is treated as **absent** everywhere it's consumed
  (falsy checks): the contact loop takes the cookie-less `jsonIp` path and
  `PublicState.hasCookie` reports `false` — and the next *successful* contact
  rewrites the state without the `cookie` key (see `mam-behavior.md` §4).
- `at` is validated only as "a string" by the schema. It is parsed into a
  date *after* schema validation (see 1.4).

Example of a complete real file (2-space pretty-printed — see 1.3):

```json
{
  "version": 2,
  "cookie": "long-opaque-mam-session-cookie-value",
  "lastMamContact": {
    "at": "2026-08-18T02:00:05.123+00:00[UTC]",
    "reached": true,
    "ip": "203.0.113.7",
    "asn": 64496,
    "as": "EXAMPLE-AS",
    "ipUpdate": {
      "success": true,
      "msg": "Completed",
      "httpStatus": 200
    }
  }
}
```

A fresh install that has contacted MAM without a cookie yet looks like:

```json
{
  "version": 2,
  "lastMamContact": {
    "at": "2026-08-18T02:00:05.123+00:00[UTC]",
    "reached": true,
    "ip": "203.0.113.7",
    "asn": 64496,
    "as": "EXAMPLE-AS"
  }
}
```

(No `cookie` key at all — `JSON.stringify` drops `undefined` properties; same
for absent `lastMamContact` and absent `ipUpdate`.)

### 1.3 Serialization rules (write path)

`store.ts` writes: `JSON.stringify(serializeState(state), undefined, 2)`.

- **Pretty-printed with 2-space indent**, LF newlines, no trailing newline
  (JSON.stringify emits none).
- **Key order** (insertion order of the TS object literals — keep it in Rust by
  declaring struct fields in this order):
  - top level: `version`, `cookie`, `lastMamContact`
  - unreached contact: `at`, `reached`, `error` (with `type`, `message`)
  - reached contact: `at`, `reached`, `ip`, `asn`, `as`, `ipUpdate`
  - `ipUpdate`: `success`, `msg`, `httpStatus`
- **Absent optionals are omitted entirely** (not `null`). Rust:
  `#[serde(skip_serializing_if = "Option::is_none")]`.

Nothing downstream re-parses this file with order-sensitive tooling, but byte
compatibility keeps diffs/backup dedup/humans happy — match it.

### 1.4 Date format: RFC 9557 (`Temporal.ZonedDateTime`)

- In memory the TS code uses `Temporal.ZonedDateTime` (temporal-polyfill); on
  disk and on the wire it uses **RFC 9557 strings** — what
  `ZonedDateTime.toString()` emits and `ZonedDateTime.from()` round-trips.
- Timestamps are generated by `getNowZdt()` (`src/shared/time.ts`):
  `Temporal.Now.zonedDateTimeISO(Intl.DateTimeFormat().resolvedOptions().timeZone)`
  — i.e. the **host/container IANA timezone** (`TZ` env in Docker; compose
  examples set `TZ: Etc/UTC`). Unset TZ in the container yields `UTC`.
- Format shape: `YYYY-MM-DDTHH:MM:SS[.fff…]±HH:MM[IANA/Name]`, e.g.
  `2026-08-18T14:05:00.123-05:00[America/Chicago]` or
  `2026-08-18T02:00:05.123+00:00[UTC]`. Notes:
  - Fractional seconds use Temporal's `"auto"` precision: only as many digits
    as needed (in practice millisecond precision from the polyfill's clock;
    zero fraction is omitted entirely when the value is on a whole second).
  - The offset is always present, the bracketed IANA timezone annotation is
    always present, and there is no `[u-ca=…]` calendar suffix (ISO calendar
    is elided).
- Reads: `Temporal.ZonedDateTime.from(contact.at)` — accepts RFC 9557 with the
  bracket annotation (which every value this app ever wrote carries).
  **Subtlety:** this parse happens *after* zod validation; if the string is not
  a parseable date, `from()` throws a raw `RangeError` which surfaces as an
  HTTP 500 `{"type":"unhandled-error", …}` (not a `schema-error`). Rust may map
  this to the same 500 without matching the exact type string obsessively —
  it's an unreachable-in-practice corruption path — but must NOT treat it as a
  missing/fresh state.
- The value is passed through **verbatim** to the wire (`PublicState.
  lastMamContact.at` re-serializes with `.toString()`, which round-trips the
  same string). The frontend parses it with JS `Temporal`. So Rust must emit
  strings the frontend's Temporal polyfill can `from()` — RFC 9557 with
  bracketed IANA name. See §6 for the jiff recommendation (the currently
  scaffolded `time` crate **cannot** produce or parse the bracket annotation).

### 1.5 Read path and error precedence (`readIfExists`)

Exact order, from `store.ts`:

1. `fs.readFile(statePath, "utf8")`.
   - `ENOENT` → return `undefined` ("fresh install"). **Only** ENOENT gets
     this treatment.
   - Any other error (EACCES, EISDIR, IO) → throw `FileReadError` →
     HTTP 500, error body `type: "file-read-error"`, message
     `` `Error reading file: ${path}. Check that it is readable and is not a directory.` ``
     The rationale (comment in source): treating unreadable as "no state"
     would let the next contact write a cookieless state over the real one.
2. `JSON.parse(contents)`. Failure → `JSONParseError.fromFile` → 500,
   `type: "json-parse-error"`, message
   `` `Error parsing JSON from file at ${path}` `` (with the parse error as
   `cause` in the response body).
3. `migrateToCurrent(json, statePath)` (see 1.6). Schema failure → 500,
   `type: "schema-error"`, message
   `` `Schema validation failed for data from ${sourceName}: ${firstIssueSummary}` ``
   plus a structured `issues: [{path, message}]` array in the body. The
   `sourceName` is the state file **path** (e.g.
   `/var/lib/mousehole/state.json`).
4. `deserializeState(...)` — converts `at` strings to ZonedDateTime (can throw,
   see 1.4) and returns the in-memory `State`
   (`{ cookie?: string, lastMamContact?: MamContact }` — no `version` field in
   memory).

**Corrupt state is an error, never silently reset.** There is no
quarantine/rename/backup of a bad file; every `GET /state`, `GET /health`, and
contact attempt will keep surfacing the 500 until a human fixes the file. The
background loop logs the error (`logger.error`) and does not crash the process.

### 1.6 Migration system (`migrate.ts`)

- Current version: `2`. There are no per-version step migrations; migration is
  a single **lossy collapse**:

  1. `isCurrentVersion(json)`: json is an object, non-null, and
     `json.version === 2` (strict numeric equality). If yes → the object goes
     straight to strict schema validation.
  2. Otherwise (any other version, no version, arrays, scalars, null…):
     build the candidate `{ version: 2, cookie: findCookie(json) }`.
     - `findCookie`: if json is a non-null object, check keys in order —
       currently the single legacy key `"currentCookie"` (the v1 field name;
       v2 renamed it to `cookie`) — and return the value if it is a string
       with `length > 0`; else `undefined`.
     - Everything else from the legacy state (last-contact data etc.) is
       **deliberately dropped** — "only the cookie survives across versions
       (lastMam is cheap to regenerate on the next check)".
  3. `serializedStateSchema.safeParse(candidate)`; on failure throw
     `SchemaError.fromExternalSource(sourceName)` (500). Note the constructed
     legacy candidate always passes (it is `{version: 2}` or
     `{version: 2, cookie: "…"}`), so in practice schema errors only occur for
     files that *claim* `version: 2` but have malformed contents.

- Migration happens **on read, in memory** — the migrated form is not written
  back until the next state write (the next contact). A legacy file therefore
  stays on disk in old form until the first contact after upgrade.
- Rust must replicate exactly: check `version == 2` → strict-parse; else
  rescue only a non-empty string `currentCookie`; validate; same error type.

### 1.7 Write path and atomicity (`write`)

Exact order:

1. Serialize (1.3).
2. `fs.mkdir(stateDirectoryPath, { recursive: true })` — every write, not just
   the first. Failure → `DirectoryCreateError` (500,
   `type: "directory-create-error"`, message
   `` `Error creating directory: ${path}. Check that the parent directory exists and you have write permissions.` ``).
3. Write to `state.json.tmp`, then `fs.rename` over `state.json` (atomic
   replace on POSIX). Failure of either step → `FileWriteError` (500,
   `type: "file-write-error"`, message
   `` `Error writing file: ${path}. Check that the parent directory exists and is writable.` `` —
   note the error names the **final** path, not the tmp path).
4. No fsync of file or directory (best-effort atomicity; comment calls it
   "millisecond-level race condition protection"). Rust may add fsync as a
   strict improvement, but tmp-then-rename with the same filenames is the
   required baseline.

### 1.8 Concurrency model

- The store itself has **no locking**. Serialization of read-modify-write is
  provided one level up: `contact.ts`'s `commitContact` wraps
  `readIfExists → contactMam → write → notifyClients` in a process-wide async
  `Mutex`, and *every* mutation goes through `commitContact` (startup contact,
  interval timer, `POST /updates`, `PUT /cookie` — the last passes the new
  cookie so replacement happens inside the same locked section, *before*
  contacting MAM).
- `GET /state` and `GET /health` read **without** the mutex (a torn read is
  impossible thanks to atomic rename; a slightly stale read is fine).
- Rust equivalent: `tokio::sync::Mutex<()>` (or a mutex owning the scheduler
  state) around the commit path; unguarded reads elsewhere.

---

## 2. Frontend build (reused verbatim by the Rust backend)

### 2.1 Build command and Vite config

- Command: `bun run build` → runs **`vite build`** (nothing else — no tsc, no
  extra steps).
- `vite.config.ts` facts:
  - `root: src/frontend` (the app's `index.html` lives there).
  - **`base: "/web/"`** — every built asset URL is absolute under `/web/`.
    The backend owns everything outside that prefix.
  - `envDir`: repo root.
  - `build.outDir`: **`<repo>/dist`** (absolute, outside the Vite root), with
    `emptyOutDir: true`.
  - Plugins: `@vitejs/plugin-react`, `@tailwindcss/vite`.
  - `define`: inlines `process.env.PUBLIC_GIT_HASH` as a string literal into
    the bundle. Resolution order (`resolveGitHash()`):
    1. `process.env.PUBLIC_GIT_HASH` (Docker build arg path),
    2. `git rev-parse --short HEAD` (local builds),
    3. `""` (the footer hides an absent hash).
  - Dev server: port `5173` strict, `hmr.clientPort: 5173` — dev-only,
    irrelevant to the Rust prod backend except for the proxy mount (2.3).

### 2.2 Output layout

`dist/` after `vite build`:

- `dist/index.html` — references assets by absolute `/web/assets/…` URLs.
- `dist/assets/*` — content-hashed JS/CSS/fonts (Commissioner + IBM Plex Mono
  woff2s), SVG logo, etc.

The bundle is completely static — no server-side templating, no runtime env
injection. The git hash and all URLs are baked at build time.

### 2.3 How the backend serves the bundle (`app.ts`)

Mount selection (`server.ts`): `NODE_ENV === "production"` →
`{ mode: "serve-static", root: "./dist" }`; otherwise a dev reverse-proxy to
`http://localhost:5173`. Note `"./dist"` is **CWD-relative** — in the Docker
image, CWD is `/usr/src/app` and `dist/` sits next to `src/`.

Prod serving rules (must be replicated exactly):

- `GET /web/*` → static file from `dist/`, with the leading `/web` stripped
  (`requestPath.replace(/^\/web/, "")`). `GET /web/` (trailing slash) thus
  resolves to the directory root and serves `dist/index.html`.
- `GET /web` (exact, no slash) → serves `dist/index.html` directly.
- **Not auth-protected** — "the login page itself needs these assets." No
  Host/Origin checks either on these routes.
- A miss (no such file) falls through to the app-level 404:
  `{"type":"not-found","message":"Not Found"}` with status 404,
  `Content-Type: application/json`. There is **no SPA history fallback** — the
  app is a single page at `/web`; unknown `/web/foo` paths 404.
- No cache-control headers are set by the current implementation (hono
  `serveStatic` defaults). Content-Type comes from the file extension.
- `GET /` (exact) → content-negotiated redirect (302): `Accept` prefers
  `text/html` (default) → `Location: /web`; prefers `application/json` →
  `Location: /health`.

The only routes are: `/`, `/login`, `/logout`, `/updates`, `/state`,
`/cookie`, `/health`, `/events`, and the `/web` mounts — everything else 404s
with the JSON body above (see `api-contract.md`).

### 2.4 Git hash — the two consumers

- **Frontend**: inlined at build time (2.1). Nothing to do at runtime.
- **Backend**: `src/shared/git-hash.ts` reads `process.env.PUBLIC_GIT_HASH` at
  **runtime**; its only backend use is the startup log line:
  `` `Mousehole v${version} (${gitHash}) running at ${server.url}` `` where
  `version` comes from `package.json` (currently `0.5.0`). Rust: read
  `PUBLIC_GIT_HASH` from the environment at startup (parity), or bake via
  `option_env!` — the Docker image sets it as a runtime ENV either way.

---

## 3. Current Dockerfile and runtime layout

Image: `tmmrtn/mousehole` (Docker Hub), platforms `linux/amd64, linux/arm64`,
built by `.github/workflows/docker-push.yaml` with
`build-args: GIT_HASH=<short sha>`.

Stages (`FROM oven/bun:1-alpine AS base`, plus
`apk add --no-cache ca-certificates curl`):

| Stage | Workdir | What it does |
|---|---|---|
| `base` | — | bun alpine + ca-certificates + curl |
| `runtime-deps` | `/temp/install` | `COPY package.json bun.lock` → `bun install --frozen-lockfile --production` (backend runtime deps only: hono, set-cookie-parser, temporal-polyfill, zod) |
| `build-web` | `/temp/build` | full `bun install --frozen-lockfile`; `ARG GIT_HASH` → `ENV PUBLIC_GIT_HASH`; `COPY vite.config.ts`, `COPY src`; `bun run build` → `/temp/build/dist` |
| `release` | `/usr/src/app` | see below |

`release` stage details:

- `EXPOSE 5010/tcp`
- `ENV NODE_ENV=production`; `ARG GIT_HASH` → `ENV PUBLIC_GIT_HASH=${GIT_HASH}`
- Copies: `node_modules` (from runtime-deps), `package.json`, `src`, and
  `dist` (from build-web) — the backend runs **from TypeScript source** under
  Bun, no backend build step.
- `HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3`
  running `bun -e "process.exit((await fetch('http://localhost:5010/health')).ok ? 0 : 1)"`
  — note it **hardcodes port 5010** regardless of `MOUSEHOLE_PORT`, and
  `/health` always returns 200 while the server is up (sync trouble does not
  fail the healthcheck — deliberate, per CHANGELOG v0.5.0).
- `CMD ["bun", "run", "src/index.ts"]`
- No `USER` directive (runs as the base image default), no `VOLUME`
  directive (compose files mount `mousehole:/var/lib/mousehole` explicitly).

`.dockerignore` (what never reaches the build context): `node_modules`,
`Dockerfile*`, `.dockerignore`, `.git`, `.gitignore`, `docs`, `contrib`,
`CONTRIBUTING.md`, `components.json`, `eslint.config.mjs`, `CHANGELOG.md`,
`README.md`, `LICENSE`, `.vscode`, `Makefile`, `.state`, `.env`, `.env.local`,
`dist`, `.fallow`. (Note `dist` is ignored — the image's dist always comes
from the `build-web` stage, never a stale local build. `.env.development` is
*not* ignored but is also never COPY'd.)

Env-file note: Bun auto-loads `.env` / `.env.development` / `.env.local` from
CWD in dev, which is how `.env.development` (60s interval, `./.state`,
password `password`) takes effect locally. The shipped image contains **no**
`.env` files, so container behavior comes purely from real environment
variables. The Rust binary should not implement `.env` loading for prod parity
(optionally gate `dotenvy` behind a dev-only flag for contributor ergonomics).

---

## 4. Startup and shutdown sequence

`src/index.ts` is "the only module with import-time side effects": it calls
`startServer()` and wires signals. Exact order inside `startServer`
(`server.ts`):

1. `buildConfig(process.env)` — all env parsing happens here, fail-fast:
   invalid values throw `` `Invalid environment variable ${name}="${raw}": ${message}` ``
   before anything binds. This includes reading `*_FILE` secret files
   (`MOUSEHOLE_AUTH_PASSWORD_FILE` / `MOUSEHOLE_AUTH_TOKEN_FILE`, trimmed,
   unreadable file → startup failure) and the
   `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH` vs credential mutual-exclusion error.
   (Full env spec: `config-auth-boundary.md`.)
2. `setLogLevel(config.logLevel)` (`MOUSEHOLE_LOG_LEVEL`: `error`|`warn`|
   `info`(default)|`debug`; all logs go to **stdout**; ANSI color only when
   stdout is a TTY and `NO_COLOR` is unset; prefix format `[LEVEL] message`).
3. `createAppContext(config)` — constructs `StateFileStore(config.stateDirPath)`
   (note: **no state read and no directory creation happens at startup**; the
   first disk touch is the first contact/read), the SSE registry, the session
   store, and the contact scheduler (not yet started).
4. Web mount chosen by `NODE_ENV` (2.3); `createApp` assembles routes.
5. `Bun.serve({ port, idleTimeout: 0, fetch })` — **idle timeout disabled**
   because a quiet SSE stream counts as idle. The Rust server must likewise
   never idle-close `/events` connections.
6. Log: `Mousehole v{version} ({gitHash}) running at {url}`.
7. `validateRuntimeSecurityConfig` — *after* the listener is bound:
   - warn if `MOUSEHOLE_ALLOWED_HOSTS` is `*`; warn if
     `MOUSEHOLE_ALLOWED_ORIGINS` is `*`;
   - warn "Browser login will be unavailable…" when a token but no password is
     configured;
   - **throw** (crashing the process) when no credential is configured and
     `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH` is not `true`:
     "Mousehole authentication is not configured. Set MOUSEHOLE_AUTH_PASSWORD
     and/or MOUSEHOLE_AUTH_TOKEN, or set MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true
     to opt out." (The TS version briefly binds the port before dying; Rust
     may validate before binding — the observable contract is "refuses to run,
     non-zero exit".)
   - warn "Running without authentication…" when the opt-out is set.
8. `ctx.contacts.start()` — fires an **immediate** contact (async,
   fire-and-forget; unexpected errors are logged, never crash), then each
   contact reschedules the next via a `setTimeout` armed in the commit's
   `finally` (interval `MOUSEHOLE_UPDATE_INTERVAL_SECONDS`, default 300; the
   timer is `unref`'d so it alone doesn't hold the process). Logs
   "Background update task started, running on N second interval" and, after
   each schedule, "Next automatic update scheduled for <RFC 9557>".
9. Signals: `SIGINT` and `SIGTERM` → `stop()` → log "Shutting down...", then
   `contacts.stop()` (set stopped flag, cancel pending timer, acquire the
   commit mutex to **drain any in-flight contact**), then
   `server.stop(true)` (force-close open connections, including SSE), then
   `process.exit(0)`.

---

## 5. Rust packaging plan

A scaffold already exists at `rust/` (`rust/Cargo.toml`, `rust/src/main.rs`
with a module map that references these spec files). The plan below aligns
with it and fills in the packaging details.

### 5.1 Cargo project layout

```
rust/
  Cargo.toml            # package "mousehole"; keep [package].version in lock-step
                        # with package.json (the startup log prints it)
  Cargo.lock            # commit it (binary crate)
  src/
    main.rs             # startup sequence of §4, tokio main, signal handling
    config.rs           # env parsing incl. _FILE variants (config-auth-boundary.md)
    boundary.rs         # Host/Origin middleware
    session.rs          # login sessions + bearer auth
    api.rs              # routes + error contract (api-contract.md)
    sse.rs              # /events
    mam.rs              # MAM + host-info clients (mam-behavior.md)
    scheduler.rs        # contact loop + commit mutex (§1.8, §4.8)
    state/
      mod.rs            # State/MamContact domain types
      serde.rs          # on-disk structs, (de)serialization, PublicState conversion
      migrate.rs        # §1.6
      store.rs          # §1.5/§1.7 (tokio::fs, tmp+rename)
    assets.rs           # rust-embed of ../dist (§5.3)
```

### 5.2 State layer in Rust (axum/tokio/serde idioms)

- **Structs mirror §1.2 field-for-field, declared in the §1.3 key order**:

  ```rust
  #[derive(Serialize, Deserialize)]
  struct SerializedState {
      version: u32, // validate == 2 after parse (or a custom de that rejects != 2)
      #[serde(skip_serializing_if = "Option::is_none")]
      cookie: Option<String>,
      #[serde(skip_serializing_if = "Option::is_none")]
      #[serde(rename = "lastMamContact")]
      last_mam_contact: Option<SerializedMamContact>,
  }
  ```

  All wire names are camelCase — either `#[serde(rename_all = "camelCase")]`
  per struct or explicit renames (`httpStatus`, `ipUpdate`, `lastMamContact`).
- **The `reached` union**: serde has no discriminated-union-on-bool. Two solid
  options: (a) an untagged enum whose variants pin the literal with the
  `monostate` crate (`reached: MustBe!(true)` / `MustBe!(false)`) — closest to
  zod's discriminatedUnion, order-independent; (b) manual `Deserialize` that
  branches on `reached`. Avoid a plain untagged enum without literal pinning:
  it would classify by which *other* fields happen to be present, changing
  error messages and edge behavior. Remember: extra unknown fields must stay
  ignored.
- **Pretty output**: `serde_json::to_string_pretty` uses 2-space indent —
  byte-compatible with `JSON.stringify(…, undefined, 2)` for these shapes.
  Write `contents` as UTF-8, no trailing newline.
- **Dates — use `jiff`, not `time`**: the scaffolded `time = "0.3"` dependency
  **cannot parse or emit the RFC 9557 bracketed-IANA form**
  (`…-05:00[America/Chicago]`) that every existing state file and the wire
  contract carry. `jiff`'s `Zoned` type is a purpose-built RFC 9557
  implementation: `Zoned::now()` (honors `TZ`), `zoned.to_string()` emits
  `2026-08-18T14:05:00.123-05:00[America/Chicago]`, and
  `str::parse::<Zoned>()` round-trips it. Swap the dependency. Two jiff notes:
  enable the bundled tzdb feature if the runtime image lacks
  `/usr/share/zoneinfo` (scratch/distroless do — see 5.4), and keep the
  on-disk field as the parsed `Zoned` in memory but a plain `String` in the
  serialized struct if you want to preserve unusual-but-valid inputs verbatim.
- **Store** (`store.rs`): `tokio::fs::read_to_string` → match
  `e.kind() == ErrorKind::NotFound` → `Ok(None)`; else `FileReadError`. Parse
  with `serde_json::from_str::<serde_json::Value>` first (so JSON-vs-schema
  errors stay distinct, matching §1.5's json-parse-error / schema-error
  split), then migrate (§1.6) on the `Value`, then deserialize to the struct.
  Write: `tokio::fs::create_dir_all(dir)` (→ DirectoryCreateError) →
  `tokio::fs::write(format!("{}.tmp", path), contents)` →
  `tokio::fs::rename(tmp, path)` (both → FileWriteError naming the final
  path).
- **Errors**: a `thiserror` enum with variants carrying
  `(http_status, error_type, message, cause)` matching §1.5's strings; convert
  to the JSON error body via the shared error-response type
  (`api-contract.md`). `IntoResponse` on the error type keeps handlers clean.
- **Commit mutex**: `tokio::sync::Mutex<()>` held across
  read→contact→write→notify; `stop()` acquires it and never releases
  (or use a `CancellationToken` + acquiring the guard) to drain in-flight
  work, mirroring §4.9.
- **Scheduler**: a `tokio::task` with `tokio::time::sleep` re-armed after each
  commit (not `interval` — the TS behavior is "next contact = interval after
  the *previous commit finished*", because `scheduleNext` runs in `finally`
  after the contact completes, and manual `POST /updates` / `PUT /cookie`
  commits also reset the timer). Track `next_contact_at: Option<Zoned>` for
  `GET /state`'s `nextContactAt`.

### 5.3 Embedding the frontend (`rust-embed`)

```rust
#[derive(rust_embed::RustEmbed)]
#[folder = "../dist/"]      // relative to rust/Cargo.toml
struct WebAssets;
```

- Build ordering: `vite build` must run before `cargo build --release`
  (rust-embed embeds at compile time in release; a missing folder is a compile
  error). In Docker the web stage output is copied in before the cargo build.
  For local dev, rust-embed's debug mode reads from disk at runtime, so
  `bun run build && cargo run` just works.
- axum wiring, matching §2.3 exactly:
  - `GET /web` and `GET /web/` → serve embedded `index.html`.
  - `GET /web/{*path}` → look up `path` in `WebAssets`;
    `Content-Type` via `mime_guess::from_path`; miss → the shared JSON 404
    (`{"type":"not-found","message":"Not Found"}`).
  - No auth/host/origin middleware on these routes; no cache headers (adding
    `Cache-Control: immutable` for `/web/assets/*` is a safe improvement —
    hashed filenames — but is a deviation; default to parity).
  - `GET /` → 302 with `Location: /web` unless the `Accept` header prefers
    `application/json` over `text/html`, then `Location: /health`.
- rust-embed's `compression` feature is fine (transparent decompress on
  lookup); serving pre-compressed bodies would change response headers — skip.

### 5.4 Multi-stage Dockerfile (proposed)

```dockerfile
# ── Stage 1: web bundle (unchanged toolchain) ────────────────────────────────
FROM oven/bun:1-alpine AS build-web
WORKDIR /build
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile
ARG GIT_HASH
ENV PUBLIC_GIT_HASH=${GIT_HASH}
COPY vite.config.ts ./
COPY src ./src
RUN bun run build                      # → /build/dist

# ── Stage 2: rust binary ─────────────────────────────────────────────────────
FROM rust:1-alpine AS build-rust
RUN apk add --no-cache musl-dev
WORKDIR /build
# dependency-layer caching
COPY rust/Cargo.toml rust/Cargo.lock rust/
RUN mkdir -p rust/src && echo 'fn main(){}' > rust/src/main.rs \
 && cargo build --release --manifest-path rust/Cargo.toml \
 && rm -rf rust/src
COPY rust/src rust/src
COPY --from=build-web /build/dist ./dist   # rust-embed folder = ../dist
RUN touch rust/src/main.rs \
 && cargo build --release --manifest-path rust/Cargo.toml

# ── Stage 3: runtime ─────────────────────────────────────────────────────────
FROM scratch AS release
COPY --from=build-rust /build/rust/target/release/mousehole /mousehole
EXPOSE 5010/tcp
ARG GIT_HASH
ENV PUBLIC_GIT_HASH=${GIT_HASH}
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD ["/mousehole", "healthcheck"]
ENTRYPOINT ["/mousehole"]
```

Runtime-stage decisions, and why:

- **TLS roots**: keep reqwest on `rustls-tls` (= webpki bundled roots) so the
  image needs no `/etc/ssl/certs` — this is why `scratch` works. If you switch
  to `rustls-tls-native-roots`, use `gcr.io/distroless/static` and copy
  `ca-certificates` instead.
- **tzdata**: `scratch` has no `/usr/share/zoneinfo`; compose examples set
  `TZ` and the state's `at` strings embed IANA names, so build jiff with its
  bundled tzdb (`jiff` feature `tzdb-bundle-always`) or copy `tzdata` from the
  build stage into `/usr/share/zoneinfo`.
- **HEALTHCHECK**: scratch has no shell or bun, so bake a `healthcheck`
  subcommand into the binary that GETs `http://localhost:5010/health` (or
  better, honor `MOUSEHOLE_PORT` — the current image hardcodes 5010; honoring
  the var is a strict improvement) and exits 0/1. Keep the exact
  interval/timeout/start-period/retries values.
- **User/permissions**: the current image has no `USER` directive and the
  default state dir `/var/lib/mousehole` is created on first write. To stay
  drop-in with existing named volumes (whose files may be root-owned), run as
  root like today. If you want distroless `:nonroot`, you must also `chown`
  a pre-created `/var/lib/mousehole` and accept a migration wrinkle for
  existing root-owned volumes — don't do this in the drop-in release.
- **Static linking**: `rust:1-alpine` + `musl-dev` gives a fully static musl
  binary on both `linux/amd64` and `linux/arm64` (CI builds per-platform via
  buildx/QEMU exactly as today; no cross-compilation changes needed in
  `.github/workflows/docker-push.yaml` beyond the Dockerfile itself — it
  already passes `GIT_HASH` and both platforms).
- Keep `EXPOSE 5010`, same tag scheme, same build-arg name (`GIT_HASH`).
- `.dockerignore`: add `rust/target` to the existing list.

### 5.5 Drop-in compatibility checklist

The container must be replaceable in every documented compose file
(`docs/docker-compose-examples/*`, `contrib/unraid/my-mousehole.xml`, the
forum post) with only the image name changing:

| Contract | Value |
|---|---|
| Listen port | `5010` default, `MOUSEHOLE_PORT` override; binds all interfaces |
| State volume | `/var/lib/mousehole` default dir, `state.json` inside, `MOUSEHOLE_STATE_DIR_PATH` override; reads existing v2 files byte-for-byte and rescues v1 `currentCookie` |
| Env vars | `MOUSEHOLE_PORT`, `MOUSEHOLE_LOG_LEVEL`, `MOUSEHOLE_STATE_DIR_PATH`, `MOUSEHOLE_UPDATE_INTERVAL_SECONDS`, `MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS`, `MOUSEHOLE_SESSION_DURATION_SECONDS`, `MOUSEHOLE_HTTPS_ONLY_COOKIES`, `MOUSEHOLE_AUTH_PASSWORD`(+`_FILE`), `MOUSEHOLE_AUTH_TOKEN`(+`_FILE`), `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH`, `MOUSEHOLE_ALLOWED_HOSTS`, `MOUSEHOLE_ALLOWED_ORIGINS` — identical names, defaults, trimming, `*` sentinels, and error messages (see `config-auth-boundary.md`) |
| `_FILE` variants | password + token only; file contents trimmed; `_FILE` wins over plain; unreadable file = refuse to start |
| `TZ` | honored for the timezone embedded in `at` timestamps |
| `PUBLIC_GIT_HASH` | runtime env consumed for the startup log; set from the `GIT_HASH` build arg |
| Healthcheck | `GET /health` on 5010, 200 = healthy, same timings |
| Logs | stdout only, `[LEVEL]` prefixes, `NO_COLOR` respected |
| Signals | SIGTERM/SIGINT → drain in-flight MAM contact → exit 0 |
| No `.env` loading in the container | behavior driven by real env only |
