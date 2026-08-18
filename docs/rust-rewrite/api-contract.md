# Mousehole HTTP API Contract (Rust rewrite reference)

Derived from the TypeScript backend at commit-time of writing. Sources of truth:
`src/backend/app.ts`, `server.ts`, `http-boundary.ts`, `error.ts`, `session.ts`,
`sse.ts`, `handlers/*.ts`, `state/*.ts`, `src/shared/{error-response,public-state,git-hash,time}.ts`,
cross-verified against `tests/app.test.ts`, `tests/http-boundary.test.ts`,
`tests/login.test.ts`, and the actual frontend consumers in `src/frontend/hooks/*.ts`
and `src/frontend/components/*.tsx`.

**Hard requirement:** the existing React frontend is reused verbatim as a built
Vite bundle. Every field name, casing, status code, cookie name, and header
below must match exactly.

---

## 1. Global HTTP behavior

### 1.1 Middleware layering (order matters)

The Hono app applies, outermost first:

1. **Debug access log** — after the response is produced, logs
   `` `${method} ${path} → ${status}` `` at `debug` level. No effect on the wire.
2. **Global body limit** — `8192` bytes (`8 * 1024`), applied to **every**
   route, *before* any per-route boundary check. Exceeding it responds:

   ```
   413
   {"type":"payload-too-large","message":"Request body must not exceed 8192 bytes."}
   ```

   Pinned by test: an oversized `PUT /cookie` with **no credentials** returns
   **413, not 401** — the limit runs in front of the auth stack.
3. **Per-route boundary stack** — each route lists its checks varargs-style in
   **host → auth → origin → json** order; the *first* failing check responds
   and the handler never runs. (Details in §5.)
4. **Handler.**

Cross-cutting:

- **`onError`** — any exception thrown from a handler is converted by
  `toErrorResponseArgs` (§9) into a JSON body + status.
- **`notFound`** — any unmatched path:

  ```
  404
  {"type":"not-found","message":"Not Found"}
  ```

### 1.2 Response content type

All JSON responses are produced by Hono's `c.json(...)`, i.e.
`Content-Type: application/json`. The frontend only ever calls
`response.json()`, so the exact charset suffix is not load-bearing, but use
plain `application/json` to match.

### 1.3 Server runtime settings that shape the API

- Listens on `MOUSEHOLE_PORT` (default **5010**).
- **Idle timeout disabled** (`Bun.serve({ idleTimeout: 0 })`) — a quiet SSE
  stream must never be closed by the server as "idle". The Rust server must
  likewise not apply an idle/read timeout that would kill `/events`.
- On startup, `validateRuntimeSecurityConfig` **refuses to boot** (throws)
  when no credential is configured and `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH` is
  not `true`. Warnings (not errors) for: `allowedHosts`/`allowedOrigins` set
  to `*`, and token-only auth ("Browser login will be unavailable...").
- Startup log line: `` `Mousehole v${version} (${gitHash}) running at ${url}` ``.
- Immediately after binding, the contact scheduler starts: **one contact right
  away**, then rescheduled per interval (§10.2).
- `SIGINT`/`SIGTERM` → stop scheduler (cancel timer, drain in-flight contact
  via the mutex), stop server, exit 0.

---

## 2. Shared wire types

### 2.1 `PublicState` (`src/shared/public-state.ts`)

Returned by `GET /state`, `PUT /cookie`, and `POST /updates`. Never contains
the MAM cookie value — only its presence.

```jsonc
{
  "hasCookie": true,            // boolean, ALWAYS present
  "hasAuth": true,              // boolean, ALWAYS present; true iff auth is "configured"
                                // (a password and/or token exists), false in no-auth mode
  "nextContactAt": "2026-08-18T09:05:00.123456789-05:00[America/Chicago]",
                                // OPTIONAL — key OMITTED when no automatic contact is scheduled
  "lastMamContact": { ... }     // OPTIONAL SerializedMamContact — key OMITTED when no contact yet
}
```

> Optionality is by **key omission** (TS `JSON.stringify` drops `undefined`),
> not `null`. In serde: `#[serde(skip_serializing_if = "Option::is_none")]`.

### 2.2 `SerializedMamContact`

A tagged union on the boolean field `reached`:

```jsonc
// transport failure
{
  "at": "<RFC 9557 string>",
  "reached": false,
  "error": { "type": "network-error", "message": "Network request to https://... failed" }
}

// contact succeeded
{
  "at": "<RFC 9557 string>",
  "reached": true,
  "ip": "203.0.113.7",          // string (IPv4 dotted quad)
  "asn": 64496,                  // number
  "as": "Example AS name",       // string
  "ipUpdate": {                  // OPTIONAL — present ONLY when a cookie drove a
                                 // dynamicSeedbox update (absent for cookie-less IP lookups)
    "success": true,             // boolean — MAM's "Success"
    "msg": "Completed",          // string — MAM's msg VERBATIM, display-only, never branch on it
    "httpStatus": 200            // number — the HTTP status MAM answered with (200/429/403/...)
  }
}
```

`error.type`/`error.message` are the `type`/`message` of the internal error
converted through `toErrorResponseArgs` (the `cause` chain is dropped).
Observed `error.type` values written by the contact loop: `network-error`,
`timeout-error`, `json-parse-error`, `schema-error`, `unhandled-error`
(the last one e.g. when `jsonIp.php` answers non-2xx — a plain `Error`).

### 2.3 Timestamps — RFC 9557, bracketed zone REQUIRED

`at` and `nextContactAt` are `Temporal.ZonedDateTime.toString()` output — RFC
9557: ISO 8601 date-time **with offset AND a bracketed IANA time-zone
annotation**, e.g.

