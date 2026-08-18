#!/usr/bin/env bash
# Parity harness: boots the Bun image and the Rust binary side by side with
# identical config and diffs their answers, probe by probe. Volatile bits
# (session ids, timestamps, host:port echoes) are masked before comparing.
# Run wherever docker can reach both: `bash parity.sh`.
set -u

# edge = tip of master, which is what the Rust backend was specified against
# (latest/0.5.0 predates e.g. the no-auth mutual-exclusion check).
BUN_IMG="${BUN_IMG:-tmmrtn/mousehole:edge}"

# The Rust side runs from an image when one is available (the low-effort
# path: `docker build -f Dockerfile.rust -t mousehole-rs .` then this script),
# and falls back to the dev-loop binary in the build.sh cargo volume.
RUST_IMG="${RUST_IMG:-mousehole-rs:latest}"
RUST_BIN_VOLUME="${RUST_BIN_VOLUME:-mousehole-cargo-target}"
if docker image inspect "$RUST_IMG" >/dev/null 2>&1; then
  RUST_MODE="image"
  rust_docker_args() { echo "$RUST_IMG"; }
else
  RUST_MODE="volume"
  rust_docker_args() {
    echo "-v $RUST_BIN_VOLUME:/t:ro --entrypoint /t/mousehole-musl-latest alpine:latest"
  }
fi
echo "rust side: $RUST_MODE mode"
BUN_PORT=5012
RUST_PORT=5011
PASS=0; FAIL=0; FAILED_NAMES=()

cleanup() {
  docker rm -f parity-bun parity-rust >/dev/null 2>&1
}
trap cleanup EXIT
cleanup

COMMON_ENV=(-e MOUSEHOLE_AUTH_PASSWORD=pw -e MOUSEHOLE_AUTH_TOKEN=tok123 -e TZ=America/Chicago)

docker run -d --name parity-bun "${COMMON_ENV[@]}" -p 127.0.0.1:$BUN_PORT:5010 "$BUN_IMG" >/dev/null
# shellcheck disable=SC2046
docker run -d --name parity-rust "${COMMON_ENV[@]}" \
  -p 127.0.0.1:$RUST_PORT:5010 $(rust_docker_args) >/dev/null
sleep 6   # let both finish their startup contact

normalize() {
  # masks: session ids, timestamps, our two ports, ANSI color, and the
  # engine-specific wording inside a json-parse cause (JavaScriptCore vs
  # serde phrase malformed JSON differently — declared in the itinerary)
  sed -E \
    -e 's/mousehole-session=[A-Za-z0-9_-]{43}/mousehole-session=SID/g' \
    -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+[+-][0-9]{2}:[0-9]{2}\[[^]]+\]/TS/g' \
    -e "s/127\.0\.0\.1:(5011|5012)/HOSTPORT/g" \
    -e "s/localhost:(5011|5012)/HOSTPORT/g" \
    -e $'s/\x1b\\[[0-9;]*m//g' \
    -e 's/"cause":\{"type":"unhandled-error","message":"[^"]*"\}/"cause":CAUSE/g'
}

# probe <name> <curl args relative to base...>
# Captures status, a fixed header subset, and the body; diffs normalized.
probe() {
  local name="$1"; shift
  local out_bun out_rust
  out_bun=$(capture "http://127.0.0.1:$BUN_PORT" "$@")
  out_rust=$(capture "http://127.0.0.1:$RUST_PORT" "$@")
  if [ "$out_bun" = "$out_rust" ]; then
    PASS=$((PASS+1)); echo "  ok: $name"
  else
    FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
    echo "FAIL: $name"
    diff <(echo "$out_bun") <(echo "$out_rust") | sed 's/^/      /' | head -12
  fi
}

capture() {
  local base="$1"; shift
  local path="$1"; shift
  local raw
  raw=$(curl -s -i --max-time 15 "$@" "$base$path")
  {
    echo "$raw" | head -1 | awk '{print "STATUS", $2}'
    echo "$raw" | tr -d '\r' | awk 'BEGIN{h=1} /^$/{h=0} h' | \
      grep -iE '^(www-authenticate|location|set-cookie|content-type):' | \
      tr 'A-Z' 'a-z' | sed -E 's/location: https?:\/\/[^/]*/location: /' | sort
    echo "BODY $(echo "$raw" | tr -d '\r' | awk 'BEGIN{h=1} /^$/{h=0; next} !h')"
  } | normalize
}

echo "== routing & content negotiation =="
probe "root default"            "/" -o /dev/null -w 'STATUS %{http_code} LOC %{redirect_url}\n'
probe "root accepts json"       "/" -H "Accept: application/json" -o /dev/null -w 'STATUS %{http_code} LOC %{redirect_url}\n'
probe "unknown path 404"        "/nope"
probe "method mismatch is 404"  "/state" -X POST
probe "HEAD health"             "/health" -I
probe "health"                  "/health"

