# MAM interaction + scheduling — Rust rewrite spec

Scope: everything between Mousehole and MyAnonamouse (MAM), plus the contact
scheduler that drives it. Derived by reading the actual TypeScript source
(commit state as of 2026-08-18); file references are to the original repo.
The React frontend is reused verbatim, so every wire detail here is **normative
and exact** — field names, casing, status codes, header names, string formats.

Source files covered:

- `src/backend/contact.ts` — contact loop + scheduler
- `src/backend/external-api/mam.ts` — `dynamicSeedbox.php` client
- `src/backend/external-api/host-info.ts` — `jsonIp.php` client
- `src/backend/external-api/fetch.ts` — shared fetch wrapper (UA, timeout, redirect policy)
- `src/backend/mutex.ts` — FIFO async mutex
- `src/backend/context.ts` — wiring (one scheduler per process)
- `src/backend/handlers/health.ts`, `handlers/updates.ts`, `handlers/cookie.ts`, `handlers/state.ts`
- `src/backend/state/serde.ts`, `state/store.ts`, `state/migrate.ts` — persisted state
- `src/shared/public-state.ts` — the classification function shared with the UI
- `src/frontend/components/mam-response.tsx`, `next-update.tsx`, `dashboard.tsx` — what the UI expects
- `tests/lib/mam-test-server.ts` — the documented MAM outcome matrix

---

## 1. The two MAM endpoints

Mousehole talks to exactly two external URLs, both on `t.myanonamouse.net`:

| Purpose | URL | Cookie sent |
|---|---|---|
| IP update (cookie configured) | `https://t.myanonamouse.net/json/dynamicSeedbox.php` | `mam_id=<value>` |
| Host-info lookup (no cookie yet) | `https://t.myanonamouse.net/json/jsonIp.php` | none |

### 1.1 Shared request behavior (`fetch.ts`)

Every external request (both endpoints) is made with:

- **Method:** `GET` (no method is set; fetch defaults to GET).
- **Header `User-Agent`:** `mousehole-by-timtimtim/<version>` — version comes
  from `package.json` (`0.5.0` today). Exact current value:
  `mousehole-by-timtimtim/0.5.0`. Keep the `mousehole-by-timtimtim/` prefix in
  the rewrite; MAM staff use it to identify the client.
- **Redirects: not followed** (`redirect: "manual"`). A 3xx response is
  returned as-is; its (non-JSON) body then fails JSON parsing and the contact
  is recorded as unreachable with a `json-parse-error` (see §4).
- **Timeout:** `AbortSignal.timeout(timeoutSeconds * 1000)` where
  `timeoutSeconds` = config `mamRequestTimeoutSeconds`
  (`MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS`, positive number — fractional
  allowed — default **10**). One timeout for the whole request.
- No other headers are set (notably: no `Accept`, no `Accept-Encoding`
  override, no auth).

**Low-level error mapping** (exceptions thrown by the fetch itself):

| Failure | Error type string | HTTP status if surfaced | Message template |
|---|---|---|---|
| Abort by timeout | `timeout-error` | 504 | `Request to <url> timed out after <n>s. Is the network up?` |
| Any other fetch failure (DNS, refused, TLS, …) | `network-error` | 500 | `Network request to <url> failed` |

`<url>` is the full endpoint URL string, e.g.
`https://t.myanonamouse.net/json/dynamicSeedbox.php`. `<n>` is the configured
timeout in seconds as written (e.g. `10`).

Edge case (faithful-behavior note): in the original, the timeout signal is only
mapped to `timeout-error` when it fires during the *fetch* call. If it fires
while the body is being read (`response.text()`), the raw exception escapes the
mapping and the contact is recorded with type `unhandled-error` and the
runtime's own message. In Rust, `reqwest`'s per-request timeout covers the full
exchange; mapping every `is_timeout()` error to `timeout-error` is the sane
behavior and an acceptable (better) deviation — nothing branches on the stored
`error.type`, it is display/log only.

### 1.2 `dynamicSeedbox.php` (`mam.ts`)

Request:

```
GET /json/dynamicSeedbox.php HTTP/1.1
Host: t.myanonamouse.net
User-Agent: mousehole-by-timtimtim/0.5.0
Cookie: mam_id=<currentCookieValue>
```

