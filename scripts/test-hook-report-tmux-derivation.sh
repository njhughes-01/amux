#!/usr/bin/env bash
# hook-report.sh and the amux CLI may recover a missing $AMUX_SESSION from
# tmux only when they run inside a tmux pane. Outside tmux, `tmux
# display-message` answers for the most recently used session, so an
# operator's plain shell or Claude session acted as an unrelated lane.
set -Eeuo pipefail
trap 'echo "FAIL hook-report tmux derivation line=$LINENO command=$BASH_COMMAND" >&2' ERR
cd "$(dirname "$0")/.."
HOOK="$PWD/scripts/hooks/hook-report.sh"
TMP="$(mktemp -d)"
cleanup() { pkill -f -- "$TMP" 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT

mkdir -p "$TMP/home/.amux/sessions" "$TMP/bin"
: > "$TMP/home/.amux/sessions/victim.env"
: > "$TMP/home/.amux/sessions/mine.env"
# Fake tmux: with -t it answers for that pane; without, it answers for the
# server's last-used session, like the real one does outside a client.
cat > "$TMP/bin/tmux" <<'SH'
#!/usr/bin/env bash
case " $* " in
  *" -t %7 "*) echo amux-mine ;;
  *) echo amux-victim ;;
esac
SH
cat > "$TMP/bin/curl" <<'SH'
#!/usr/bin/env bash
echo 000
SH
chmod +x "$TMP/bin/tmux" "$TMP/bin/curl"

run_hook() {
  env -i HOME="$TMP/home" PATH="$TMP/bin:/usr/bin:/bin" AMUX_URL="http://127.0.0.1:9" "$@" \
    bash "$HOOK" idle stop-hook <<<'{}' >/dev/null 2>&1 || true
}
reported_as() { [ -e "$TMP/home/.amux/hook-report-queue/$1.state.json" ]; }

# Outside tmux: no report at all, and certainly not as the last-used lane.
run_hook
if reported_as victim; then echo "FAIL hook outside tmux reported as the last-used tmux session" >&2; exit 1; fi
echo "ok   hook outside tmux does not adopt the last-used lane"

# Control: inside a pane the lost name is still recovered, from THAT pane.
run_hook TMUX="/tmp/tmux-1000/default,1,0" TMUX_PANE="%7"
reported_as mine || { echo "FAIL hook inside tmux did not recover its own pane's session" >&2; exit 1; }
if reported_as victim; then echo "FAIL hook inside tmux reported as another pane's session" >&2; exit 1; fi
echo "ok   hook inside tmux recovers its own pane's session"

# The CLI has the same fallback: outside tmux it must not act as a lane.
cli_whoami() {
  env -i HOME="$TMP/home" PATH="$TMP/bin:/usr/bin:/bin" "$@" bash "$PWD/amux" whoami 2>&1 | sed -n 1p
}
out="$(cli_whoami)"
case "$out" in *victim*) echo "FAIL amux CLI outside tmux acts as the last-used lane: $out" >&2; exit 1 ;; esac
echo "ok   amux CLI outside tmux does not adopt the last-used lane"
out="$(cli_whoami TMUX="/tmp/tmux-1000/default,1,0" TMUX_PANE="%7")"
case "$out" in *mine*) ;; *) echo "FAIL amux CLI inside tmux did not recover its own pane's session: $out" >&2; exit 1 ;; esac
echo "ok   amux CLI inside tmux recovers its own pane's session"