echo "== auth ladder =="
probe "state unauthenticated"   "/state"
probe "state basic-auth junk"   "/state" -H "Authorization: Basic x"
probe "state stale cookie"      "/state" -H "Cookie: mousehole-session=stale-session-id"
probe "state lowercase bearer"  "/state" -H "Authorization: bearer tok123"
probe "state trailing-space token fails" "/state" -H "Authorization: Bearer tok123 "

echo "== login / logout =="
probe "login no content-type"   "/login" -X POST --data-raw '{"password":"pw"}'
probe "login bad json"          "/login" -X POST -H "Content-Type: application/json" --data-raw '{nope'
probe "login schema"            "/login" -X POST -H "Content-Type: application/json" --data-raw '{}'
probe "login wrong password"    "/login" -X POST -H "Content-Type: application/json" --data-raw '{"password":"wrong"}'
probe "login success"           "/login" -X POST -H "Content-Type: application/json" --data-raw '{"password":"pw"}'
probe "logout without cookie"   "/logout" -X POST

# session round-trip needs real (unmasked) cookies, one per backend
session_roundtrip() {
  local base="$1"
  local sid
  sid=$(curl -s -i --max-time 15 -X POST -H "Content-Type: application/json" \
        --data-raw '{"password":"pw"}' "$base/login" | tr -d '\r' | \
        grep -oE 'mousehole-session=[A-Za-z0-9_-]{43}' | head -1)
  local with; with=$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: $sid" "$base/state")
  local lo;   lo=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: $sid" "$base/logout")
  local after; after=$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: $sid" "$base/state")
  echo "state-with-session=$with logout=$lo state-after-logout=$after"
}
A=$(session_roundtrip "http://127.0.0.1:$BUN_PORT")
B=$(session_roundtrip "http://127.0.0.1:$RUST_PORT")
if [ "$A" = "$B" ]; then PASS=$((PASS+1)); echo "  ok: session round-trip ($A)"; else FAIL=$((FAIL+1)); FAILED_NAMES+=("session round-trip"); echo "FAIL: session round-trip: bun[$A] rust[$B]"; fi

echo "== boundary precedence =="
BIG=$(head -c 9000 /dev/zero | tr '\0' 'a')
probe "oversized body beats auth" "/cookie" -X PUT -H "Content-Type: application/json" --data-raw "$BIG"
probe "evil host"               "/state" -H "Host: evil.example" -H "Authorization: Bearer tok123"
probe "default host any port"   "/state" -H "Host: localhost:9999" -H "Authorization: Bearer tok123"
probe "origin evil with token passes" "/updates" -X POST -H "Origin: https://evil.example" -H "Authorization: Bearer tok123"
probe "origin evil with session fails" "/logout" -X POST -H "Origin: https://evil.example" -H "Cookie: mousehole-session=x"
probe "origin null same-origin mode" "/logout" -X POST -H "Origin: null"
probe "no origin passes"        "/logout" -X POST
probe "cross-origin GET state has no origin check" "/state" -H "Origin: https://evil.example" -H "Authorization: Bearer tok123"

echo "== PUT /cookie error ladder =="
probe "cookie bad json"         "/cookie" -X PUT -H "Authorization: Bearer tok123" -H "Content-Type: application/json" --data-raw '{nope'
probe "cookie empty value"      "/cookie" -X PUT -H "Authorization: Bearer tok123" -H "Content-Type: application/json" --data-raw '{"value":""}'
probe "cookie empty body"       "/cookie" -X PUT -H "Authorization: Bearer tok123" -H "Content-Type: application/json" --data-raw ''
probe "cookie non-object body"  "/cookie" -X PUT -H "Authorization: Bearer tok123" -H "Content-Type: application/json" --data-raw '"scalar"'
probe "cookie no content-type"  "/cookie" -X PUT -H "Authorization: Bearer tok123" --data-raw '{"value":"x"}'

echo "== static =="
probe "web index"               "/web" -o /dev/null -w 'STATUS %{http_code} TYPE %{content_type}\n'
probe "web trailing slash"      "/web/" -o /dev/null -w 'STATUS %{http_code} TYPE %{content_type}\n'
probe "web miss is json 404"    "/web/definitely-missing.xyz"
# Asset filenames are content-hashed and the two backends may carry builds
# with different PUBLIC_GIT_HASH inlined, so each is asked for an asset from
# ITS OWN index.html — parity is "serves its bundle with the right type".
asset_check() {
  local base="$1"
  local asset
  asset=$(curl -s --max-time 10 "$base/web" | grep -oE '/web/assets/[^"]+\.js' | head -1)
  [ -n "$asset" ] || { echo "no-asset-found"; return; }
  curl -s -o /dev/null --max-time 10 -w '%{http_code} %{content_type}' "$base$asset"
}
AA=$(asset_check "http://127.0.0.1:$BUN_PORT")
AB=$(asset_check "http://127.0.0.1:$RUST_PORT")
if [ "$AA" = "$AB" ] && [ "$AA" != "no-asset-found" ]; then
  PASS=$((PASS+1)); echo "  ok: web hashed asset ($AA)"