The `Cookie` header is exactly `` `${cookieKey}=${currentCookieValue}` `` with
`cookieKey = "mam_id"` — a single cookie pair, no quoting, no attributes, the
stored value inserted verbatim (it is never URL-encoded on send).

The IP being "updated to" never appears in the request — MAM derives it from
the connection's source address. That is the entire point: the request must
egress via the seedbox/VPN interface.

Response body — parsed as JSON from the **full body text regardless of HTTP
status** (MAM returns this shape even on 429/403). Validated shape (zod):

```jsonc
{
  "Success": true,          // boolean, REQUIRED — note uppercase S
  "msg": "Completed",       // string, REQUIRED
  "ip": "203.0.113.7",      // string, REQUIRED, must validate as IPv4
  "ASN": 64496,             // number, REQUIRED — note all-caps key
  "AS": "TEST-AS (RFC 5737)" // string, REQUIRED — note all-caps key
}
```

- Unknown extra keys are **allowed and ignored** (zod default object
  behavior). Do not use `deny_unknown_fields` in serde.
- `ip` is validated as IPv4 specifically (`z.ipv4()`); an IPv6 literal fails
  validation → `schema-error` → contact recorded as unreachable.
- **The HTTP status is not checked before parsing.** Any status with a
  valid-shaped JSON body produces a "reached" contact whose `httpStatus` is the
  raw status code.

Result of a successful parse (internal `MamUpdateResult`):

| Field | Source |
|---|---|
| `ip` | body `ip` |
| `asn` | body `ASN` |
| `as` | body `AS` |
| `success` | body `Success` |
| `msg` | body `msg` (verbatim; **display-only, never branch on it**) |
| `httpStatus` | HTTP response status code |
| `rotatedCookie` | see §3 |

Parse failures throw (caught by the contact loop, §4):

- Body is not JSON → `json-parse-error` (message:
  `Error parsing JSON from response with status <status> and URL <url>`).
- JSON but wrong shape → `schema-error` (message:
  `Schema validation failed for data from <url>: <path>: <first-issue-message>`).

### 1.3 `jsonIp.php` (`host-info.ts`)

Used **only when no cookie is stored** — it lets a not-yet-configured install
still display the egress IP. Request: same shared behavior, **no Cookie
header**.

Unlike `dynamicSeedbox`, this call **requires a 2xx** first:
`if (!response.ok) throw new Error("Failed to fetch host IP from <url>: <status>")`.
That is a plain error (not a typed one), so if it surfaces into a stored
contact its type is `unhandled-error` with exactly that message.

Then the body is parsed as JSON and validated:

```jsonc
{
  "ip": "203.0.113.7",   // string, REQUIRED, IPv4
  "ASN": 64496,          // number, REQUIRED
  "AS": "TEST-AS (RFC 5737)" // string, REQUIRED
}
```

The live endpoint also returns a `time` field — ignored/stripped (again: allow
unknown fields). Returns `HostInfo { ip, asn, as }` (lowercased keys in the
domain shape).

---

## 2. The MAM outcome matrix (every case the UI renders)

MAM's documented `dynamicSeedbox.php` outcomes (message strings verbatim from
MAM's API docs, mirrored in `tests/lib/mam-test-server.ts`). Mousehole
**branches only on the HTTP status**; `msg` is stored and displayed verbatim
but never interpreted:

| HTTP status | `Success` | `msg` (verbatim) |
|---|---|---|
| 200 | `true` | `Completed` |
| 200 | `true` | `No change` |
| 429 | `false` | `Last change too recent` |
| 403 | `false` | `No Session Cookie` |
| 403 | `false` | `Invalid session` |
| 403 | `false` | `Invalid session - IP mismatch` |
| 403 | `false` | `Invalid session - ASN mismatch` |
| 403 | `false` | `Invalid session - Invalid Cookie` |
| 403 | `false` | `Invalid session - Other` |
| 403 | `false` | `Incorrect session type - not allowed this function` |
| 403 | `false` | `Incorrect session type - non-API session` |

### 2.1 Classification (`classify` in `shared/public-state.ts`)

The single classification function, shared by backend (health endpoint) and
frontend (status line, dashboard logic). Input is the last contact record;
precedence is exactly this order:

