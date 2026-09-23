#!/usr/bin/env bash
# AF-697: the server's own cross-board-reassignment refusal, and this CLI's own
# help text, both name `shepherd` as a peer hand-off escape hatch on par with
# `reviewer` -- but no CLI verb existed to set it, even though `shepherd` is a
# fully PATCH-able server field. A how_to_fix naming a verb that does not exist
# sends a caller straight at a raw, unattributed PATCH (ethos rule 6).
#
# This is a CLI-side write (the PATCH body is built in the shell), so no server
# log could catch a regression -- runs the REAL shipped verb against a MOCK
# curl, same pattern as test-needsyou-signin-link.sh. No live server needed.
set -euo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
has()  { if grep -qF -- "$2" "$1"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: $3 (missing '$2')"; fi; }

TMP=$(mktemp -d)
mkdir -p "$TMP/bin"
cat > "$TMP/bin/curl" <<'MOCK'
#!/usr/bin/env bash
body=""; is_patch=0; args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    -X) [[ "${args[$((i+1))]}" == "PATCH" ]] && is_patch=1 ;;
    -d|--data|--data-binary) body="${args[$((i+1))]}"; [[ "$body" == "@-" ]] && body="$(cat)" ;;
  esac
done
if [[ $is_patch -eq 1 ]]; then
  printf '%s\n' "$body" >> "$CAPTURE"
  echo '{"ok":true,"id":"TEST-1","shepherd":"peer-lane"}'
elif [[ "$*" == *"/api/sessions/ghost-lane"* ]]; then
  # A peer the fleet has never heard of: `_board_assignee_state` prints
  # "missing" for the server's 404 not-found body, and the guard must die.
  # The server's real 404 body (session_verbs.rs): only this shape means
  # "does not exist"; other error bodies (503/501) are "could not tell".
  echo '{"error":"session '"'"'ghost-lane'"'"' not found"}'
elif [[ "$*" == *"/api/sessions/"* ]]; then
  # `_board_assignee_guard` probes the peer before any PATCH and needs a
  # `name` back; it also refuses on `isolated` or `archived`. Returning the
  # board-item shape here made the guard read every peer as missing, so the
  # verb refused and cell 1 saw an empty capture.
  echo '{"name":"gtm-engine","isolated":false,"archived":false}'
else
  echo '{"item":{"id":"TEST-1","status":"doing","type":"code"}}'
fi
MOCK
chmod +x "$TMP/bin/curl"
export PATH="$TMP/bin:$PATH"
export AMUX_API="https://localhost:9999"   # never contacted -- curl is mocked
export AMUX_SESSION="wtest" AMUX_WORKER="wtest"

# 1. Setting a shepherd sends the field, attributed via X-Amux-Worker (the CLI
#    adds that header on every board write; not re-checked here since every
#    other board verb already pins it).
export CAPTURE="$TMP/c1"; : > "$CAPTURE"
"$AMUX_BIN" board shepherd TEST-1 gtm-engine >/dev/null 2>&1 || true
has "$TMP/c1" '"shepherd": "gtm-engine"' "shepherd set sends the field"

# 2. 'none' clears it to an empty string, not a literal absence -- the same
#    convention `reviewer` uses, and the one thing a caller cannot express by
#    omitting the argument (omitting dies on usage, cell 3 below).
export CAPTURE="$TMP/c2"; : > "$CAPTURE"
"$AMUX_BIN" board shepherd TEST-1 none >/dev/null 2>&1 || true
has "$TMP/c2" '"shepherd": ""' "'none' clears the field"

# 3. Missing the peer argument must die on usage, not silently PATCH an empty
#    shepherd -- the failure mode a hand-typed `shepherd <id>` with no second
#    arg would otherwise hit.
export CAPTURE="$TMP/c3"; : > "$CAPTURE"
out3=$("$AMUX_BIN" board shepherd TEST-1 2>&1) && rc3=0 || rc3=$?
if [ "$rc3" -ne 0 ] && [ ! -s "$TMP/c3" ]; then
  PASS=$((PASS+1))
else
  FAIL=$((FAIL+1)); echo "FAIL: missing peer must die on usage without PATCHing anything (rc=$rc3): $out3"
fi

# 4. A peer the fleet has never heard of must die WITHOUT PATCHing.
#    `_board_assignee_guard` probes GET /api/sessions/<who> first, and this
#    cell exists because that probe is what broke cell 1: the mock answered
#    every GET with a board-item shape, the guard read "missing" for a peer
#    the test meant to be real, and the verb refused. The harness was pinning a
#    version of the verb that no longer existed, and nothing here said so.
#    Cells 1 and 4 now hold the guard from both sides, so the next change to it
#    reddens one of them instead of silently making the suite describe the past.
export CAPTURE="$TMP/c4"; : > "$CAPTURE"
out4=$("$AMUX_BIN" board shepherd TEST-1 ghost-lane 2>&1) && rc4=0 || rc4=$?
if [ "$rc4" -ne 0 ] && [ ! -s "$TMP/c4" ] && printf '%s' "$out4" | grep -qF 'does not exist'; then
  PASS=$((PASS+1))
else
  FAIL=$((FAIL+1)); echo "FAIL: an unknown peer must be refused before any PATCH (rc=$rc4): $out4"
fi

rm -rf "$TMP"
echo "board shepherd verb: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
