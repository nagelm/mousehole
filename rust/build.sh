#!/usr/bin/env bash
# Dev-loop build harness: runs cargo inside a rust:alpine (musl) container —
# the same environment the release image builds in, so you're always building
# the artifact you'll actually ship. Cargo registry and target dir persist in
# named volumes, so incremental checks are fast after the first run.
#
# Usage:
#   ./build.sh check           # cargo check (default)
#   ./build.sh test            # cargo test
#   ./build.sh release         # cargo build --release, binary lands in ./out/
#
# Runs against the local docker daemon by default. Set REMOTE_HOST=<ssh host>
# to build on a remote docker host instead (the source is piped over ssh).
set -euo pipefail

CMD="${1:-check}"
HERE="$(cd "$(dirname "$0")" && pwd)"
REMOTE_HOST="${REMOTE_HOST:-}"

case "$CMD" in
  check|test) CARGO="cargo $CMD" ;;
  release)    CARGO="cargo build --release" ;;
  *) echo "usage: $0 [check|test|release]" >&2; exit 2 ;;
esac

# Ship the built Vite bundle too when it exists (rust-embed needs ../dist at
# compile time; a placeholder is created otherwise so backend dev works
# before the frontend is built).
INCLUDE="rust"
[ -d "$HERE/../dist" ] && INCLUDE="rust dist"

DOCKER_CMD=(docker run --rm -i \
  -v mousehole-cargo-registry:/usr/local/cargo/registry \
  -v mousehole-cargo-target:/work/rust/target \
  -w /work rust:1-alpine sh -c "\
    apk add --no-cache musl-dev >/dev/null 2>&1 && \
    tar xf - && \
    mkdir -p dist && [ -f dist/index.html ] || printf '<!doctype html><title>mousehole placeholder</title>' > dist/index.html; \
    cd rust && $CARGO 2>&1 && \
    if [ \"$CMD\" = release ]; then cp target/release/mousehole /work/rust/target/mousehole-musl-latest; fi")

if [ -n "$REMOTE_HOST" ]; then
  tar -C "$HERE/.." -cf - --exclude=rust/target --exclude=rust/out $INCLUDE | \
    ssh "$REMOTE_HOST" "$(printf '%q ' "${DOCKER_CMD[@]}")"
else
  tar -C "$HERE/.." -cf - --exclude=rust/target --exclude=rust/out $INCLUDE | \
    "${DOCKER_CMD[@]}"
fi

if [ "$CMD" = "release" ]; then
  mkdir -p "$HERE/out"
  EXTRACT=(docker run --rm -v mousehole-cargo-target:/t alpine cat /t/mousehole-musl-latest)
  if [ -n "$REMOTE_HOST" ]; then
    ssh "$REMOTE_HOST" "$(printf '%q ' "${EXTRACT[@]}")" > "$HERE/out/mousehole"
  else
    "${EXTRACT[@]}" > "$HERE/out/mousehole"
  fi
  chmod +x "$HERE/out/mousehole"
  echo "binary: $HERE/out/mousehole"
fi