```text
no contact recorded            → "pending"
contact.reached == false       → "unreachable"
contact has no ipUpdate        → "no-cookie"     (a cookieless jsonIp lookup)
ipUpdate.httpStatus == 200     → "ok"
ipUpdate.httpStatus == 429     → "throttled"
anything else (403, 500, …)    → "rejected"
```

`ContactStatus` string values (appear verbatim on the wire in `/health`):
`"ok"`, `"throttled"`, `"rejected"`, `"unreachable"`, `"no-cookie"`,
`"pending"`.

### 2.2 How the UI renders each case (`mam-response.tsx`)

`describeStatus` (normative for what state must produce):

1. If `hasCookie == false` → warn tone, literal text `No cookie set`
   (regardless of any last contact).
2. Otherwise classify `lastMamContact` and render:

| Status | Tone | Text shown |
|---|---|---|
| `ok` | success | MAM `msg` verbatim, fallback `OK` |
| `throttled` | warn | MAM `msg` verbatim, fallback `Throttled` |
| `rejected` | error | MAM `msg` verbatim, fallback `Cookie rejected` |
| `unreachable` | error | `Couldn't reach MAM` (no MAM msg exists) |
| `no-cookie` | warn | `No cookie` (cookie now set but last contact predates it) |
| `pending` | warn | `Awaiting first update` |

The "MAM msg verbatim" is `lastMamContact.ipUpdate.msg` — present whenever a
cookie drove the contact, so the fallbacks for ok/throttled/rejected only show
if `msg` were absent (they are effectively dead code; keep `msg` required).

Additional UI consumers of these fields:

- Host IP / Host AS rows render only when `lastMamContact.reached == true`,
  from `ip`, `asn`, `as`.
- Dashboard (`dashboard.tsx`): `rejected` forces the cookie form open;
  `rejected` or `throttled` shows the "Need help?" section; `ok` shows the
  "you can close this page" tip.

---

## 3. Cookie rotation and persistence

After every `dynamicSeedbox` response (any status), Mousehole scans the
response's `Set-Cookie` headers:

- All `Set-Cookie` headers are parsed (`response.headers.getSetCookie()` +
  `set-cookie-parser`), and the **first** cookie named `mam_id` wins.
- Its **value** becomes `rotatedCookie`. Note: `set-cookie-parser` runs with
  its default `decodeValues: true`, i.e. the value is **percent-decoded**
  (`decodeURIComponent`) before storage. The stored value is later sent back
  raw (§1.2). Replicate: decode on receipt, send verbatim.
- Cookie attributes (Path, Expires, etc.) are ignored entirely.

Persistence rule (in `contactMam`):

```text
new stored cookie = rotatedCookie ?? previous cookie
```

Rotation is applied **regardless of outcome status** — if MAM ever sent a
`mam_id` Set-Cookie alongside a 429/403, it would still be persisted. On a
transport error (§4) the previous cookie is preserved unchanged.

---

## 4. The contact procedure (`contactMam` in `contact.ts`)

A "contact" always reaches out to MAM. Exact algorithm, given the previous
state (which may be absent):

1. `cookie := prevState?.cookie`.
2. `at := now` — captured **before** any network I/O, as a zoned timestamp in
   the host system's IANA timezone (§8).