else
  FAIL=$((FAIL+1)); FAILED_NAMES+=("web hashed asset")
  echo "FAIL: web hashed asset: bun[$AA] rust[$AB]"
fi

echo "== live MAM path + state file bytes (garbage cookie) =="
probe "put cookie garbage (real MAM)" "/cookie" -X PUT -H "Authorization: Bearer tok123" -H "Content-Type: application/json" --data-raw '{"value":"parity-garbage-cookie"}'
SB=$(docker exec parity-bun cat /var/lib/mousehole/state.json 2>/dev/null | normalize)
SR=$(docker exec parity-rust cat /var/lib/mousehole/state.json 2>/dev/null | normalize)
if [ "$SB" = "$SR" ] && [ -n "$SB" ]; then
  PASS=$((PASS+1)); echo "  ok: state.json byte-identical (normalized)"
else
  FAIL=$((FAIL+1)); FAILED_NAMES+=("state.json bytes")
  echo "FAIL: state.json"; diff <(echo "$SB") <(echo "$SR") | sed 's/^/      /' | head -12
fi

echo "== SSE =="
sse_test() {
  local base="$1"
  (curl -s -N --max-time 5 -H "Authorization: Bearer tok123" "$base/events" > /tmp/sse.$$ 2>/dev/null) &
  local cp=$!
  sleep 1
  curl -s -o /dev/null -X POST -H "Authorization: Bearer tok123" "$base/updates"
  wait $cp 2>/dev/null
  grep -c "^data: changed$" /tmp/sse.$$ 2>/dev/null | tr -d '\n'; rm -f /tmp/sse.$$
}
SA=$(sse_test "http://127.0.0.1:$BUN_PORT"); SBc=$(sse_test "http://127.0.0.1:$RUST_PORT")
if [ -n "$SA" ] && [ "$SA" -ge 1 ] && [ -n "$SBc" ] && [ "$SBc" -ge 1 ]; then
  PASS=$((PASS+1)); echo "  ok: SSE change frames (bun=$SA rust=$SBc)"
else
  FAIL=$((FAIL+1)); FAILED_NAMES+=("SSE frames"); echo "FAIL: SSE frames bun=$SA rust=$SBc"
fi
probe "events headers" "/events" -H "Authorization: Bearer tok123" -o /dev/null --max-time 2 -w 'TYPE %{content_type}\n'

echo "== boot & config errors (fresh one-shot containers) =="
boot_probe() {
  local name="$1"; shift
  local bun_out rust_out
  # timeout guards against a backend that (unexpectedly) boots fine and
  # would otherwise hang the harness forever
  bun_out=$(timeout 30 docker run --rm "$@" "$BUN_IMG" 2>&1 | normalize)
  # shellcheck disable=SC2046
  rust_out=$(timeout 30 docker run --rm "$@" $(rust_docker_args) 2>&1 | normalize)
  # compare just the error line — Bun wraps it in an uncaught-error dump with
  # a stack, the Rust binary logs it plainly; the message itself must match
  local b r
  b=$(echo "$bun_out"  | grep -oE '(Invalid environment variable|MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true cannot|Mousehole authentication is not configured).*' | head -1 | sed 's/ *$//')
  r=$(echo "$rust_out" | grep -oE '(Invalid environment variable|MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true cannot|Mousehole authentication is not configured).*' | head -1 | sed 's/ *$//')
  if [ -n "$b" ] && [ "$b" = "$r" ]; then
    PASS=$((PASS+1)); echo "  ok: $name"
  else
    FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
    echo "FAIL: $name"; echo "      bun:  $b"; echo "      rust: $r"
  fi
}
boot_probe "no credentials refuses to start"
boot_probe "mutual exclusion" -e MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true -e MOUSEHOLE_AUTH_PASSWORD=pw
boot_probe "invalid numeric"  -e MOUSEHOLE_PORT=eleventy -e MOUSEHOLE_AUTH_TOKEN=t

echo
echo "================================"
echo "PASS: $PASS  FAIL: $FAIL"
[ $FAIL -gt 0 ] && printf 'failed: %s\n' "${FAILED_NAMES[@]}"
exit $FAIL