```
2026-08-18T09:05:00.123456789-05:00[America/Chicago]
```

The frontend parses both with `Temporal.ZonedDateTime.from(string)`
(`mam-response.tsx` lines 48–49), which **throws without the `[Zone]`
annotation**. A bare RFC 3339 string will blank-screen the dashboard. The Rust
backend must emit the bracketed annotation (zone = the host's local IANA zone,
per `getNowZdt()` = `Temporal.Now.zonedDateTimeISO(systemTimeZone)`).
Sub-second precision is whatever the clock gives; the frontend only calls
`.epochMilliseconds`, so nanosecond digits are optional but harmless.

### 2.4 `ContactStatus` classification (`classify`)

Used by `GET /health` (server-side) and by the dashboard (client-side, from
`PublicState`). Interprets **status code only, never `msg`**:

| Value | Condition |
|---|---|
| `"pending"` | no contact recorded yet |
| `"unreachable"` | `reached === false` |
| `"no-cookie"` | reached, no `ipUpdate` (lookup only) |
| `"ok"` | `ipUpdate.httpStatus === 200` |
| `"throttled"` | `ipUpdate.httpStatus === 429` |
| `"rejected"` | any other `ipUpdate.httpStatus` (MAM uses 403) |

### 2.5 `ErrorResponseBody` (`src/shared/error-response.ts`)

