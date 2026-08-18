# Rust Rewrite — Build Order Overview

The entry point for implementing the Rust backend. Read this first, then the
four companion specs, which are **normative** (the React frontend is reused
verbatim as the built Vite bundle, so every wire detail must match exactly):

| Spec | Covers |
|---|---|
| `api-contract.md` | Every route, status code, header, JSON shape, boundary precedence, SSE, static serving, error catalog |
| `config-auth-boundary.md` | Env var matrix (+`_FILE` secrets), auth ladder, sessions/cookies, host/origin/json/body-limit checks, logging |
| `mam-behavior.md` | dynamicSeedbox + jsonIp clients, cookie rotation, contact loop, scheduler, mutex, outcome classification |
| `state-and-build.md` | `state.json` schema/serde/migration/atomicity, frontend build + `/web` serving, Docker packaging, startup/shutdown |

Ground rules: **drop-in replacement** — same image contract (port 5010,
`/var/lib/mousehole` volume, same env vars, same healthcheck semantics), same
wire bytes, no `.env` loading in the container, no new required configuration.

---

## 1. Crate layout (the `rust/` scaffold)

A scaffold exists at `rust/` (`Cargo.toml` + stub modules). Its module map is
the intended layout:

| Module | Implements | Spec sections |
|---|---|---|
| `config.rs` | `Config::from_env`, `_FILE` secrets, auth/hosts/origins enums, exact error strings | `config-auth-boundary.md` §1 |
| `boundary.rs` | host / auth / origin / json-content-type checks as axum middleware; `AuthMethod` via request extensions; body-limit JSON 413 | `config-auth-boundary.md` §4, `api-contract.md` §5 |
| `session.rs` | session store (32-byte base64url ids, fixed expiry, lazy prune + expiry reaper firing SSE close), `safeEqual`, Set-Cookie building/parsing | `config-auth-boundary.md` §2 |
| `api.rs` | router assembly, `GET /` negotiation, login/logout, state/updates/cookie/health handlers, error envelope + JSON 404 (and 405→404), `PublicState` | `api-contract.md` §1–§6, §9 |
| `sse.rs` | `/events` registry, `data: changed\n\n` broadcast, per-session close | `api-contract.md` §7 |
| `mam.rs` | reqwest clients for dynamicSeedbox + jsonIp, error mapping, `mam_id` rotation (percent-decoded) | `mam-behavior.md` §1–§3 |
| `scheduler.rs` | `commit_contact` (tokio mutex, read→contact→write→notify→reschedule-in-finally), fixed-interval timer, `next_contact_at`, start/stop-with-drain | `mam-behavior.md` §4–§6 |
| `state.rs` (or a `state/` dir: `serde` / `migrate` / `store`) | on-disk structs (key order!), v-not-2 → `currentCookie` rescue, ENOENT-only fresh-install, tmp+rename writes | `state-and-build.md` §1 |
| `assets.rs` | `rust-embed` of `../dist`, `/web` + `/web/*` serving, JSON 404 on miss | `state-and-build.md` §2.3, §5.3 |
| `main.rs` | startup sequence, security validation, signals, ordered shutdown, `healthcheck` subcommand | `state-and-build.md` §4, §5.4 |

### Scaffold corrections needed before real work

The stubs are placeholders, not decisions. Known deviations from the specs:

1. **Swap `time = "0.3"` for `jiff`** — `time` cannot parse/emit the RFC 9557
   bracketed-IANA form (`…-05:00[America/Chicago]`) that every timestamp in
   the contract carries (`state-and-build.md` §1.4, §5.2). Enable a bundled
   tzdb feature for scratch images.
2. **reqwest features**: drop `cookies` (cookie handling is manual and must
   stay manual — `mam-behavior.md` §3; never enable a cookie store). `json` is
   unnecessary (bodies are read as text first to keep `json-parse-error`
   distinct). Add `percent-encoding` (Set-Cookie value decode), `url`
   (host/origin normalization), `cookie` (Set-Cookie parse/build), `base64`,
   `regex` (Bearer parse) as needed.
3. **`MOUSEHOLE_HOST` in the stub `config.rs` is an invention** — the TS
   backend has no bind-address config (Bun binds all interfaces). Either
   delete it or keep it as a purely additive extension defaulting to
   `0.0.0.0`; it must never be required.
4. **`main.rs` only handles Ctrl-C** — production needs SIGTERM too
   (`tokio::signal::unix`), and the ordered shutdown: stop scheduler (cancel
   timer, drain in-flight contact via the mutex) → stop server → exit 0.
5. **`tracing_subscriber::fmt()` does not match the log contract** —
   output must be `[LEVEL] message` on **stdout**, threshold from
   `MOUSEHOLE_LOG_LEVEL`, ANSI only when TTY && `NO_COLOR` absent
   (`config-auth-boundary.md` §5). Hand-roll the logger or write a custom
   `FormatEvent`.
