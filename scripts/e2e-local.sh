#!/usr/bin/env bash
# Run the whole e2e suite LOCALLY, sharded in parallel, on this machine's cores.
#
# WHY: a CI shard spends ~13 min, and four of them run the suite once. A
# developer box with real cores can do the same work without spending anyone's
# CI minutes — but only if the shards do not fight over ports. Each project
# binds a fixed port (18823/18833/18843), so two concurrent runs collided and
# the second died in webServer startup. AMUX_E2E_PORT_OFFSET (playwright.config.ts)
# gives each shard its own port block; homes were already per-run (mkdtemp).
#
# Usage:
#   scripts/e2e-local.sh                  # shards = cores/4 (min 2, max 8), 2 workers each
#   scripts/e2e-local.sh -s 8 -w 3        # explicit shards / workers-per-shard
#   scripts/e2e-local.sh -- e2e/board-activity-scrolls.spec.ts   # pass through to playwright
#
# Exit status is non-zero if ANY shard failed. Per-shard logs land in
# $OUT/shard-N.log and the summary names each one, because a single merged
# stream from parallel shards is unreadable and hides which shard failed.
set -uo pipefail
cd "$(dirname "$0")/.."

CORES=$(nproc 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || echo 4)
SHARDS=$(( CORES / 4 )); [ "$SHARDS" -lt 2 ] && SHARDS=2; [ "$SHARDS" -gt 8 ] && SHARDS=8
WORKERS=2
OUT="${AMUX_E2E_LOCAL_OUT:-${TMPDIR:-/tmp}/amux-e2e-local-$(date +%Y%m%d-%H%M%S)}"

while [ $# -gt 0 ]; do
  case "$1" in
    -s|--shards)  SHARDS="$2"; shift 2 ;;
    -w|--workers) WORKERS="$2"; shift 2 ;;
    -o|--out)     OUT="$2"; shift 2 ;;
    -h|--help)    sed -n '2,20p' "$0"; exit 0 ;;
    --)           shift; break ;;
    *)            break ;;
  esac
done

# One playwright run at a time per port block. A stray run from another shell
# owns ports this one would take, and the failure would look like a flaky
# webServer timeout rather than a collision.
if pgrep -f "playwright test" >/dev/null 2>&1; then
  echo "refusing: a playwright run is already active (pgrep -f 'playwright test')." >&2
  echo "  Wait for it, or pass a different --out and offsets by hand." >&2
  exit 2
fi

mkdir -p "$OUT"
echo "e2e local: $SHARDS shards x $WORKERS workers on $CORES cores -> $OUT"
[ $# -gt 0 ] && echo "  playwright args: $*"

# Build ONCE before the shards start. They share one CARGO_TARGET_DIR, so
# without this the first shard builds and the rest sit on cargo's lock — the
# same wall-clock cost, spent looking like a hung webServer.
echo "prebuild: cargo build -p amux-server (shared target dir)"
if ! CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}"      cargo build -p amux-server > "$OUT/prebuild.log" 2>&1; then
  echo "prebuild FAILED — see $OUT/prebuild.log" >&2
  tail -20 "$OUT/prebuild.log" >&2
  exit 1
fi

pids=()
for i in $(seq 1 "$SHARDS"); do
  # 100 ports apart: three projects per shard, plus room for the browser's own
  # ephemeral listeners, so blocks cannot overlap.
  offset=$(( i * 100 ))
  AMUX_E2E_WORKING_TREE=1 AMUX_E2E_PORT_OFFSET="$offset" \
    npx playwright test --config e2e/playwright.config.ts \
      --shard="$i/$SHARDS" --workers="$WORKERS" --reporter=line "$@" \
      > "$OUT/shard-$i.log" 2>&1 &
  pids+=("$!")
done

rc=0
for i in $(seq 1 "$SHARDS"); do
  if wait "${pids[$((i-1))]}"; then
    printf 'shard %s/%s  PASS  %s\n' "$i" "$SHARDS" "$(grep -aE '^ *[0-9]+ (passed|skipped)' "$OUT/shard-$i.log" | tail -1)"
  else
    rc=1
    printf 'shard %s/%s  FAIL  %s\n' "$i" "$SHARDS" "$(grep -aE '^ *[0-9]+ (failed|passed)' "$OUT/shard-$i.log" | tail -1)"
    grep -aE '^ *[0-9]+\) ' "$OUT/shard-$i.log" | sed 's/^/    /' | head -10
  fi
done

echo "logs: $OUT"
[ "$rc" -eq 0 ] && echo "e2e local: all $SHARDS shards passed" || echo "e2e local: at least one shard failed"
exit "$rc"