Every non-2xx JSON response (except `POST /login`'s, see §4.2) has shape:

```jsonc
{
  "type": "<kebab-case error type>",   // string, always present
  "message": "<human-actionable text>", // string, always present
  "issues": [ { "path": "value", "message": "..." } ],  // OPTIONAL — schema-error only
  "cause": { "type": "...", "message": "...", "cause": { ... } }  // OPTIONAL — nested chain
}
```

The frontend **never branches on `type`** — it branches on **status 401**
(login screen) vs. everything else, and displays `message`. `issues` and
`cause` are extra diagnostics; preserve them for parity but nothing in the UI
reads them today.

---

## 3. Route summary

| Method | Path | Boundary stack (in order) | Success |
|---|---|---|---|
| GET | `/` | *(none)* | 302 redirect (content negotiation) |
| POST | `/login` | host, origin, json | 200 `{"ok":true}` + `Set-Cookie` |
| POST | `/logout` | host, origin | 200 `{"ok":true}` |
| POST | `/updates` | host, **auth**, origin | 200 `PublicState` |
| GET | `/state` | host, **auth** *(no origin — GET is CSRF-safe)* | 200 `PublicState` |
| PUT | `/cookie` | host, **auth**, origin, json | 200 `PublicState` |
| GET | `/health` | *(none — public probe)* | 200 `{"lastMamContactResult": ...}` |
| GET | `/events` | host, **auth**, origin | 200 SSE stream |
| GET | `/web`, `/web/*` | *(none — login page needs the assets)* | static frontend |

The global 8 KiB body limit applies to all of them (§1.1).

---

## 4. Routes in detail

### 4.1 `GET /` — content-negotiated entry redirect

Uses Hono's `accepts` helper over the `Accept` header with
`supports: ["text/html", "application/json"]`, `default: "text/html"`.

- Negotiated `text/html` (including no/unknown `Accept`) → `302`,
  `Location: /web`.
- Negotiated `application/json` → `302`, `Location: /health`.

(Pinned: `Accept: application/json` → `/health`; `Accept: text/html` and
missing header → `/web`; status 302.) Full q-value negotiation is what the
helper does; a simple "does Accept prefer application/json over text/html"
match satisfies the pinned tests.

### 4.2 `POST /login`

Boundary: host → origin → json. **No auth check** (this is how you get auth).
The handler parses its own body (it does not use the shared error shape).

Request body: `{"password": "<string>"}` (`Content-Type: application/json`
required by the json boundary).

**Response shape is `{ok, message?}` — NOT `{type, message}`.** Exact cases,
in evaluation order:

| Case | Status | Body |
|---|---|---|
| Auth not "configured", or configured without a password (token-only) | 500 | `{"ok":false,"message":"Browser login is unavailable: MOUSEHOLE_AUTH_PASSWORD is not set"}` |
| Body is not parseable JSON | 400 | `{"ok":false,"message":"Malformed JSON in request body"}` |
| JSON but wrong schema (no string `password`) | 400 | `{"ok":false,"message":"Request body must match expected schema"}` |
| Wrong password (constant-time compare, §5.2) | 401 | `{"ok":false,"message":"Incorrect password"}` |
| Correct password | 200 | `{"ok":true}` + session `Set-Cookie` (§6) |

Frontend (`hooks/login.ts`): on `!response.ok` it reads `body.message`
(fallback text `"Incorrect password."`) and displays it; on success it
refetches `GET /state`. So `message` must be present on failures.

### 4.3 `POST /logout`

Boundary: host → origin. **No auth** — logging out with a dead session must
still succeed. No body expected (frontend sends none, no `Content-Type`;
there is deliberately no json boundary here).

Behavior: if the request carries a session cookie naming a known session,
delete it (which also closes that session's SSE streams, §7.4); then always:

```
200
{"ok":true}
```

plus a cookie-clearing `Set-Cookie` (Hono `deleteCookie`):
`mousehole-session=; Max-Age=0; Path=/`.

Frontend (`hooks/logout.ts`): any non-OK response shows the static banner
`"Logout failed."` (the body is not read); on success it invalidates and
refetches `GET /state` (which then 401s ⇒ login screen).

### 4.4 `POST /updates` — contact MAM now

Boundary: host → auth → origin. No body (frontend sends none, no
`Content-Type` — do **not** add a json boundary).

Runs one full contact (`commitContact()`, §10) with the cookie currently on
disk, persists the result, notifies SSE clients, reschedules the automatic
timer, and returns the resulting **`PublicState`** with `200`.

**A failed MAM contact is still `200`** — transport failures (connection
refused, timeout, bad JSON from MAM, non-2xx from `jsonIp.php`) are recorded
*into* the state as a `reached: false` contact and returned as data. Only
unexpected internal failures (state file unreadable/corrupt, write failure)
throw and become 5xx via `toErrorResponseArgs`.

Frontend (`hooks/update.ts`): on `!ok` shows
`` `${body?.message ?? "Update failed."} Check server logs for details.` ``;
on success replaces the cached state with the response body.

### 4.5 `GET /state`

Boundary: host → auth. (**No origin check** — reads are CSRF-safe; keep it
that way, adding one would break token-less cross-origin GET setups pinned by
the test matrix.)

Reads persisted state (no network call) and returns **`PublicState`**, `200`.
`hasAuth` := `config.auth` is "configured"; `nextContactAt` := the scheduler's
current target (RFC 9557 string) or omitted.

Failure modes (all via thrown errors → §9): state file unreadable → 500
`file-read-error`; not JSON → 500 `json-parse-error`; wrong shape/version
after migration → 500 `schema-error`. A **missing** state file is not an
error — fresh install ⇒ `{"hasCookie":false,"hasAuth":...}` with contact
fields omitted.

Frontend (`hooks/state.ts`) — the most behavior-laden consumer:

- `401` (any body) ⇒ treated as unauthenticated ⇒ login screen. **Status is
  the only signal**; the body is not read for this.
- other non-OK ⇒ error screen showing the body's `message` if the body parsed
  as JSON with a string `message`, else
  `` `GET /state failed with status ${status}` ``.
- retry policy: never on 401; never on any status `< 500` (deterministic
  rejections like `host-not-allowed` must not spin the loading state); at
  most 2 retries otherwise.

### 4.6 `PUT /cookie`

Boundary: host → auth → origin → json.

Request body: `{"value": "<non-empty string>"}` — the MAM `mam_id` session
cookie value. Zod schema: `{ value: z.string().min(1) }`. There is
deliberately **no way to clear** the cookie over the API.

Handler steps:

1. Read the raw body text. Empty text ⇒ parsed value `undefined` (falls to
   schema failure below). Non-empty but unparseable ⇒ **throw**
   `JSONParseError.fromRequest` ⇒

   ```
   400
   {"type":"json-parse-error","message":"Error parsing JSON from request with method PUT and URL <url>","cause":{...}}
   ```

2. Schema-validate ⇒ on failure **throw** `SchemaError.fromUserSource("request body", ...)` ⇒

   ```
   400
   {"type":"schema-error",
    "message":"Schema validation failed for data from request body: value: <first zod message>",
    "issues":[{"path":"value","message":"<zod message>"}]}
   ```

   (Pinned: empty `value` ⇒ 400, `type: "schema-error"`,
   `issues[0].path === "value"`. Exact zod message text is not pinned — the
   UI displays `message` opaquely.)

   **Path-prefix subtlety:** the `<path>: ` prefix in `message` comes from
   `` `${first.path ? `${first.path}: ` : ""}${first.message}` `` — when the
   first issue's path is **empty** (body is not an object at all: empty body
   ⇒ `undefined`, or a JSON scalar/array), the message is just
   `Schema validation failed for data from request body: <zod message>` with
   no path prefix, and `issues[0].path === ""`. With zero issues (unreachable
   in practice) the summary falls back to the literal `invalid data`.

3. `commitContact(newValue)` — inside the mutex: replace the stored cookie
   with the submitted value **before** contacting MAM, contact, persist,
   notify SSE, reschedule. **The submitted cookie is persisted even when MAM
   is unreachable or rejects it** (the IP update is a side effect, not a
   precondition). If MAM's response carried a rotated `mam_id` `Set-Cookie`,
   the **rotated** value is what gets persisted.
4. Return the resulting **`PublicState`**, `200` — so a bad-cookie rejection
   shows up immediately in `lastMamContact.ipUpdate` of the response.

Frontend (`hooks/cookie.ts`): on `!ok` shows `body.message` (fallback
`"Failed to save cookie."`); on success replaces cached state with the body.

### 4.7 `GET /health`

**No boundary checks at all** — public liveness probe usable by Docker
healthchecks without credentials, any Host, any Origin.

Pure read of persisted state (no network call):

```
200
{"lastMamContactResult":"ok" | "throttled" | "rejected" | "unreachable" | "no-cookie" | "pending"}
```

Returns 200 whenever the server is up and can read its state; the *value*
carries the verdict. A corrupt/unreadable state file throws ⇒ 500 error body.

### 4.8 `GET /events` — see §7.

### 4.9 `/web` — see §8.

---

## 5. Boundary checks (exact semantics)

All boundary rejections use the standard error shape (§2.5) with **no
server-side logging** (deliberate: the client surfaces the actionable
message). Precedence = the per-route order host → auth → origin → json, with
the global 413 in front of everything.

### 5.1 Host allowlist (`host-not-allowed`, DNS-rebinding defense)

Config `MOUSEHOLE_ALLOWED_HOSTS`: default allowlist
`["localhost", "127.0.0.1", "[::1]"]`; `*` disables the check; else
comma-separated list (items trimmed, empties dropped; an all-empty list is a
startup error).

Check, when not `*`:

- Effective host := `Host` header, else the host of the request URL.
- Missing entirely ⇒ `403 {"type":"host-not-allowed","message":"Request Host header is required."}`
- Parse as `hostname[:port]` by URL-parsing `"http://" + trim(lowercase(value))`;
  reject if that yields a path other than `/`, a query, or no hostname ⇒
  `403 ... "Request Host \"<host>\" is invalid."`
- Match against each allowlist entry parsed the same way. Rule **without** a
  port matches that hostname on **any** port; rule with a port requires an
  exact port match. Hostname comparison is on the URL-normalized lowercase
  hostname (IPv6 must be bracketed, e.g. `[::1]`; URL normalization also
  makes `LOCALHOST` or trailing-dot-free forms compare equal).
- No match ⇒

  ```
  403
  {"type":"host-not-allowed","message":"Host \"<original header value>\" not allowed. (Add to MOUSEHOLE_ALLOWED_HOSTS to permit it.)"}
  ```

  (Pinned: message contains the offending value and the env var name.)

### 5.2 Authentication (`requireAuth`)

Config (from `MOUSEHOLE_AUTH_PASSWORD`, `MOUSEHOLE_AUTH_TOKEN`, each with a
`*_FILE` indirection variant that takes precedence, and
`MOUSEHOLE_INSECURE_ALLOW_NO_AUTH`):

- `configured` — password and/or token set (setting a credential together
  with the insecure flag is a startup error).
- `none` — neither set; only bootable with the insecure flag.

Ladder, in order (the order is what lets `originAllowed` trust "token"):

1. `type == "none"` + insecure flag ⇒ pass, method = `"none"`.
   `type == "none"` without the flag ⇒
   `500 {"type":"auth-not-configured","message":"Mousehole authentication is not configured. Set MOUSEHOLE_AUTH_PASSWORD or MOUSEHOLE_AUTH_TOKEN to enable."}`
   (defense-in-depth — normally unreachable because startup refuses this
   config, but keep it: the app factory is separable from the composition root).
2. **Session first:** valid `mousehole-session` cookie ⇒ pass, method = `"session"`.
3. **Then bearer:** `Authorization` matches `/^Bearer\s+(.+)$/i` (scheme
   case-insensitive, one-or-more whitespace, the entire remainder — spaces
   included — is the token) and the token equals the configured token under a
   **constant-time comparison** ⇒ pass, method = `"token"`. (`safeEqual`:
   length mismatch short-circuits false; otherwise XOR-accumulate over
   code points.)
4. Otherwise ⇒ `401` with header `WWW-Authenticate: Bearer realm="Mousehole"`
   and body `{"type":"authentication-required","message": <one of>}`:
   - an `Authorization` header was present:
     `"Rejected Bearer token (wrong value, or MOUSEHOLE_AUTH_TOKEN not set)"`
   - else a session cookie was present: `"Unknown or expired session cookie"`
   - else: `"No credentials presented"`

The auth method is recorded per-request for the origin check.

### 5.3 Origin (`origin-not-allowed`, CSRF defense)

Config `MOUSEHOLE_ALLOWED_ORIGINS`: default **same-origin**; `*` disables;
else comma-separated allowlist.

- **Token-authenticated requests skip this check entirely** (a cross-site
  page cannot attach a bearer header without a CORS preflight that is never
  approved). Only requests whose recorded auth method is exactly `"token"`
  skip; sessions, no-auth mode, and routes without `requireAuth` in their
  stack (`/login`, `/logout` — where the method is unset) are always checked.
  Pinned both ways: session + evil `Origin` ⇒ 403 even with a token
  configured; same request with `Authorization: Bearer <token>` ⇒ 200.
- Requests **without an `Origin` header pass** (curl, same-origin GET
  navigations, EventSource).
- Normalize the header by URL-parsing and taking `.origin` (an unparseable
  value is compared verbatim — e.g. the literal string `null` from sandboxed
  frames, which then never matches and is rejected).
- `same-origin` mode: compare against the origin synthesized from the request
  (scheme the server was reached on + `Host` header).
  Allowlist mode: compare against each configured origin, itself normalized
  the same way.
- Mismatch ⇒

  ```
  403
  {"type":"origin-not-allowed","message":"Origin \"<normalized origin>\" is not allowed. (Add to MOUSEHOLE_ALLOWED_ORIGINS to permit it.)"}
  ```

### 5.4 JSON content type (`requireJsonBody`)

Applied only on `/login` and `/cookie`. Checks the **header only** (no body
sniffing): media type = the part of `Content-Type` before the first `;`,
trimmed, lowercased; must be exactly `application/json` (so
`application/json; charset=utf-8` passes; a missing header fails):

```
415
{"type":"unsupported-media-type","message":"Unsupported content type \"<raw header or empty string>\", must be \"application/json\""}
```

---

## 6. Sessions and the session cookie

- **Cookie name: `mousehole-session`** (constant `SESSION_COOKIE_NAME`).
- Session ID: 32 cryptographically random bytes, **base64url, no padding**
  (43 chars).
- Store: in-memory map `id → expiry`. Sessions do **not** survive restart.
- Lifetime: `MOUSEHOLE_SESSION_DURATION_SECONDS` (default `604800` = 1 week).
  Expiry is enforced both by a per-session timer *and* by pruning expired
  entries on every validity check — an expired session stops authenticating
  even with no intervening request (pinned), and its SSE streams are closed
  at expiry (§7.4).
- `Set-Cookie` on login success, exactly these attributes:
  `Max-Age=<sessionDurationSeconds>; HttpOnly; SameSite=Lax; Path=/`, plus
  `Secure` iff `MOUSEHOLE_HTTPS_ONLY_COOKIES=true` (default absent — pinned
  both ways). No `Domain`, no `Expires`.
- Logout / clearing: `Set-Cookie: mousehole-session=; Max-Age=0; Path=/`.
- Cookie extraction parses the `Cookie` header for that one name (standard
  cookie-header parsing).
- Each login creates a **new** session; multiple concurrent sessions are fine.
  Deleting an unknown session is a no-op. Sessions are per-process (two
  instances never share).

---

## 7. Server-Sent Events — `GET /events`

### 7.1 Request

Boundary: host → auth → origin. The browser's `EventSource` can only carry
the ambient session cookie (no headers), so in practice this is
session-authenticated; a bearer-token client (curl) also works and skips the
origin check. When logged out it 401s — the frontend deliberately mounts the
subscription only on the authenticated dashboard.

### 7.2 Response

```
200
Content-Type: text/event-stream
Cache-Control: no-cache
Connection: keep-alive
```

Body: an unending stream. **Nothing is sent on connect** — no hello, no
retry field, no id, no comments/keep-alive pings, ever. The Bun server's idle
timeout is disabled to keep silent streams open (§1.3); replicate that.

### 7.3 Events

Exactly one kind of frame, the contentless change signal:

```
data: changed\n\n
```

No `event:` field ⇒ arrives as the default `message` event (the frontend
listens for `message`; it also treats `open` as a signal to re-pull, catching
anything missed while disconnected). The payload is deliberately meaningless —
**`GET /state` is the single source of truth**; clients react to any frame by
refetching it.

**When it fires:** `sse.notify()` broadcasts to *all* connected clients after
**every persisted contact** — i.e. at the end of every `commitContact()`:
startup contact, each automatic interval contact, every `POST /updates`, and
every `PUT /cookie` (the initiating client receives it too; harmless — the UI
also gets the state in the mutation response). It fires after the state file
write, before the initiating HTTP response completes.

A send to a dead/closed client is swallowed and the client dropped from the
registry.

### 7.4 Stream lifecycle & session coupling

- On connect, the stream is registered together with the request's session ID
  (empty string when the request had no session cookie — e.g. token or
  no-auth clients; such streams are never force-closed).
- **When a session is deleted — logout or expiry — every stream registered
  under it is server-side closed** (clean end-of-stream, not an error frame).
  The browser `EventSource` then auto-reconnects, gets a 401, and the app
  lands on the login screen. Pinned: an open stream ends (`done`) promptly
  after `POST /logout` with that session, and after the session's own expiry.
- Client cancellation (browser tab gone) unregisters the stream.
- No server-driven reconnection hints: no `retry:` line — browser default
  reconnect delay applies.

---

## 8. Static assets, SPA, and `PUBLIC_GIT_HASH`

### 8.1 Production serving (the mode the Rust rewrite implements)

The `vite build` output directory (TS used `./dist` relative to CWD; make it
configurable) is mounted under `/web` with **no boundary checks** (the login
page needs its own assets):

- `GET /web` → serve `<dist>/index.html` (this exact route, not a redirect).
- `GET /web/*` → strip the leading `/web` and serve from `<dist>`:
  - `/web/assets/index-D2fBnPMk.js` → `<dist>/assets/index-D2fBnPMk.js`
  - trailing slash (`/web/`) and extension-less paths resolve with an
    `index.html` default document (`/web/` → `<dist>/index.html`; `/web/foo`
    → `<dist>/foo/index.html` if present) — that's Hono `serveStatic`'s
    default-document behavior. Only `/web` and `/web/` matter in practice.
- **No SPA fallback**: a `/web/<missing-file>` miss falls through to the
  global JSON 404 (§1.1). The app is a single page served at `/web` with no
  client-side routing, so a catch-all rewrite to `index.html` is NOT needed
  and NOT present today. Path traversal must be rejected (the TS stack's
  static handler guards it; `tower-http`'s `ServeDir` does too).
- Content types: standard by file extension (`.html` → `text/html`, `.js` →
  `text/javascript`, `.css` → `text/css`, `.svg` → `image/svg+xml`, ...).
- **Cache headers: none are set today** (no `Cache-Control`, no explicit
  ETag contract). The UI works without them. If you add caching, keep
  `index.html` uncached — Vite asset filenames are content-hashed, the HTML
  is not.

Dev mode in TS reverse-proxied `/web`/`/web/*` to a Vite dev server at
`http://localhost:5173` (with `/web` → `/web/` rewrite, forwarding request
headers; 502 `text/plain` if Vite is down). Irrelevant to a Rust backend
serving the prebuilt bundle; noted for completeness.

### 8.2 `PUBLIC_GIT_HASH` — build-time inlining, NOT an API

There is **no endpoint serving the git hash**. Two independent consumers:

- **Frontend:** `src/shared/git-hash.ts` reads `process.env.PUBLIC_GIT_HASH`;
  Vite's `define` replaces that expression **at `vite build` time** with a
  literal (resolution order: `$PUBLIC_GIT_HASH` env var → `git rev-parse
  --short HEAD` → `""`; the footer hides the hash when empty). The reused
  built bundle therefore already contains its hash — the Rust backend does
  nothing for this.
- **Backend:** reads the same env var at **runtime** solely for the startup
  log line (§1.3). Optional nicety in Rust (`option_env!`/env var).

The `/` route, `/health`, and all JSON endpoints carry no version/hash
headers or fields.

---

## 9. Error model & catalog

### 9.1 Conversion rule (`toErrorResponseArgs`)

Any error escaping a handler becomes `{ body, status }`:

- `message`: the error's message (or `` `Unhandled error: ${String(value)}` ``
  for non-`Error` throws).
- `type`: the Mousehole error's `errorType`, else `"unhandled-error"`.
- `status`: the Mousehole error's `httpStatus`, else `500`.
- `issues`: attached for `SchemaError` only.
- `cause`: if the error has any **truthy** `cause` (not only `Error` values —
  the guard is `error.cause &&`) and the error is not a `SchemaError` (whose
  zod cause is already represented by `issues`), the cause is recursively
  converted and embedded as a nested body — producing a
  `{type,message,cause:{...}}` chain. A non-`Error` cause renders as
  `{"type":"unhandled-error","message":"Unhandled error: <String(value)>"}`.
  Nested causes are typically `"unhandled-error"` (plain Errors). (The typed
  Mousehole error constructors coerce/drop non-Error causes, so in practice
  non-Error causes only appear via plain `Error` instances.)

### 9.2 Full catalog of `type` values on the wire

| `type` | Status | Message (template) | Producer |
|---|---|---|---|
| `payload-too-large` | 413 | `Request body must not exceed 8192 bytes.` | global body limit |
| `not-found` | 404 | `Not Found` | unmatched route / static miss |
| `host-not-allowed` | 403 | §5.1 (three variants) | host boundary |
| `authentication-required` | 401 | §5.2 (three variants) + `WWW-Authenticate: Bearer realm="Mousehole"` | auth boundary |
| `auth-not-configured` | 500 | §5.2 case 1 | auth boundary (defense-in-depth) |
| `origin-not-allowed` | 403 | §5.3 | origin boundary |
| `unsupported-media-type` | 415 | §5.4 | json boundary |
| `schema-error` | 400 | `Schema validation failed for data from request body: <path>: <msg>` (+`issues`) | `PUT /cookie` bad body |
| `schema-error` | 500 | `Schema validation failed for data from <path-or-url>: ...` (+`issues`) | corrupt state file / (stored contact error) |
| `json-parse-error` | 400 | `Error parsing JSON from request with method <M> and URL <url>` | `PUT /cookie` malformed JSON |
| `json-parse-error` | 500 | `Error parsing JSON from file at <path>` | corrupt state file |
| `file-read-error` | 500 | `Error reading file: <path>. Check that it is readable and is not a directory.` | state read |
| `file-write-error` | 500 | `Error writing file: <path>. Check that the parent directory exists and is writable.` | state write |
| `directory-create-error` | 500 | `Error creating directory: <path>. Check that the parent directory exists and you have write permissions.` | state dir |
| `network-error` | 500 | `Network request to <url> failed` | (stored in contact `error`, §4.4 — normally never a wire status) |
| `timeout-error` | 504 | `Request to <url> timed out after <n>s. Is the network up?` | (same — stored, not thrown to HTTP) |
| `unhandled-error` | 500 | verbatim message | anything unexpected |

`network-error`/`timeout-error`/external `schema-error`/`json-parse-error
(from response)` are caught inside the contact loop and recorded into
`lastMamContact.error` — the mutation endpoints still answer 200 (§4.4). They
would only surface as HTTP statuses through an unexpected path.

`POST /login` is the one route with a different failure body: `{ok:false,message}` (§4.2).

### 9.3 What the frontend actually distinguishes

- **`GET /state` → 401**: the only place status-code identity matters
  structurally (login screen). Everything else: display `message`, keyed off
  `response.ok` only.
- Retry suppression on `GET /state` for status `< 500` (§4.5) — keep 4xx vs
  5xx assignments as specified or the UI's retry behavior changes.
- `fetch`-level network failure (backend down) is detected client-side as a
  `TypeError`; no server involvement.

---

## 10. Behavioral subtleties (ordering, timing, concurrency)

### 10.1 The contact mutex

Every contact — startup, interval timer, `POST /updates`, `PUT /cookie` —
funnels through `commitContact(newCookie?)`, serialized by an async **FIFO
mutex**. Inside the lock: read state from disk → (for `PUT /cookie`) splice in
the new cookie → contact MAM → write state (atomic: write `state.json.tmp`,
rename over `state.json`; parent dir `mkdir -p`'d first) → `sse.notify()`.
In the `finally` (still before releasing): **reschedule the next automatic
contact `intervalSeconds` from now** — a manual update resets the automatic
timer. Concurrent `PUT /cookie` + `POST /updates` therefore never interleave
reads/writes; the second simply runs after the first.

### 10.2 Scheduling & `nextContactAt`

`MOUSEHOLE_UPDATE_INTERVAL_SECONDS` default `300`. `start()` fires an
immediate contact (async, errors logged not fatal) and each contact's
completion arms the next one-shot timer. `nextContactAt` is computed as
`now + interval` when the timer is armed and is what `GET /state` reports —
informational, not a hard guarantee. After `stop()` no rescheduling occurs and
the field is absent. Handlers build `PublicState` *after* `commitContact`
returns, so a mutation response's `nextContactAt` already reflects the fresh
schedule.

### 10.3 State file (context for the 5xx cases)

`<MOUSEHOLE_STATE_DIR_PATH>/state.json` (dir default `/var/lib/mousehole`),
pretty-printed 2-space JSON:
`{"version":2,"cookie":"...","lastMamContact":{...}}` (serialized contact =
wire shape, §2.2). Missing file ⇒ fresh install (`undefined`), **any other
read failure is a hard error** (never treat corruption as "no state" — the
next write would destroy the cookie). Legacy/other versions migrate by
salvaging only a non-empty string `currentCookie` field (v1's name) into
`cookie` and discarding the rest; the migrated candidate is then
schema-validated (failure ⇒ `schema-error`).

### 10.4 MAM specifics that leak into the API surface

(Full MAM contract belongs in a separate doc; these affect wire payloads.)

- With a cookie: `GET https://t.myanonamouse.net/json/dynamicSeedbox.php`
  with header `Cookie: mam_id=<value>`, `User-Agent:
  mousehole-by-timtimtim/<package.json version>`, redirects **not** followed,
  timeout `MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS` (default 10). Response JSON
  `{Success,msg,ip,ASN,AS}` (MAM's uppercase keys) is mapped to the
  lowercase wire fields `{success,msg}` under `ipUpdate` plus top-level
  `{ip,asn,as}`; `ipUpdate.httpStatus` is MAM's actual HTTP status —
  **MAM's body is parsed and stored even for 429/403 responses.**
  A `Set-Cookie: mam_id=<new>` in the response rotates the stored cookie.
- Without a cookie: `GET .../json/jsonIp.php` (same UA/timeout), non-2xx is
  an error (`unhandled-error` in the stored contact); success yields a
  reached contact **without** `ipUpdate`.

### 10.5 Misc

- **Method mismatch on a known path is a 404, not a 405.** Hono routes on
  (method, path) pairs; `POST /state` or `GET /login` simply doesn't match and
  falls through to `notFound` ⇒ `404 {"type":"not-found","message":"Not Found"}`.
  There is no 405 / `Allow` header anywhere. **axum's default for a matched
  path with an unmatched method is 405 with an empty body** — override it
  (e.g. `MethodRouter::fallback`, or a `map_response` layer converting 405 to
  the JSON 404) to keep parity. Nothing pins this in tests, but curl users and
  monitors observe it.
- **`HEAD` is not explicitly routed**, but Hono answers a `HEAD` request by
  dispatching it as `GET` and stripping the body (headers + status of the GET
  route, empty body) — so `HEAD /health` is 200. axum's `routing::get()`
  handles HEAD the same way natively; no work needed, just don't "fix" it away.
- `GET /` is the only redirect; everything else answers in place.
- Boundary rejections are not logged server-side; only a rejected *presented*
  bearer token merits default-level logging per the code comments (currently
  surfaced via the 401 message, not a log).

---

## 11. Configuration reference (env vars shaping the API)

| Variable | Default | Notes |
|---|---|---|
| `MOUSEHOLE_PORT` | `5010` | 1–65535 |
| `MOUSEHOLE_STATE_DIR_PATH` | `/var/lib/mousehole` | holds `state.json` |
| `MOUSEHOLE_UPDATE_INTERVAL_SECONDS` | `300` | positive number (fractions legal) |
| `MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS` | `10` | positive number |
| `MOUSEHOLE_SESSION_DURATION_SECONDS` | `604800` | positive **integer**; also the cookie `Max-Age` |
| `MOUSEHOLE_AUTH_PASSWORD` / `_FILE` | unset | enables browser login |
| `MOUSEHOLE_AUTH_TOKEN` / `_FILE` | unset | enables bearer auth |
| `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH` | `false` | `"true"`/`"false"` only; mutually exclusive with credentials (startup error) |
| `MOUSEHOLE_ALLOWED_HOSTS` | `localhost,127.0.0.1,[::1]` | `*` = allow all |
| `MOUSEHOLE_ALLOWED_ORIGINS` | same-origin | `*` = allow all |
| `MOUSEHOLE_HTTPS_ONLY_COOKIES` | `false` | adds `Secure` to the session cookie |
| `MOUSEHOLE_LOG_LEVEL` | `info` | `debug`\|`info`\|`warn`\|`error` (case-insensitive input) |
| `PUBLIC_GIT_HASH` | unset | startup log only (frontend copy is baked at build) |

Common parsing rules: values are trimmed; empty ⇒ unset. `*_FILE` variants
take precedence over the plain variable; unreadable file ⇒ startup error;
file whose trimmed content is empty ⇒ unset. Invalid values ⇒ startup error
`` `Invalid environment variable NAME="raw": <reason>` ``.

---

## 12. Rust implementation notes (axum / tokio / reqwest / serde)

**App assembly (axum):**

- One `Router` built from an `AppContext`-equivalent `Arc<AppState>` holding
  `Config`, the state-file store, session store, SSE registry, and contact
  scheduler handle — mirroring `context.ts` so tests can build isolated apps.
- Boundary checks as `axum::middleware::from_fn_with_state` layers applied
  **per-route** via `route_layer`, composed in host → auth → origin → json
  order (remember: in axum, the **last-added layer runs first**, so push them
  in reverse). The global body limit is `RequestBodyLimitLayer::new(8192)` on
  the whole router — but axum's built-in 413 has no JSON body, so either map
  it with a custom layer or implement the limit as an outermost
  `from_fn` that answers the exact `payload-too-large` JSON.
- Auth middleware inserts `AuthMethod` into request extensions
  (`req.extensions_mut().insert(AuthMethod::Token)`); the origin middleware
  reads it — the direct analog of Hono's `c.set("authMethod", ...)`.
- The `{type,message,issues?,cause?}` error body: one `MouseholeError` enum
  implementing `IntoResponse`; a fallback route (`Router::fallback`) for the
  JSON 404; a wrapper for handler `Result<Json<T>, MouseholeError>`.
- **Method mismatch must also 404** (§10.5): axum's `MethodRouter` answers
  unmatched methods with a bare 405 — attach the JSON-404 fallback per method
  router (`get(h).fallback(not_found)`) or map 405 responses to the JSON 404
  in an outer layer. axum's `get()` already serves HEAD like Hono does.
- `GET /` content negotiation: read `Accept` yourself (or via a tiny q-value
  parse) and answer `Redirect::to("/web")` / `Redirect::to("/health")` —
  `Redirect::to` emits 303 in some versions; use `StatusCode::FOUND` +
  `Location` explicitly to keep the pinned **302**.

**Serde:**

- `PublicState`, `SerializedMamContact`, `IpUpdate` with
  `#[serde(rename_all = "camelCase")]` where needed (`hasCookie`,
  `nextContactAt`, `lastMamContact`, `ipUpdate`, `httpStatus` — note `asn`
  and `as` are lowercase; `as` needs `#[serde(rename = "as")]` since it's a
  Rust keyword, field name e.g. `as_name`).
- The contact union: `#[serde(untagged)]` won't round-trip the `reached`
  discriminator reliably — model it as
  `struct Contact { at: String, #[serde(flatten)] outcome: Outcome }` with a
  custom (de)serialization, or simplest: two structs and
  `#[serde(untagged)]` **with** explicit `reached: bool` literal fields
  validated manually; on the write side plain struct-with-Options also works
  since the backend is the only producer. Skip-none everywhere:
  `#[serde(skip_serializing_if = "Option::is_none")]` for `nextContactAt`,
  `lastMamContact`, `ipUpdate` — key omission, never `null`.
- State file: same types + `version: u32` (must equal 2), `serde_json::to_string_pretty`.

**Timestamps:** neither `chrono` nor `time` emits RFC 9557 bracketed zones.
Options: (a) the `temporal_rs` crate (the Temporal reference impl);
(b) `jiff` — `jiff::Zoned`'s default `Display` is exactly RFC 9557
(`2026-08-18T09:05:00-05:00[America/Chicago]`) and it resolves the system
IANA zone. **Use `jiff`.** Verify output round-trips through
`Temporal.ZonedDateTime.from` (the polyfill) in a smoke test against the real
bundle.

**Sessions:** `Mutex<HashMap<String, Instant>>` (expiry instant); generate IDs
with `rand::rngs::OsRng` 32 bytes → `base64::engine::general_purpose::URL_SAFE_NO_PAD`.
Prune-on-check reproduces the TS behavior; instead of per-session timers, a
small tokio task (or prune-on-check plus a periodic sweep) must also **close
SSE streams at expiry without any request arriving** — the TS pins that. Use
`cookie` crate / `axum-extra`'s `CookieJar` for parsing and building
`Set-Cookie` (attributes exactly as §6).

**Constant-time compare:** the `subtle` crate (`ConstantTimeEq` over bytes)
for password and token; keep the length short-circuit semantics (subtle
requires equal lengths anyway — return false on mismatch).

**SSE (axum + tokio):**

- Registry: `Mutex<Vec<Client>>` where
  `Client { session_id: String, tx: tokio::sync::mpsc::UnboundedSender<Frame> }`.
- Handler returns `Sse::new(stream)` built from `UnboundedReceiverStream`,
  mapping the notify signal to `Event::default().data("changed")` (axum
  renders `data: changed\n\n`). **Do not** add `.keep_alive(...)` — the TS
  server sends no pings; ensure no hyper/server write timeouts kill idle
  streams instead.
- `notify()` = send to all, dropping clients whose channel is closed.
  `close_session_streams(session_id)` = drop the senders (receiver stream
  ends ⇒ clean stream close, which is what the browser sees).
- Set `Cache-Control: no-cache` (axum's `Sse` does); the `Connection:
  keep-alive` header is a hop-by-hop nicety the TS sends — HTTP/1.1 defaults
  to persistent connections, but emit it for byte-level parity if trivial.

**Contact loop (tokio):** one task owning the schedule; `commitContact` as an
`async fn` guarded by `tokio::sync::Mutex<()>` (FIFO under contention, same
as the TS mutex). Reschedule in a `finally`-equivalent (run it before
returning on both paths, or via a guard). `stop()`: set a stopped flag, abort
the sleep, then acquire the mutex to drain an in-flight contact.

**MAM calls (reqwest):** a shared `Client` with
`redirect(Policy::none())`, per-request `.timeout(Duration::from_secs_f64(t))`,
`User-Agent: mousehole-by-timtimtim/<version>` and `Cookie: mam_id=<value>`
headers. Map `reqwest` timeout errors → `timeout-error`, other transport
errors → `network-error`, body-JSON failure → `json-parse-error`, zod-like
validation (serde with an IPv4 check on `ip`) → `schema-error` — these types
end up verbatim in the stored contact's `error.type`. Read the status
**before** consuming the body and parse the body regardless of status (MAM's
429/403 bodies are meaningful). Cookie rotation: iterate
`response.headers().get_all(SET_COOKIE)` and parse for `mam_id`.

**State file:** `tokio::fs`; write `state.json.tmp` then `rename` (atomic on
the same filesystem); `create_dir_all` first; only `ErrorKind::NotFound` on
read maps to "fresh install", everything else is `file-read-error`.

**Static files:** `tower_http::services::ServeDir` nested at `/web` (strips
the prefix itself), with an explicit route for `/web` → `ServeFile(index.html)`.
Replace `ServeDir`'s default 404 with the JSON not-found body
(`.fallback(...)` / `not_found_service`). Do not add an SPA fallback.

**Graceful shutdown:** `axum::serve(...).with_graceful_shutdown(signal)` on
SIGINT/SIGTERM; stop the scheduler first, mirroring `server.ts`.

**Parity test plan:** port `tests/app.test.ts`'s route-protection matrix
(§3 + §5 rejection shapes), the login/logout cookie assertions, the SSE
`data: changed` + logout-close assertions, and the contact-flow tests against
a fake MAM server — they pin every subtle behavior in this document.