6. The stub binds before building the real config; the required order is:
   config (fail fast) → log level → context → bind → banner → security
   validation (may abort) → scheduler start (`state-and-build.md` §4).

---

## 2. Suggested implementation order

Each step is testable in isolation before the next; port the matching TS test
file (in `tests/`) as you go — they pin the subtle behaviors.

1. **`config.rs`** — pure function of an env map + injected file reader.
   Port `tests/config.test.ts`. Exact error strings matter (user-facing).
2. **Error model** — the `MouseholeError` enum, `{type,message,issues?,cause?}`
   envelope, `IntoResponse`. Port `tests/error.test.ts`.
3. **Time** — `jiff::Zoned` now/parse/format helpers; smoke-test that output
   round-trips `Temporal.ZonedDateTime.from` (run the polyfill via `bun -e`).
4. **`state.rs`** — serialized structs (declare fields in the on-disk key
   order), migration, store with ENOENT-only-fresh and tmp+rename. Port
   `tests/serde.test.ts`, `tests/migrate.test.ts`, `tests/store.test.ts`.
   Byte-compare output against a real `state.json` from the Bun instance.
5. **`classify` + `PublicState`** — single classification function, key
   omission via `skip_serializing_if`. Port `tests/public-state.test.ts`.
6. **`mam.rs`** — clients against a fake MAM server (port
   `tests/lib/mam-test-server.ts`: it encodes the full outcome matrix,
   including 429/403-with-body, cookie rotation, timeout, non-JSON).
7. **`scheduler.rs`** — `commit_contact` + timer. Verify: reschedule happens
   even on write failure; manual contact resets the countdown; `stop()`
   drains; `next_contact_at` fresh before the HTTP response is built.
8. **`session.rs` + `boundary.rs`** — port `tests/http-boundary.test.ts` and
   `tests/login.test.ts` (cookie attribute assertions, auth-ladder order,
   token-skips-origin, host parsing incl. `:80`-stripping).
9. **`api.rs`** — assemble the router; port `tests/app.test.ts` (the
   route-protection matrix is the single most valuable parity suite). Include
   the 405→404 mapping and the global JSON-bodied 8 KiB limit.
10. **`sse.rs`** — no hello frame, no keep-alive pings, `data: changed\n\n`
    on every persisted contact, close-on-logout/expiry, no idle timeout.
11. **`assets.rs`** — embed `../dist`; `/web`, `/web/`, `/web/*`, JSON 404 on
    miss, no cache headers, no SPA fallback.
12. **`main.rs`** — startup/shutdown sequence + `healthcheck` subcommand.
13. **Dockerfile** — per `state-and-build.md` §5.4; keep `GIT_HASH` build arg,
    `EXPOSE 5010`, healthcheck timings.

---

## 3. Parity-test checklist (against the running Bun instance)

Run the Bun backend (`bun run src/index.ts` with `NODE_ENV=production`, a
built `dist/`, and e.g. `MOUSEHOLE_AUTH_PASSWORD=pw`) and the Rust binary
side-by-side; every probe below must answer identically (modulo `Date`-like
headers). Automate as a diff harness where possible.

**Boot & config**
- [ ] No credentials, no opt-out ⇒ refuses to start, non-zero exit, exact message.
- [ ] `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true` + password ⇒ startup error (mutual exclusion string).
- [ ] `MOUSEHOLE_AUTH_PASSWORD_FILE` unreadable ⇒ startup error; empty file ⇒ no credential (even with plain var set).
- [ ] Invalid numeric/flag values ⇒ `Invalid environment variable NAME="raw": …`.
- [ ] Warnings for `*` hosts, `*` origins, token-only auth, no-auth opt-out.

**Routing & content negotiation**
- [ ] `GET /` with no/`text/html` Accept ⇒ 302 `Location: /web`; `Accept: application/json` ⇒ 302 `/health`.
- [ ] Unknown path ⇒ 404 `{"type":"not-found","message":"Not Found"}`.
- [ ] `POST /state` (wrong method) ⇒ **404** JSON body, not 405.
- [ ] `HEAD /health` ⇒ 200, empty body.

