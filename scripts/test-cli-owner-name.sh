#!/usr/bin/env bash
# The CLI names the owner from configuration, never a baked-in person (GC3-7):
# AMUX_OWNER_NAME in the environment, then ~/.amux/server.env, then
# `git config --global user.name`, then the login. Only the function is
# extracted, in a throwaway HOME, so nothing on this machine is read.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fn="$(sed -n '/^_owner_name() {/,/^}/p' "$ROOT/amux")"
[ -n "$fn" ] || { echo "FAIL _owner_name not found in amux"; exit 1; }
eval "$fn"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
export HOME="$T" AMUX_HOME="$T/.amux" GIT_CONFIG_NOSYSTEM=1 USER=login-user LOGNAME=login-user
unset AMUX_OWNER_NAME GIT_CONFIG_GLOBAL
mkdir -p "$AMUX_HOME"
fail=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi; }

check "no configuration falls back to the login" "$(_owner_name)" "login-user"
git config --global user.name "Git Person"
check "git user.name beats the login" "$(_owner_name)" "Git Person"
printf 'OTHER=x\nAMUX_OWNER_NAME="Env File Owner"\n' > "$AMUX_HOME/server.env"
check "server.env beats git (quotes stripped)" "$(_owner_name)" "Env File Owner"
check "the environment beats server.env" "$(AMUX_OWNER_NAME='Shell Owner' _owner_name)" "Shell Owner"

[ "$fail" -eq 0 ] && echo PASS || echo FAIL
exit "$fail"