3. **No cookie stored** — the branch condition is JS-falsy `!cookie`, so an
   **empty-string cookie on disk counts as "no cookie"** (the disk schema
   permits `""`; `PUT /cookie` can't create one, but a hand-edited file can).
   Consequently `hasCookie` is `Boolean(cookie)` = `false` for `""` too.
   Downstream effect: a *successful* cookie-less contact returns a state with
   no `cookie` key at all — an empty-string cookie is silently dropped from
   disk on the next successful contact; a *failed* contact (step 5) carries
   `cookie` forward verbatim, `""` included. Call `getHostInfo` (jsonIp). On
   success, log
   `No cookie set yet. Visit the web UI to configure.` and return state:

   ```jsonc
   {
     // no "cookie" key
     "lastMamContact": {
       "at": "<RFC 9557>", "reached": true,
       "ip": "...", "asn": 0, "as": "..."
       // NO "ipUpdate" key — this is what classifies as "no-cookie"
     }
   }
   ```

4. **Cookie stored:** call `updateMamIp` (dynamicSeedbox). On success
   (transport + parse level — including 429/403 outcomes):
   - Log an IP/ASN change if the previous contact was `reached` and its
     `ip`/`asn` differ from the new ones:
     `Network change: IP <old> -> <new>, ASN <old> -> <new>` (only the changed
     parts, comma-joined).
   - If `result.success` → info log `MAM update: <msg>`;
     else → error log `MAM update not applied (<httpStatus>): <msg>`.
   - Return state:

     ```jsonc
     {
       "cookie": "<rotatedCookie ?? cookie>",
       "lastMamContact": {
         "at": "<RFC 9557>", "reached": true,
         "ip": "...", "asn": 0, "as": "...",
         "ipUpdate": { "success": true, "msg": "Completed", "httpStatus": 200 }
       }
     }
     ```

5. **Any throw** from steps 3–4 (network, timeout, non-JSON body, schema
   mismatch, jsonIp non-2xx) is caught — *never propagated*:
   - Convert to `{type, message}` via the error mapping (§1.1 / §1.2 / §1.3).
     Only `type` and `message` are kept — schema issues / cause chains are
     dropped from stored state.
   - Error-log `Could not reach MAM: <message>`.
   - Return state:

     ```jsonc
     {
       "cookie": "<previous cookie, unchanged>",   // absent if there was none
       "lastMamContact": {
         "at": "<RFC 9557>", "reached": false,
         "error": { "type": "timeout-error", "message": "Request to ... timed out after 10s. Is the network up?" }
       }
     }
     ```

Possible stored `error.type` values: `network-error`, `timeout-error`,
`json-parse-error`, `schema-error`, `unhandled-error`.

Note the returned state fully **replaces** the previous state — `lastMamContact`
is overwritten every contact; only the cookie carries forward.

---

## 5. The scheduler (`createContactScheduler`)

One scheduler per process (created in `context.ts`). Configuration:

- `intervalSeconds` = config `updateIntervalSeconds`
  (`MOUSEHOLE_UPDATE_INTERVAL_SECONDS`, positive number, fractional allowed,
  default **300** = 5 minutes).
- `mamRequestTimeoutSeconds` — see §1.1.

### 5.1 `commitContact(newCookie?)` — the single entry point

Every contact in the system goes through this one function: **startup, the
interval timer, `POST /updates`, and `PUT /cookie`**. Exact sequence, all under
one mutex hold:

```text
acquire mutex (FIFO)
try:
    diskState := stateFile.readIfExists()          // may throw → propagates
    base := newCookie == None ? diskState
                              : diskState with cookie replaced by newCookie
    state := contactMam(base)                       // never throws (§4)
    stateFile.write(state)                          // may throw → propagates
    notifyClients()                                 // SSE "data: changed\n\n" to all clients
    return state
finally:
    scheduleNext()                                  // ALWAYS reschedules, even on throw
    release mutex
```

Subtleties that must be preserved:

- **State is re-read from disk on every contact** — the file is the source of
  truth, not memory.
- `newCookie` (from `PUT /cookie`) replaces the cookie *before* contacting
  MAM, inside the same locked section — so the very next request uses it, and
  a concurrent background tick can't interleave.
- `contactMam` cannot throw; only the state-file read/write can. Those errors
  propagate to the caller: an HTTP caller turns them into a 500 JSON error
  body; the background timer path just logs them
  (`handleBackgroundContactError` → `logger.error`).
- `scheduleNext()` runs in `finally` — even a failed write re-arms the timer.
- `scheduleNext()` runs **before** `commitContact` resolves, so the
  `PublicState` built from its return value (for `POST /updates` /
  `PUT /cookie` responses) already contains the **new** `nextContactAt`. The
  UI depends on this: the countdown/donut re-mounts keyed on
  `state.nextContactAt` (see §8.2).
- `notifyClients()` fires only after a successful persist.

### 5.2 `scheduleNext()` — the timer

```text
cancel any pending timer
if stopped: clear task record; return
arm a one-shot timer for intervalSeconds*1000 ms from now
  → on fire: commitContact() (errors logged, never crash)
nextContactAt := now + intervalSeconds      // informational; computed at arm time
log: "Next automatic update scheduled for <nextContactAt RFC 9557>"
```

Normative details:

- **Fixed interval. There is NO jitter.** Do not add randomization in the
  rewrite.
- **There is NO rate-limit backoff.** A 429 ("throttled") outcome is recorded
  and displayed but the next contact is scheduled exactly `intervalSeconds`
  later, identical to every other outcome. Likewise 403 does not stop or slow
  the loop — it keeps retrying at the same fixed cadence forever.
- Because *every* `commitContact` reschedules, a manual `POST /updates` or
  `PUT /cookie` **resets the countdown**: the next automatic contact is a full
  interval after the manual one.
- The interval measures from *end* of one contact to *start* of the next (the
  timer is armed after the contact completes), so wall-clock spacing =
  interval + contact duration.
- `nextContactAt` is a best-effort prediction (comment in source: informational
  only), exposed to clients via `GET /state` / `POST /updates` / `PUT /cookie`
  responses as an RFC 9557 string; `getNextContactAt()` returns `None` before
  the first contact completes and after `stop()`.
- The Node timer is `unref()`d — an armed timer alone must not keep the
  process alive (in tokio this is automatic; a spawned sleep task doesn't
  block shutdown, but see §5.4).

### 5.3 `start()`

Fire-and-forget: kick off `commitContact()` immediately (errors logged, not
awaited — the HTTP listener starts serving before the first contact resolves),
then log
`Background update task started, running on <intervalSeconds> second interval`.
Startup ordering in `server.ts`: bind listener → log banner → validate security
config → `contacts.start()`.

Consequence to preserve: a `GET /state` served before the first contact
completes has `nextContactAt` absent and `lastMamContact` = whatever was
persisted by the previous run (or absent on first install → UI shows
"Awaiting first update").

### 5.4 `stop()`

```text
stopped := true
cancel pending timer; clear task record
await mutex.acquire()      // and never release: drains any in-flight contact
```

After `stop()` resolves, no contact is running and none can be scheduled
(`scheduleNext` no-ops once stopped). Contacts already queued on the mutex
*ahead* of stop still run to completion first. (A `commitContact` that queues
*behind* stop's acquire would wait forever — unreachable in practice because
the server has stopped accepting requests; in Rust, prefer a
`CancellationToken` + draining the task, with the same observable guarantees:
in-flight contact finishes, timer never refires.)

### 5.5 Triggers for an immediate contact — summary

| Trigger | Cookie override | Result surfaced as |
|---|---|---|
| Process startup (`start()`) | no | log only (state persisted + SSE) |
| Interval timer fire | no | log only (state persisted + SSE) |
| `POST /updates` | no | 200 + full `PublicState` JSON |
| `PUT /cookie` (body `{"value": "<cookie>"}`) | yes — replaces before contact | 200 + full `PublicState` JSON |

All four paths also: persist state, notify SSE clients, reset the interval
timer.

---

## 6. Mutex semantics (`mutex.ts`)

A minimal asynchronous mutex built on a promise chain:

- `acquire()` returns a `release` closure. Waiters are served **strictly
  FIFO** (each acquire chains on the previous holder's promise).
- Non-reentrant; no timeout; no poisoning (a throw inside the critical section
  must still release — the original does so via `finally`).
- Used for exactly one thing: serializing the read → contact → write →
  notify → reschedule transaction so concurrent triggers (timer vs. HTTP)
  can't interleave or lose writes.

Rust: `tokio::sync::Mutex<()>` is FIFO-fair and gives the same semantics with
RAII guards; hold the guard across the await points of the whole transaction.

---

## 7. Outcome → health endpoint and stored state

### 7.1 `GET /health` (`handlers/health.ts`)

- **No auth, no host allowlist, no origin check** — it is the one public
  route (mounted without any boundary middleware).
- Reads the state **from disk** on every request (`readIfExists`), classifies
  `lastMamContact` (§2.1) and responds `200` with exactly:

  ```json
  { "lastMamContactResult": "ok" }
  ```

  where the value is one of the six `ContactStatus` strings.
- It returns 200 even for `"unreachable"`/`"rejected"` — monitors must check
  the JSON value, not the status code. A 200 means only "server up and state
  readable".
- If the state file exists but cannot be read/parsed, the read throws and the
  global error handler responds 500 with the standard error body
  (`{"type": "...", "message": "...", ...}`).

### 7.2 Stored state file

`<stateDir>/state.json` (`stateDir` = `MOUSEHOLE_STATE_DIR_PATH`, default
`/var/lib/mousehole`). Written pretty-printed with 2-space indent, atomically:
write `state.json.tmp` then rename over `state.json`; parent directory created
`mkdir -p` style first. Exact serialized shape (version is **2**):

```jsonc
{
  "version": 2,
  "cookie": "…",                       // optional; omitted when never set
  "lastMamContact": {                  // optional
    "at": "2026-08-18T09:15:30.123-05:00[America/Chicago]",  // RFC 9557
    "reached": true,
    "ip": "203.0.113.7",
    "asn": 64496,
    "as": "TEST-AS (RFC 5737)",
    "ipUpdate": {                      // optional: only for cookie-driven contacts
      "success": true,
      "msg": "Completed",
      "httpStatus": 200
    }
  }
}
```

or, unreached:

```jsonc
{
  "version": 2,
  "cookie": "…",
  "lastMamContact": {
    "at": "…",
    "reached": false,
    "error": { "type": "network-error", "message": "Network request to https://t.myanonamouse.net/json/dynamicSeedbox.php failed" }
  }
}
```

Read semantics (`store.ts`):

- Missing file (ENOENT) → fresh install (`None`). **Only** a missing file;
  any other IO error, JSON parse failure or schema failure must **throw**, not
  be treated as empty — otherwise the next contact would overwrite a real
  state (and its cookie) with a cookieless one.
- Read validation is structural only (strings/numbers/booleans in the right
  places, discriminated on `reached`); it does **not** re-validate IPv4 etc.
  Never make the read schema stricter than the write path — previously
  persisted states must stay readable. Unknown extra keys: zod's default
  strips them without error; serde should likewise tolerate them.
- **Migration** (`migrate.ts`): if the parsed JSON's `version` field is not
  exactly `2`, migration is *deliberately lossy*: the only thing salvaged is a
  legacy cookie, looked up under the key `currentCookie` (v1's name), kept only
  if a non-empty string. Result becomes
  `{"version": 2, "cookie": <found or omitted>}` — `lastMamContact` is
  discarded (regenerated on next contact). The candidate is then validated
  normally; failure → `schema-error` (500).

### 7.3 The wire shape (`PublicState`)

Returned by `GET /state`, `PUT /cookie`, `POST /updates` — this doc includes it
because the contact/scheduler fields flow through it:

```jsonc
{
  "hasCookie": true,                  // Boolean(state.cookie) — cookie value itself NEVER leaves the server
  "hasAuth": true,                    // auth.type == "configured"
  "nextContactAt": "2026-08-18T09:20:30.456-05:00[America/Chicago]",  // optional
  "lastMamContact": { /* SerializedMamContact — same shape as on disk, §7.2 */ }
}
```

`nextContactAt` is `getNextContactAt()?.toString()` — absent before the first
contact finishes. `lastMamContact` is serialized identically to disk (minus the
`version` wrapper).

---

## 8. Time handling and frontend timing expectations

### 8.1 Timestamp format — RFC 9557 with timezone annotation

`at` and `nextContactAt` are produced by
`Temporal.Now.zonedDateTimeISO(<system IANA timezone>).toString()`:
ISO 8601 date-time **with UTC offset AND a bracketed IANA timezone
annotation**, e.g.

```
2026-08-18T09:15:30.123-05:00[America/Chicago]
```

The frontend parses these with `Temporal.ZonedDateTime.from(...)` (polyfill),
which **requires** the RFC 9557 form — a bare ISO string without the
`[Zone]` suffix will throw in the UI. The Rust side must emit the same format
(see Rust notes: `jiff::Zoned` does exactly this).

### 8.2 What the countdown UI assumes (`next-update.tsx`, `mam-response.tsx`, `dashboard.tsx`)

- The "Next update" row renders only when **both** `nextContactAt` and
  `lastMamContact.at` are present. The pair defines the wait window:
  `at` = start, `nextContactAt` = end.
- The status card is keyed on `state.nextContactAt` — a fresh value after
  every contact re-mounts the card and restarts the depletion donut. Backend
  obligation: **every contact must produce a new, distinct `nextContactAt`**
  (guaranteed today: it's recomputed as now+interval at millisecond
  precision on every reschedule).
- The countdown ticks locally every second showing
  `nextContactAt − now`, clamped at 0 (it just sits at `00:00` if the server
  is late; the SSE `changed` signal then delivers the fresh state). The full
  window size `nextContactAt − at` only selects the display units
  (`SS` / `MM:SS` / `HH:MM:SS`) so the width stays stable.
- Degenerate/inverted windows (`nextContactAt ≤ at`, e.g. clock skew) are
  tolerated: empty ring, no animation. Don't sweat exactness, but don't emit
  garbage.
- The donut animation assumes `at ≤ now ≤ nextContactAt` roughly holds in the
  *client's* clock; the server's only duty is emitting honest values from one
  monotonic-ish source (`getNowZdt` for both).

### 8.3 SSE contract

After every persisted contact the backend broadcasts, to every connected
`/events` client, the exact frame:

```
data: changed\n\n
```

The payload is deliberately contentless — clients respond by re-fetching
`GET /state`. Response headers on `/events`:
`Content-Type: text/event-stream`, `Cache-Control: no-cache`,
`Connection: keep-alive`; the stream stays open indefinitely (idle timeouts
disabled server-side).

---

## 9. Log lines (exact formats, for parity)

| Event | Level | Format |
|---|---|---|
| No cookie configured | info | `No cookie set yet. Visit the web UI to configure.` |
| Update applied | info | `MAM update: <msg>` |
| Update not applied | error | `MAM update not applied (<httpStatus>): <msg>` |
| Transport failure | error | `Could not reach MAM: <message>` |
| IP/ASN change | info | `Network change: IP <old> -> <new>, ASN <old> -> <new>` (only changed parts, `", "`-joined) |
| Timer armed | info | `Next automatic update scheduled for <RFC 9557>` |
| Loop started | info | `Background update task started, running on <n> second interval` |

The IP/ASN change comparison uses the previous contact only if it was
`reached: true`; a first-ever or previously-unreachable contact logs nothing.

---

## 10. Rust implementation notes (axum / tokio / reqwest / serde)

**HTTP client (`reqwest`)**

- One shared `reqwest::Client` built with
  `.redirect(redirect::Policy::none())` (≙ `redirect: "manual"`),
  `.user_agent(format!("mousehole-by-timtimtim/{}", env!("CARGO_PKG_VERSION")))`,
  and **no cookie store** (`cookie_store(false)`, the default) — cookie
  handling is manual and must stay manual (§3).
- Per-request `.timeout(Duration::from_secs_f64(mam_request_timeout_seconds))`
  — config values are fractional-capable floats, keep `f64` seconds.
- Send the cookie with
  `.header(reqwest::header::COOKIE, format!("mam_id={value}"))`.
- Read the body with `resp.text().await` **before** JSON-parsing so the
  error can distinguish `json-parse-error` (serde_json from string) from
  transport errors; capture `resp.status()` first.
- Set-Cookie scan: `resp.headers().get_all(SET_COOKIE)` → for each, parse with
  the `cookie` crate (`Cookie::parse`) → first with `name() == "mam_id"` →
  take `value()`, then **percent-decode** it (e.g. `percent-encoding` crate's
  `percent_decode_str(...).decode_utf8()`) to match `set-cookie-parser`'s
  `decodeValues: true` default.
- Error mapping: `e.is_timeout()` → `timeout-error` (504); other reqwest
  errors → `network-error` (500). Keep the exact message templates of §1.1.

**Response types (`serde`)**

```rust
#[derive(Deserialize)]              // NOTE: no deny_unknown_fields — MAM may add keys
struct DynamicSeedboxBody {
    #[serde(rename = "Success")] success: bool,
    msg: String,
    ip: String,                     // validate as std::net::Ipv4Addr after parse
    #[serde(rename = "ASN")] asn: i64,
    #[serde(rename = "AS")] r#as: String,
}
```

Validate `ip.parse::<Ipv4Addr>()` separately so the failure maps to
`schema-error`, not a serde error. Same pattern (minus `Success`/`msg`) for
`jsonIp.php` (`ip`/`ASN`/`AS`, ignore `time`).

**State & wire serialization**

- `MamContact` as a tagged-by-bool shape: serde can model it with
  `#[serde(untagged)]` over two structs, or a single struct with `reached:
  bool` + `#[serde(skip_serializing_if = "Option::is_none")]` optionals —
  whichever you choose, the emitted JSON must match §7.2 **exactly**,
  including omitting absent keys (`cookie`, `ipUpdate`, `nextContactAt`)
  rather than emitting `null`.
- Persist with `serde_json::to_string_pretty` (2-space indent — matches
  `JSON.stringify(_, undefined, 2)`), write to `state.json.tmp`, then
  `tokio::fs::rename` over `state.json`; `create_dir_all` the state dir first.
- Distinguish ENOENT (`e.kind() == ErrorKind::NotFound` → fresh install) from
  every other read error (must fail the request/log, never silently reset).

**Timestamps (`jiff`)**

- Use the `jiff` crate: `jiff::Zoned` round-trips RFC 9557
  (`2026-08-18T09:15:30.123-05:00[America/Chicago]`) via `Display`/`FromStr`
  — exactly what the frontend's Temporal polyfill consumes/produces. Get "now"
  with `Zoned::now()` (system timezone), matching `getNowZdt()`.
- `chrono` cannot emit the bracketed zone annotation; do not use it for these
  fields.

**Scheduler (tokio)**

- Model the scheduler as a struct owning:
  `Mutex<()>` (tokio, FIFO-fair) for the contact transaction, an
  `RwLock<Option<(JoinHandle<()>, Zoned)>>` (or `AbortHandle`) for the armed
  timer + its informational `next_contact_at`, and a `CancellationToken` (or
  `AtomicBool`) for `stopped`.
- `schedule_next`: abort the previous sleep task, then
  `tokio::spawn(async { sleep(interval).await; commit_contact().await })` —
  storing the `JoinHandle`; compute and store `next_contact_at = now +
  interval` at arm time (informational only; do not derive it from the sleep's
  actual deadline).
- `commit_contact`: take the tokio `Mutex` guard for the whole
  read→contact→write→notify sequence; run `schedule_next` + guard-drop in the
  equivalent of `finally` (i.e. run `schedule_next` on both success and error
  paths before returning — a small `scopeguard` or explicit match works).
  Ensure `schedule_next` happens **before** the caller builds the
  `PublicState` response so `nextContactAt` is fresh (§5.1).
- `start`: spawn `commit_contact` without awaiting; then the server begins
  serving (or vice versa — the original binds the listener first; either way
  do not block startup on the first contact).
- `stop`: cancel token → abort the armed sleep → acquire the mutex once (and
  hold it / or await the in-flight task) to drain. Interval fires and manual
  triggers must both go through the same `commit_contact`.
- **No jitter, no backoff, no retry** — resist the idiom. The fixed cadence,
  the reset-on-manual-update, and the keep-retrying-on-403 behavior are all
  load-bearing for UI expectations and MAM-side semantics.

**Health / axum**

- `GET /health`: mount **outside** the auth/host/origin middleware layers.
  Handler: read state from disk, `classify`, respond
  `Json(json!({"lastMamContactResult": status_str}))`. Implement `classify`
  once, unit-matched to §2.1's precedence, and reuse it (there is exactly one
  classification function in the original, shared with the UI — keep the Rust
  one the single backend authority).
- Contact-scheduler errors surfacing through `POST /updates` / `PUT /cookie`
  become the standard error body `{"type": ..., "message": ..., ...}` with
  the mapped status (500 for file errors, 504 timeout, etc.) — see the
  companion HTTP API spec for the full boundary; classification of *transport*
  failures never produces an HTTP error on these routes (they're recorded
  in state and returned as a normal 200 `PublicState`).

**SSE**

- After each successful persist, send `data: changed\n\n` to all registered
  streams (axum: `Sse` responses fed from a `tokio::sync::broadcast` channel;
  a lagging/closed receiver just gets dropped, matching the original's
  drop-on-enqueue-failure). Disable/raise any idle timeouts so quiet streams
  survive (the original sets Bun's `idleTimeout: 0`).