**Auth**
- [ ] `GET /state` unauthenticated ⇒ 401 + `WWW-Authenticate: Bearer realm="Mousehole"`, message `No credentials presented`.
- [ ] With bogus `Authorization: Basic x` ⇒ 401 `Rejected Bearer token (…)`; with stale session cookie ⇒ `Unknown or expired session cookie`.
- [ ] `Authorization: bearer <token>` (lowercase scheme) works; token with internal spaces byte-matches; trailing space fails.
- [ ] Login: no `Content-Type` ⇒ 415; `{}` ⇒ 400 `Request body must match expected schema`; bad JSON ⇒ 400 `Malformed JSON in request body`; wrong pw ⇒ 401 `Incorrect password`; token-only config ⇒ 500 `Browser login is unavailable: …`. All in the `{ok:false,message}` envelope.
- [ ] Login success ⇒ `{"ok":true}` + `Set-Cookie: mousehole-session=<43 chars>; Max-Age=…; Path=/; HttpOnly; SameSite=Lax` (+`Secure` iff `MOUSEHOLE_HTTPS_ONLY_COOKIES=true`); the cookie then authenticates `GET /state`.
- [ ] Logout with unknown/no cookie ⇒ still 200 `{"ok":true}` + clearing cookie `mousehole-session=; Max-Age=0; Path=/`.
- [ ] Session expiry (short `MOUSEHOLE_SESSION_DURATION_SECONDS`) ⇒ later request 401s **and** its open `/events` stream is closed without any request.

**Boundary precedence**
- [ ] Oversized (>8192B) `PUT /cookie` with **no credentials** ⇒ 413 `payload-too-large` (limit precedes auth).
- [ ] Evil `Host: evil.example` ⇒ 403 `host-not-allowed`, message contains the raw value + `MOUSEHOLE_ALLOWED_HOSTS`.
- [ ] Default host rules match any port (`localhost:9999` passes); rule `myhost:8080` requires that port.
- [ ] Session-authed `POST /updates` with `Origin: https://evil.example` ⇒ 403 `origin-not-allowed` (normalized origin in message); same request with Bearer token ⇒ 200.
- [ ] No `Origin` header ⇒ passes; `Origin: null` ⇒ 403 in same-origin mode, passes when `null` is allowlisted.
- [ ] `GET /state` has **no** origin check (cross-origin GET with session still 200).

**State & contacts** (use a fake MAM — port `tests/lib/mam-test-server.ts`)
- [ ] `PUT /cookie` bad JSON ⇒ 400 `json-parse-error`; `{"value":""}` ⇒ 400 `schema-error` with `issues[0].path === "value"`; empty body ⇒ 400 `schema-error` with **no path prefix** in the message.
- [ ] `PUT /cookie` with MAM down ⇒ **200**, `reached:false` in the returned state, cookie persisted anyway.
- [ ] MAM 200/429/403 ⇒ `/health` reports `ok`/`throttled`/`rejected`; body parsed and stored on all three; `Set-Cookie: mam_id=…` rotates the stored cookie (percent-decoded) on any status.
- [ ] No cookie stored ⇒ jsonIp path ⇒ contact without `ipUpdate` ⇒ `/health` = `no-cookie`; fresh install ⇒ `pending`.
- [ ] `POST /updates` resets the automatic countdown; every mutation response carries a **fresh** `nextContactAt`.
- [ ] All timestamps parse via `Temporal.ZonedDateTime.from` (bracketed IANA zone present); `TZ` honored.
- [ ] `state.json` byte-identical for the same logical state (2-space pretty, key order, omitted keys, no trailing newline); v1 file (`currentCookie`) rescued to `{"version":2,"cookie":…}` on next write; corrupt file ⇒ 500 `file-read-error`/`json-parse-error`/`schema-error` on `/state` and `/health`, never silently reset.

**SSE**
- [ ] `GET /events` ⇒ 200, `Content-Type: text/event-stream`, `Cache-Control: no-cache`, `Connection: keep-alive`; **nothing** sent on connect; stays open silently past any idle window.
- [ ] Every persisted contact (startup, interval, `/updates`, `/cookie`) ⇒ exactly one `data: changed\n\n` to every client.
- [ ] `POST /logout` for the stream's session ⇒ clean stream end; token-authed streams unaffected by any session's logout.

**Static & misc**
- [ ] `/web` and `/web/` serve `index.html`; `/web/assets/<hash>.js` serves with `text/javascript`; `/web/nope` ⇒ JSON 404; no cache headers.
- [ ] Startup log `Mousehole v0.5.0 (<hash>) running at …`; `[LEVEL]` prefixes on stdout; `NO_COLOR=` (empty) disables ANSI.
- [ ] Debug level logs `<METHOD> <path> → <status>` per request (path only, U+2192 arrow).
- [ ] SIGTERM mid-contact ⇒ contact completes + persists, then exit 0.
- [ ] Docker drop-in matrix: `state-and-build.md` §5.5.

**Final gate**: point the real built frontend at the Rust backend and walk the
UI — login, set cookie (good + bad), Update Now, countdown donut renders and
re-mounts after each contact, SSE-driven refresh, logout — with the browser
devtools network tab compared against the Bun instance.
