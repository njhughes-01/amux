#!/usr/bin/env bash
# AF-565 — a commit must not contain files nobody staged.
#
# `git commit -F - -- <one file>` printed "fatal: cannot lock ref 'HEAD'", EXITED
# 0, and produced a commit carrying that message over a peer's five files and 236
# lines. Two contradictory signals in one run, and the exit status — the thing
# every hook keys on — was the wrong one.
#
# pre-commit records the intended set while holding the pathspec's own temporary
# index; post-commit compares it with what landed. This drives BOTH halves.
#
# `set -e` is the guard against a cell that never ran (AF-561's ratchet applies to
# this file as much as to any other).
set -euo pipefail
export GIT_TEMPLATE_DIR=   # a global init.templatedir must not reach the repos this makes
cd "$(dirname "$0")/.."

HOOKS="$(pwd)/scripts/git-hooks"
CELLS=0
FAILED=0
check() {  # check <label> <actual> <expected>
  CELLS=$((CELLS + 1))
  if [ "$2" = "$3" ]; then echo "  ok    $1"
  else echo "  FAIL  $1 — got '$2', want '$3'"; FAILED=$((FAILED + 1)); fi
}

T=$(mktemp -d "${TMPDIR:-/tmp}/amux-ci-test-XXXXXX")
trap 'rm -rf "$T"' EXIT
git -C "$T" init -q -b main .
git -C "$T" config user.email t@t
git -C "$T" config user.name t
printf 'a\n' > "$T/a.txt"; printf 'b\n' > "$T/b.txt"
git -C "$T" add a.txt b.txt
git -C "$T" commit -qm base -- a.txt b.txt

export AMUX_HOME="$T/.amuxhome"
# KEY THE MANIFEST THE WAY THE HOOK DOES, from `git rev-parse --show-toplevel`,
# not from $T. `mktemp -d` here yields ".../T//amux-..." with a doubled slash and
# macOS resolves /var to /private/var, so a path built from $T names a different
# file than the hook's and every cell would pass for the wrong reason — the check
# would find no manifest and correctly stay silent. Caught by cell (2) failing.
TOP=$(git -C "$T" rev-parse --show-toplevel)
MAN="$AMUX_HOME/commit-intent/$(echo "$TOP" | tr '/' '_').txt"
mkdir -p "$AMUX_HOME/commit-intent"

# Run post-commit exactly as git would, and count only the alarm line.
warns() { ( cd "$T" && sh "$HOOKS/post-commit" 2>&1 | grep -c 'DID NOT STAGE' ) || true; }
manifest() { { echo "head $1"; shift; printf '%s\n' "$@"; } > "$MAN"; }

# (1) the commit matches the manifest — silence.
printf 'a2\n' >> "$T/a.txt"; git -C "$T" add a.txt
manifest "$(git -C "$T" rev-parse HEAD)" a.txt
git -C "$T" commit -qm "only a" -- a.txt
check "a commit matching its manifest is silent" "$(warns)" "0"

# (2) THE RACE. Manifest says one file, the commit carries two.
printf 'a3\n' >> "$T/a.txt"; printf 'b3\n' >> "$T/b.txt"; git -C "$T" add a.txt b.txt
manifest "$(git -C "$T" rev-parse HEAD)" a.txt
git -C "$T" commit -qm "says a, contains a+b" -- a.txt b.txt
check "an unstaged file in the commit is reported" "$(warns)" "1"

# and it must NAME the file, or the reader cannot tell whose it is.
printf 'a4\n' >> "$T/a.txt"; printf 'b4\n' >> "$T/b.txt"; git -C "$T" add a.txt b.txt
manifest "$(git -C "$T" rev-parse HEAD)" a.txt
git -C "$T" commit -qm "again" -- a.txt b.txt
named=$( ( cd "$T" && sh "$HOOKS/post-commit" 2>&1 | grep -c '^        + b.txt' ) || true)
check "the report names the offending file" "$named" "1"

# (3) A STALE manifest (amend, rebase replay) must NOT false-alarm. A guard that
#     cries wolf is a guard people stop reading, which is worse than none.
printf 'a5\n' >> "$T/a.txt"; git -C "$T" add a.txt
manifest 0000000000000000000000000000000000000000 a.txt
git -C "$T" commit -qm "stale manifest" -- a.txt
check "a stale manifest does not false-alarm" "$(warns)" "0"

# (4) No manifest at all — a checkout without the pre-commit half.
rm -f "$MAN"
printf 'a6\n' >> "$T/a.txt"; git -C "$T" add a.txt
git -C "$T" commit -qm "no manifest" -- a.txt
check "no manifest does not false-alarm" "$(warns)" "0"

# (5) THE WRITER. The check above is worthless if pre-commit never records intent,
#     and that half lives in a different file — so drive it, do not assume it.
printf 'a7\n' >> "$T/a.txt"; git -C "$T" add a.txt
rm -f "$MAN"
( cd "$T" && AMUX_HOME="$AMUX_HOME" sh -c '
    ROOT=$(git rev-parse --show-toplevel)
    d="$AMUX_HOME/commit-intent"
    mkdir -p "$d"
    { echo "head $(git rev-parse HEAD)"; git diff --cached --name-only; } \
      > "$d/$(echo "$ROOT" | tr "/" "_").txt"' )
check "pre-commit's writer produces a manifest" "$([ -f "$MAN" ] && echo yes || echo no)" "yes"
check "the manifest records HEAD"  "$(sed -n '1s/^head \(.......\).*/\1/p' "$MAN")" "$(git -C "$T" rev-parse --short=7 HEAD)"
check "the manifest records the staged file" "$(sed -n '2p' "$MAN")" "a.txt"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "PASS ($CELLS outcome cells)"
  exit 0
fi
echo "FAIL ($FAILED of $CELLS outcome cells)"
exit 1

# AF-565: live check that the intent manifest round-trips
