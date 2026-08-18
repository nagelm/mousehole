# Mousehole Rust Rewrite Spec — Configuration, Authentication, HTTP Boundary

Sources of truth (all read at commit state of 2026-08-18):

- `src/backend/config.ts` — env var resolution
- `src/backend/session.ts` — session store + cookie
- `src/backend/handlers/login.ts` — login handler
- `src/backend/http-boundary.ts` — host/auth/origin/content-type checks
- `src/backend/app.ts` — route wiring, middleware order, body limit, 404/error/redirect
- `src/backend/server.ts` — composition root, startup validation
- `src/backend/logger.ts` — log levels/format
- `src/backend/error.ts`, `src/shared/error-response.ts` — error envelope
- `.env.development`, `README.md`, `docs/security-guide.md`, `docs/API.md` — documented semantics

The React frontend is reused verbatim, so **every** field name, casing, status
code, cookie name, and header below must be reproduced exactly.

---

## 1. Configuration

### 1.1 General env-var reading rules

From `config.ts`:

- Reading an env var (`getEnv`): the raw value is **trimmed**; an empty or
  whitespace-only value is treated **identically to unset**
  (`env[name]?.trim() || undefined`).
- Config is built **exactly once** at startup by the composition root
  (`startServer` → `buildConfig(process.env)`). There is no module-global
  config; everything receives it explicitly. Any validation failure **throws
  before the listener binds** — the process refuses to start.
- Validation failures produce this exact error message shape (message from the
  first zod issue):

  ```
  Invalid environment variable ${name}="${raw}": ${message}
  ```

- Boolean flags accept **exactly** the strings `true` or `false` (after trim);
  anything else is a validation error. Unset ⇒ `false`.
- Numeric parsing uses `z.coerce.number()` — the string is coerced with JS
  `Number(...)` semantics, then range-checked. Note the practical consequences:
  - "positive number" schemas (`UPDATE_INTERVAL`, `MAM_REQUEST_TIMEOUT`)
    accept **non-integer** values like `0.5`; must be `> 0`.
  - "positive int" schema (`SESSION_DURATION`) requires an integer `> 0`.
  - port schema requires an integer in `1..=65535`.

### 1.2 The complete env var table

| Variable | Type | Default | Validation |
|---|---|---|---|
| `MOUSEHOLE_LOG_LEVEL` | enum | `info` | lowercased before parsing; one of `debug`, `info`, `warn`, `error` (so `INFO` is accepted) |
| `MOUSEHOLE_STATE_DIR_PATH` | string | `/var/lib/mousehole` | none (any non-empty string; empty ⇒ default) |
| `MOUSEHOLE_UPDATE_INTERVAL_SECONDS` | number | `300` | positive number (floats allowed) |
| `MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS` | number | `10` | positive number (floats allowed) |
| `MOUSEHOLE_SESSION_DURATION_SECONDS` | integer | `604800` (1 week) | positive integer |
| `MOUSEHOLE_PORT` | integer | `5010` | integer 1–65535 |
| `MOUSEHOLE_HTTPS_ONLY_COOKIES` | bool flag | `false` | exactly `true`/`false` |
| `MOUSEHOLE_AUTH_PASSWORD` | secret | unset | none (any non-empty string); supports `_FILE` variant |
| `MOUSEHOLE_AUTH_PASSWORD_FILE` | file path | unset | file must be readable (else fatal) |
| `MOUSEHOLE_AUTH_TOKEN` | secret | unset | none; supports `_FILE` variant |
| `MOUSEHOLE_AUTH_TOKEN_FILE` | file path | unset | file must be readable (else fatal) |
| `MOUSEHOLE_INSECURE_ALLOW_NO_AUTH` | bool flag | `false` | exactly `true`/`false`; mutually exclusive with credentials (see 1.4) |
| `MOUSEHOLE_ALLOWED_HOSTS` | `*` or CSV | `localhost,127.0.0.1,[::1]` | see 1.5 |
| `MOUSEHOLE_ALLOWED_ORIGINS` | `*` or CSV | same-origin mode | see 1.5 |

Process-level variables read outside `buildConfig`:

| Variable | Effect |
|---|---|
| `NODE_ENV` | `production` ⇒ (a) web UI served statically from `./dist` instead of proxied to Vite at `http://localhost:5173`; (b) logger renders `Error` values via `util.inspect` (stack + cause chain). The Docker image sets `ENV NODE_ENV=production`. The Rust rewrite always behaves as "production" — dev proxying is a TS-dev-workflow feature. |
| `NO_COLOR` | If present **at all** (even empty — note: NOT trimmed, checked with `!== undefined`), disables ANSI color in log output regardless of TTY (https://no-color.org). |
| `TZ` | Documented in README (default `Etc/UTC` in Docker). Not read by config code; it flows through libc/ICU into `Intl.DateTimeFormat().resolvedOptions().timeZone`, which `src/shared/time.ts` uses to produce RFC 9557 zoned timestamps (e.g. `2025-06-21T14:27:28.113-05:00[America/Chicago]`) in state responses. |
| `PUBLIC_GIT_HASH` | Baked at build time into the startup banner (`gitHash`). |

Dev-only note: `.env.development` (loaded automatically by Bun in dev, not
production) sets `MOUSEHOLE_UPDATE_INTERVAL_SECONDS=60`,
`MOUSEHOLE_STATE_DIR_PATH=./.state`, `MOUSEHOLE_AUTH_PASSWORD=password`, and
may be overridden by `.env.local`. Bun strips inline `#` comments. The Rust
binary does **not** need dotenv loading for production parity; the container
supplies real env vars.

### 1.3 The `_FILE` secret variant (`resolveSecret`)

Applies to `MOUSEHOLE_AUTH_PASSWORD` and `MOUSEHOLE_AUTH_TOKEN` only.

Algorithm, exactly:

1. Read `${name}_FILE` (trimmed; empty = unset).
2. If `${name}_FILE` is **unset**: the value is the plain `${name}` env var
   (trimmed; empty = unset).
3. If `${name}_FILE` is **set**: the plain `${name}` env var is **ignored
   entirely** (the `_FILE` variant takes precedence even over a non-empty
   plain variable).
   - Read the file as UTF-8. On any read failure, **fail fast** at startup
     with:
     ```
     Invalid environment variable ${name}_FILE="${filePath}": could not read file (${reason})
     ```
     where `${reason}` is the underlying I/O error message.
   - Trim the file contents. If the trimmed content is empty, the credential
     resolves to `undefined` — "exactly as an empty or unset env var does".
     Subtle: `_FILE` pointing at an empty file + a non-empty plain var still
     yields **no credential**.

### 1.4 Auth config resolution (`resolveAuthConfig`)

Inputs: `password` (via `_FILE` mechanism), `token` (via `_FILE` mechanism),
`insecureAllowNoAuth` flag.

- **Mutual exclusion (fatal at startup):** if `insecureAllowNoAuth` is `true`
  and either credential is set, throw:

  ```
  MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true cannot be combined with ${credential}: turning off authentication and configuring a credential are mutually exclusive. Unset one of them.
  ```

  where `${credential}` is `MOUSEHOLE_AUTH_PASSWORD` if a password is set
  (even if a token is also set), else `MOUSEHOLE_AUTH_TOKEN`.

- Resolution ladder:
  1. password set ⇒ `{ type: "configured", password, token }` (token included
     iff also set)
  2. else token set ⇒ `{ type: "configured", token }`
  3. else ⇒ `{ type: "none", insecureAllowNoAuth }`

### 1.5 Allowed hosts / origins resolution

`MOUSEHOLE_ALLOWED_HOSTS`:

- unset ⇒ `{ type: "allowlist", hosts: ["localhost", "127.0.0.1", "[::1]"] }`
- exactly `*` (after trim) ⇒ `{ type: "all" }`
- otherwise: split on `,`, trim each entry, drop empty entries. If the result
  is empty (e.g. value was `",,"`), throw:
  ```
  Invalid environment variable MOUSEHOLE_ALLOWED_HOSTS: must not be empty; use * to allow all hosts
  ```

`MOUSEHOLE_ALLOWED_ORIGINS`:

- unset ⇒ `{ type: "same-origin" }`
- exactly `*` ⇒ `{ type: "all" }`
- otherwise: same CSV parse; empty result throws:
  ```
  Invalid environment variable MOUSEHOLE_ALLOWED_ORIGINS: must not be empty; use * to allow all origins
  ```

### 1.6 Startup security validation (`validateRuntimeSecurityConfig`)

Runs **after** the listener binds and the startup banner is logged. In order:

1. If hosts mode is `all`, `logger.warn`:
   ```
   MOUSEHOLE_ALLOWED_HOSTS allows any Host header. This is less secure and almost always avoidable. Set it to your specific host(s) or IP(s).
   ```
2. If origins mode is `all`, `logger.warn`:
   ```
   MOUSEHOLE_ALLOWED_ORIGINS allows any Origin Header for cross-origin requests. This is less secure and almost always avoidable. Set it to your specific allowed origins.
   ```
3. If auth is `configured` with token only (no password), `logger.warn`:
   ```
   Browser login will be unavailable. Set MOUSEHOLE_AUTH_PASSWORD to enable it.
   ```
4. If auth is `none` and `insecureAllowNoAuth` is false, **throw (fatal)**:
   ```
   Mousehole authentication is not configured. Set MOUSEHOLE_AUTH_PASSWORD and/or MOUSEHOLE_AUTH_TOKEN, or set MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true to opt out.
   ```
5. If auth is `none` with opt-out, `logger.warn`:
   ```
   Running without authentication (MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true). Do not expose Mousehole to mixed-trust LAN, VPN, or public interfaces.
   ```

Startup banner (info level), logged before the validation above:

```
Mousehole v${version} (${gitHash}) running at ${server.url}
```

---

## 2. Authentication

Three ways a request clears auth, tracked as an `AuthMethod` of
`"none" | "session" | "token"` (the method matters downstream — see origin
check, §4.4).

### 2.1 Password sessions (browser)

**Cookie name:** `mousehole-session` (constant `SESSION_COOKIE_NAME`).

**Session token generation:** 32 cryptographically random bytes
(`crypto.getRandomValues`), encoded **base64url without padding** (43 chars,
alphabet `[A-Za-z0-9_-]`).

**Store:** in-memory `Map<sessionId, { expiry, timeout }>` — sessions do
**not** survive a restart (restart logs everyone out; the SPA then gets 401
from `GET /state` and shows the login screen).

**Expiry:** fixed-lifetime, **no sliding renewal**. `expiry = now +
durationSeconds*1000`. Two reaping mechanisms, both required:

1. A per-session timer (`setTimeout(durationMs)`, unref'd so it never keeps
   the process alive) deletes the session on schedule.
2. A lazy sweep: **every** `isSessionValid` call first prunes all entries with
   `expiry <= now` before the lookup.

Deleting a known session (expiry, logout, or manual) fires
`onSessionDeleted(sessionId)`, which the context wires to
`sse.closeSessionStreams(sessionId)` — closing that session's SSE streams so
the client re-pulls `GET /state`, gets 401, and lands on login.

**Set-Cookie on login** (Hono `setCookie` with
`{ maxAge: durationSeconds, httpOnly: true, sameSite: "lax", secure: httpsOnlyCookies, path: "/" }`);
Hono 4.12.25 serializes attributes in the order Max-Age, Path, HttpOnly,
Secure, SameSite:

```
Set-Cookie: mousehole-session=<43-char-base64url>; Max-Age=604800; Path=/; HttpOnly; SameSite=Lax
```

with `; Secure` inserted between `HttpOnly` and `SameSite=Lax` when
`MOUSEHOLE_HTTPS_ONLY_COOKIES=true`. (The value is `encodeURIComponent`-ed by
Hono, a no-op for base64url. Attribute order is not semantically significant
to browsers; value, flags, Max-Age, and Path are.)

**Set-Cookie on logout** (Hono `deleteCookie` with `{ path: "/" }`, which is
`setCookie(name, "", { path, maxAge: 0 })`):

```
Set-Cookie: mousehole-session=; Max-Age=0; Path=/
```

(No HttpOnly/Secure/SameSite attributes on the deletion cookie.)

**Session extraction (`extractSessionId`):** parse the `Cookie` request header
for the `mousehole-session` name; absent header or absent pair ⇒ no session.
Hono's parser: split on `;`, trim pairs, match name exactly, strip one pair of
surrounding double quotes if present, percent-decode the value.

**Session validity:** extracted id must be a current key of the store map
(after the lazy prune).

**Password comparison:** `safeEqual` — a constant-time-style comparison:
returns false immediately on length mismatch, otherwise ORs together the XOR
of the code points at each index and requires the accumulator to be 0. Used
for both login passwords and Bearer tokens.

### 2.2 Bearer token (API clients)

- Header: `Authorization: Bearer <token>`, matched with the regex
  `/^Bearer\s+(.+)$/i` against the raw header value:
  - the `Bearer` scheme keyword is **case-insensitive** (`bearer`, `BEARER`
    accepted);
  - one or more whitespace characters between scheme and token;
  - the token is everything to end-of-line (`.+` — it may contain spaces, and
    trailing whitespace is **not** trimmed; it must byte-match the configured
    token via `safeEqual`).
- Only checked when `authConfig.token` is configured.
- **Every route behind `requireAuth` accepts it**: `POST /updates`,
  `GET /state`, `PUT /cookie`, `GET /events`. (`/login`, `/logout`,
  `/health`, `/`, `/web/*` have no `requireAuth`, so a Bearer header there is
  simply ignored.)
- Auth ladder order is **session first, then token** (deliberate: if a valid
  session cookie authorized the request, the reported method is `"session"`
  and the CSRF/origin check still applies; a reported `"token"` guarantees the
  ambient cookie played no part, which is what lets the origin check be
  waived).

### 2.3 Insecure no-auth mode

`MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true` (with no credentials set): every
`requireAuth` check passes with method `"none"`. Origin and host checks
**still apply**. The startup warning in §1.6 is logged.

If auth is unconfigured *and* the opt-out is absent, the server refuses to
start (§1.6 item 4); but the middleware also carries a defensive branch (used
by direct `createApp` embedding, e.g. tests): a protected request under
unconfigured auth returns **500**:

```json
{ "type": "auth-not-configured", "message": "Mousehole authentication is not configured. Set MOUSEHOLE_AUTH_PASSWORD or MOUSEHOLE_AUTH_TOKEN to enable." }
```

### 2.4 Auth failure response (requireAuth)

Status **401**, header `WWW-Authenticate: Bearer realm="Mousehole"`, JSON body
`{ "type": "authentication-required", "message": <one of three> }` where the
message is chosen by what was presented:

| Condition (checked in this order) | `message` |
|---|---|
| `Authorization` header present (any value, even non-Bearer) | `Rejected Bearer token (wrong value, or MOUSEHOLE_AUTH_TOKEN not set)` |
| else a `mousehole-session` cookie was presented | `Unknown or expired session cookie` |
| else | `No credentials presented` |

The boundary deliberately does **not** log rejections (client-facing messages
carry the diagnosis; duplicating to the server log was decided to be noise).

### 2.5 `hasAuth` in state responses (frontend dependency)

`GET /state` (handlers/state.ts) reports
`hasAuth: ctx.config.auth.type === "configured"` — i.e. true if *either*
credential is set. The login UI keys off this.

---

## 3. Login / logout endpoints

Both use a **different response envelope** from boundary/error responses:
`{ "ok": boolean, "message"?: string }` — not `{ type, message }`.

### 3.1 `POST /login`

Middleware stack (in order): `host`, `origin`, `requireJsonBody`. **No
`requireAuth`** (the login page must reach it). Then the handler, whose checks
run in exactly this order:

1. **Auth mode check (before reading the body):** if auth is not `configured`
   or has no password (token-only or no-auth setups) ⇒ **500**
   ```json
   { "ok": false, "message": "Browser login is unavailable: MOUSEHOLE_AUTH_PASSWORD is not set" }
   ```
2. **JSON parse:** body must parse as JSON, else **400**
   ```json
   { "ok": false, "message": "Malformed JSON in request body" }
   ```
3. **Schema:** body must match `z.object({ password: z.string() })` — a
   `password` key of type string is required; **unknown extra keys are
   silently ignored** (zod default strip mode); missing/non-string password ⇒
   **400**
   ```json
   { "ok": false, "message": "Request body must match expected schema" }
   ```
4. **Password compare** via `safeEqual`; mismatch ⇒ **401**
   ```json
   { "ok": false, "message": "Incorrect password" }
   ```
5. **Success** ⇒ create a session, attach the Set-Cookie header (§2.1), and
   respond **200**
   ```json
   { "ok": true }
   ```

Full failure precedence at this route (outermost first): body-limit
(413, only when Content-Length is declared — see §4.6) → host (403) → origin
(403) → content-type (415) → auth-mode (500) → JSON parse (400) → schema
(400) → password (401).

### 3.2 `POST /logout`

Middleware stack: `host`, `origin`. No auth, no content-type check, no body.

Behavior: delete the session named by the request's cookie **if present and
known** (firing `onSessionDeleted` → SSE close only for known sessions), then
unconditionally send the clearing Set-Cookie (§2.1), then **200**
`{ "ok": true }` — always, even with no cookie or an unknown/expired one.

---

## 4. HTTP boundary

### 4.1 Architecture and per-route stacks

Boundary checks are pure functions wrapped as route middleware; **failure
precedence is the textual order at the route** — the first failing check
responds and the handler never runs. Routes list requirements in
host → auth → origin → json order:

| Route | host | auth | origin | json body | Notes |
|---|---|---|---|---|---|
| `POST /login` | ✅ | — | ✅ | ✅ | envelope `{ok,…}` |
| `POST /logout` | ✅ | — | ✅ | — | |
| `POST /updates` | ✅ | ✅ | ✅ | — | takes no body |
| `GET /state` | ✅ | ✅ | — | — | no origin check (safe method) |
| `PUT /cookie` | ✅ | ✅ | ✅ | ✅ | |
| `GET /health` | — | — | — | — | fully open, **no host check** |
| `GET /events` | ✅ | ✅ | ✅ | — | SSE |
| `GET /` | — | — | — | — | redirect only |
| `/web`, `/web/*` | — | — | — | — | static assets; unprotected (login page needs them) |

Global middleware, registered before all routes, in order:

1. **Debug request logger** — logs *after* the response:
   `logger.debug("${method} ${path} → ${status}")` (path only, no query
   string; literal ` → ` with U+2192).
2. **Body limit** — 8 KiB (`8 * 1024` bytes), all requests (§4.6).

Boundary failure responses are JSON:
`{ "type": <string>, "message": <string> }` plus optional extra headers, with
the check's status code.

### 4.2 Host check (`hostAllowed`) — DNS-rebinding defense

- Mode `all` ⇒ pass.
- Request host = `Host` header; if the header is absent, fall back to the host
  component of the request URL (in Bun the URL is synthesized from server
  config + Host + path, so this fallback is effectively the listener address).
  If still empty ⇒ **403**
  `{ "type": "host-not-allowed", "message": "Request Host header is required." }`.
- **Parsing** (`parseHostAndPort`, applied to both the request host and each
  allowlist rule): trim, lowercase, then parse `"http://" + value` as a URL.
  Invalid if: URL parse fails, hostname is empty, pathname ≠ `/`, or a query
  string is present (i.e. the value smuggled a path/query). An invalid request
  host ⇒ **403**
  `{ "type": "host-not-allowed", "message": "Request Host \"${host}\" is invalid." }`.
  An unparseable allowlist **rule** silently matches nothing (no startup
  error).
  - URL parsing implies normalization: case-insensitive hostnames, IDN →
    punycode, IPv6 hosts keep brackets (`[::1]` ⇒ hostname `[::1]`), and —
    subtle — **an explicit `:80` port is stripped** (default port for the
    `http://` prefix used in parsing), so `example.com:80` parses with no
    port. `:443` and other ports are preserved.
- **Matching** (`hostMatchesRule`): hostnames must be equal; if the rule has
  no port it matches **any** request port; if the rule has a port it must
  equal the request port exactly. Consequences: the defaults
  `localhost`/`127.0.0.1`/`[::1]` match any port; a rule `myhost:80` behaves
  like port-agnostic `myhost` (port stripped at parse); a request host
  `myhost:80` matches rule `myhost:8080`? No — request port is `undefined`
  after stripping, rule port is `8080`, mismatch ⇒ rejected.
- No match ⇒ **403**
  ```json
  { "type": "host-not-allowed", "message": "Host \"${host}\" not allowed. (Add to MOUSEHOLE_ALLOWED_HOSTS to permit it.)" }
  ```
  (`${host}` is the raw, pre-normalization header value.)

### 4.3 Auth check (`requireAuth`)

Ladder (see §2): none-mode → session → token → 401 failure; publishes the
successful `authMethod` for downstream middleware. Details and exact bodies in
§2.3–§2.4.

### 4.4 Origin check (`originAllowed`) — CSRF defense

- **Skipped entirely when `authMethod == "token"`.** Rationale (from source):
  CSRF requires an ambient credential; a Bearer token is explicit — a
  cross-site page cannot attach it. Sessions, the no-auth opt-out, and stacks
  without `requireAuth` (login/logout) stay enforced. This requires
  `requireAuth` to run **earlier** in the same route stack.
- Mode `all` ⇒ pass.
- **No `Origin` header ⇒ pass** (origin-less requests — curl, same-origin GET
  navigations — are allowed).
- Request origin normalization (`normalizeOrigin`): `new URL(origin).origin`
  (lowercases, strips default ports, e.g. `HTTP://Foo.com:80` →
  `http://foo.com`). If parsing fails (notably the literal `Origin: null`
  sent from sandboxed/opaque contexts), the **raw string is used as-is** —
  so `null` can be explicitly allowlisted.
- Mode `same-origin` (the default): the request origin must equal
  `new URL(request.url).origin` — the origin **synthesized from the server's
  own scheme and the Host header**. There is **no `X-Forwarded-Proto` /
  `X-Forwarded-Host` handling anywhere** (verified by grep). Behind a
  TLS-terminating reverse proxy the synthesized origin is `http://…` while
  the browser sends `https://…`, so same-origin mode fails — deployments
  behind proxies must set `MOUSEHOLE_ALLOWED_ORIGINS` explicitly (this is the
  documented behavior in the security guide; preserve it, do not "fix" it by
  trusting forwarded headers).
- Mode `allowlist`: each configured origin is normalized the same way, and
  the request origin must be exactly equal to one of them (string equality;
  scheme and non-default port are significant, paths are impossible after
  `.origin`).
- Rejection ⇒ **403**
  ```json
  { "type": "origin-not-allowed", "message": "Origin \"${requestOrigin}\" is not allowed. (Add to MOUSEHOLE_ALLOWED_ORIGINS to permit it.)" }
  ```
  (`${requestOrigin}` is the *normalized* origin.)

### 4.5 JSON content-type check (`requireJsonBody`)

- Take the `Content-Type` header, split at the first `;` (parameters like
  `charset=` are ignored), trim, lowercase; must equal `application/json`.
- Failure ⇒ **415**
  ```json
  { "type": "unsupported-media-type", "message": "Unsupported content type \"${contentType}\", must be \"application/json\"" }
  ```
  where `${contentType}` is the raw header value, or the empty string when
  the header is absent (message then contains `""`).

### 4.6 Body size limit

Hono `bodyLimit` with `maxSize = 8192` bytes, mounted **globally before every
route**, rejecting with **413**:

```json
{ "type": "payload-too-large", "message": "Request body must not exceed 8192 bytes." }
```

Semantics of the Hono middleware to reproduce:

- Requests without a body pass through untouched.
- If a `Content-Length` header is declared: compare it against the limit
  **up front** — an oversized declared length gets the 413 *before any other
  boundary check* (it precedes host/auth/origin since it's mounted earlier).
- If no `Content-Length` (chunked): the body stream is wrapped with a byte
  counter and the request is rejected once the limit is crossed during the
  handler's read.

### 4.7 Not-found, error handler, and `GET /`

- **404 (no matching route):** `{ "type": "not-found", "message": "Not Found" }`.
- **Unhandled/thrown errors** are converted by `toErrorResponseArgs`
  (`error.ts`) to `{ type, message, issues?, cause? }` with the error's
  `httpStatus` (500 for anything unclassified, `type: "unhandled-error"`).
  A wrapped `cause` chain is rendered recursively as nested
  `{ type, message }` objects, except a `SchemaError`'s zod cause (already
  represented by `issues: [{ path, message }]`). This envelope is shared with
  the frontend (`src/shared/error-response.ts`): `type` is a stable
  machine-readable tag, `message` human-readable.
- **`GET /`:** content-negotiates the `Accept` header between `text/html` and
  `application/json` (default `text/html`); responds **302** redirect to
  `/web` for html, `/health` for json.
- **`GET /events`** (for completeness of the boundary): after clearing
  host+auth+origin, responds 200 with headers
  `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
  `Connection: keep-alive`, registering the stream under the request's session
  id (empty string for token/no-auth clients — such streams are never closed
  by session deletion since `""` is never a real session). The server's idle
  timeout is disabled (`idleTimeout: 0` in `Bun.serve`) so quiet SSE streams
  aren't reaped.

### 4.8 Web UI mounting (prod mode)

Static-serve the built Vite bundle under `/web`: `GET /web` serves
`${root}/index.html`; `GET /web/*` serves files with the `/web` prefix
stripped from the path (root is `./dist`). Unauthenticated by design.

---

## 5. Logging

- **Levels** (most → least verbose): `debug`, `info`, `warn`, `error`;
  numeric 2/3/4/5. A message is emitted when its level ≥ the configured
  threshold. Default threshold `info`; set from
  `MOUSEHOLE_LOG_LEVEL` once config resolves (startup logging before that
  uses the default).
- **Destination: stdout for every level** (Twelve-Factor; routing/storage is
  the environment's job).
- **Format:** `[LEVEL] message…` — the uppercased level name in brackets,
  a space, then the args space-joined (console.log semantics).
- **Color:** ANSI-colored prefix only when stdout is a TTY **and** `NO_COLOR`
  is not present (present-but-empty still disables). Codes: debug `\x1b[90m`
  (gray), info `\x1b[36m` (cyan), warn `\x1b[33m` (yellow), error `\x1b[31m`
  (red); reset `\x1b[0m`. Only the `[LEVEL]` prefix is colored.
- **Error rendering:** when any logged value is an `Error`, the prefix goes on
  its own line and the error follows on subsequent lines; in production the
  error renders via `util.inspect` (message + stack + cause chain).
- **Per-request log line** (debug): `${method} ${path} → ${status}` after the
  response is produced.
- The boundary intentionally logs nothing on rejection (§2.4).

---

## 6. Rust implementation notes (axum / tokio / reqwest / serde)

**Config (`config.rs`)**

- Mirror `buildConfig` as `Config::from_env(vars: &HashMap<String,String>)` →
  `Result<Config, ConfigError>` so tests stay hermetic; inject the `_FILE`
  reader as a closure/trait (`Fn(&Path) -> io::Result<String>`) exactly like
  the TS `readTextFileSync` seam.
- Reproduce the exact error strings — they are user-facing and documented.
  Replace zod's per-issue message with equivalent hand-rolled text; keep the
  `Invalid environment variable NAME="raw": …` frame verbatim.
- `AuthConfig` as an enum:
  `Configured { password: Option<String>, token: Option<String> }` (invariant:
  at least one `Some` — enforce in the constructor) and
  `None { insecure_allow_no_auth: bool }`. Same for
  `AllowedHosts::{All, Allowlist(Vec<String>)}` and
  `AllowedOrigins::{SameOrigin, All, Allowlist(Vec<String>)}`.
- Numbers: parse with `f64::from_str` for the positive-number vars (floats
  are legal), `u64` for session duration, `u16` (≥1) for port. Trim first;
  empty ⇒ default.
- Secrets: wrap in a `Secret<String>` newtype with a redacting `Debug` impl so
  values can't leak into logs.

**Sessions (`session.rs`)**

- `Arc<Mutex<HashMap<String, Instant /* expiry */>>>` (or `parking_lot`).
  Skip per-session timers: the TS lazy sweep in `is_session_valid` is
  sufficient for auth correctness, but the timer also drives
  `on_session_deleted` → SSE close *at expiry moment*; replicate with a
  single `tokio::time::sleep`-based reaper task (or a `DelayQueue`) that
  fires the SSE-close callback, matching "expired session ⇒ stream closes ⇒
  client re-pulls and gets 401".
- Token: 32 bytes from `rand::rngs::OsRng` (or `getrandom`), encoded
  `base64::engine::general_purpose::URL_SAFE_NO_PAD` → 43 chars.
- `safeEqual` → `subtle::ConstantTimeEq` on the byte slices (equivalent given
  equal-length check; TS compares UTF-16-ish code points, but both sides are
  ASCII in practice — byte comparison is fine and strictly better).
- Cookies: build `Set-Cookie` by hand or with the `cookie` crate:
  `Cookie::build(("mousehole-session", id)).max_age(Duration::seconds(n)).path("/").http_only(true).same_site(SameSite::Lax)` +
  `.secure(cfg)` conditional; deletion cookie = empty value, `Max-Age=0`,
  `Path=/` only. Parse inbound cookies leniently (split `;`, trim, first
  match, strip surrounding quotes, percent-decode) — `cookie::Cookie::split_parse`
  is close enough; session ids are base64url so decoding is a no-op.

**Boundary middleware (`http-boundary.rs`)**

- Model each check as a pure
  `fn(&http::request::Parts, …) -> Result<AuthMethod?, BoundaryFailure>` and
  wrap with `axum::middleware::from_fn_with_state`, or implement as
  extractors. Per-route stacks compose with `.route_layer(...)`; **order
  matters** — axum layers run outermost-first in the order
  `ServiceBuilder::layer` adds them, so build host → auth → origin → json
  explicitly per route (mirroring the vararg order in `app.ts`).
- Propagate `AuthMethod` from the auth middleware to the origin middleware via
  `Request::extensions_mut().insert(AuthMethod::…)` (the Hono
  `c.set("authMethod", …)` equivalent).
- `BoundaryFailure { status: StatusCode, r#type: &'static str, message: String, headers: Option<HeaderMap> }`
  with `IntoResponse` producing `(status, headers, Json(json!({"type": …, "message": …})))`.
  Field name `type` needs `#[serde(rename = "type")]` or `json!` literals.
- Host/origin parsing: `url::Url::parse(&format!("http://{lowercased}"))` and
  reject when `path() != "/"` or `query().is_some()` or host is empty — this
  reproduces the `:80`-stripping subtlety for free (`url` also drops default
  ports). `Url::origin().ascii_serialization()` gives the normalized-origin
  string for the origin check (note: it serializes an *opaque* origin as
  `"null"`, which coincidentally matches the TS fallback for `Origin: null`).
  The "synthesized request origin" for same-origin mode is
  `format!("http://{host_header}")` normalized the same way — the Rust server,
  like Bun, serves plain HTTP, so the scheme is always `http` and forwarded
  headers are deliberately ignored.
- Body limit: `tower_http::limit::RequestBodyLimitLayer` /
  `axum::extract::DefaultBodyLimit::max(8192)` gets the streaming behavior,
  but you must map the failure to the exact JSON
  `{"type":"payload-too-large","message":"Request body must not exceed 8192 bytes."}`
  with 413 (`map_response` on the 413, or a custom layer). Keep it as a
  global layer so declared-oversize requests fail before host/auth, matching
  Hono.
- Bearer parse: `regex` `^Bearer\s+(.+)$` with `(?i)`, applied to the raw
  header string; do not trim the capture.
- `WWW-Authenticate: Bearer realm="Mousehole"` via a static `HeaderValue`.

**Login/logout (`handlers/login.rs`)**

- Do **not** use `Json<T>` extractors directly — the error precedence and
  bodies are bespoke. Take `body: axum::body::Bytes`, run the ordered ladder:
  config check (500) → `serde_json::from_slice::<serde_json::Value>` (400
  "Malformed JSON in request body") → shape check (object with string
  `password`, unknown keys ignored — `#[derive(Deserialize)] struct { password: String }`
  *without* `deny_unknown_fields` matches zod's strip mode, but derive it from
  `Value` in a second step so parse-vs-schema failures stay distinguishable)
  → `ConstantTimeEq` (401) → 200. Envelope is `{"ok": bool, "message"?: str}`
  — a different serde struct from the boundary envelope; skip `message` when
  `None` (`#[serde(skip_serializing_if = "Option::is_none")]`).

**Server (`main.rs`)**

- `tokio::net::TcpListener::bind(("0.0.0.0", port))` + `axum::serve`. No HTTP
  idle timeout by default in axum/hyper — matches `idleTimeout: 0` (verify
  hyper's `http1` keep-alive defaults don't reap quiet SSE connections).
- Static files: `tower_http::services::ServeDir` nested at `/web` with
  `ServeFile` fallback for `/web` → `dist/index.html`.
- `GET /`: inspect `Accept` (simple contains/priority check between
  `text/html` and `application/json`, default html) →
  `Redirect::to("/web")` / `Redirect::to("/health")` — use `Redirect::to`
  (302 Found, matching Hono's default).
- 404 fallback: `Router::fallback` returning the `not-found` envelope; global
  error conversion mirrors `toErrorResponseArgs` (an app `Error` enum with
  `status()`, `error_type()`, optional `issues`, recursive `cause`
  serialization).
- Startup sequence must match: build config (exit non-zero on error, printing
  the message) → set log threshold → bind → banner →
  `validate_runtime_security_config` (warns + possible fatal) → start the
  contact scheduler.

**Logger**

- `tracing` is idiomatic but its default format won't match `[INFO] …` on
  stdout; either a custom `FormatEvent` for `tracing-subscriber`, or a
  20-line hand-rolled logger (levels, threshold, `[LEVEL]` prefix,
  `IsTerminal` for TTY detection, `NO_COLOR` present-check via
  `std::env::var_os("NO_COLOR").is_some()`). Everything to stdout, including
  errors. Per-request debug line via a small middleware:
  `debug!("{method} {path} → {status}")` after the inner service resolves.

**reqwest** (context for the other spec files): the MAM timeout
(`MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS`, float seconds) maps to
`reqwest::ClientBuilder::timeout(Duration::from_secs_f64(n))`; it exists to
keep the updater from hanging when the VPN is down.
