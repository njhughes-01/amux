#!/usr/bin/env bash
# test-selector-clauses.sh — AF-346 (second pass) and AF-336.
#
# AF-346's closing sentence is that `--lib` is not the only flag that reports a
# subset as if it were a total. `--test <name>` is the sharper case: it skips the
# LIB ENTIRELY, which is the larger half of this crate, and prints the SMALLEST
# number of any selector, so it is the one most likely to be read as a clean
# suite. `scripts/test-target-clause.sh` cell 2 asserted that case stayed silent.
# That cell encoded the narrower contract and is updated alongside this file.
#
# AF-336 asked the runner to say whether a peer's uncommitted source is in the
# code THIS command compiled. The existing worktree clause lists every dirty
# file, which is right and is a different question: on this checkout the usual
# dirty set is a peer editing a crate you are not testing, so five paths read as
# five reasons to doubt a red when the honest count is zero.
#
# WHY THIS HARNESS STUBS CARGO. test-target-clause.sh invokes the runner for
# real, so every cell pays a compile. Worse for the cells here: they need a
# DIRTY TREE with files under a specific package, and the only way to get one
# without editing the shared checkout is a scratch repo — where a real cargo run
# would compile a different workspace root into the shared CARGO_TARGET_DIR and
# evict every other lane's artifacts. So the runner is invoked with a stub
# safe-cargo.sh beside it. That also makes every cell host-independent, which is
# the AF-564 lesson: a cell whose precondition is environmental cannot fail on
# the machine that ships it.
set -eu
export GIT_TEMPLATE_DIR=   # a global init.templatedir must not reach the repos this makes
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0; FAIL=0
ok()  { if [ "$2" = "$3" ]; then PASS=$((PASS+1)); echo "  ok   $1"; else
        FAIL=$((FAIL+1)); echo "  FAIL $1: want [$3] got [$2]"; fi; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# The runner resolves safe-cargo.sh next to its own origin path, so a copy in a
# scratch dir with a stub beside it never reaches cargo.
BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$SRC/scripts/test-contended.sh" "$BIN/"
cat > "$BIN/safe-cargo.sh" <<'STUB'
#!/usr/bin/env bash
echo "test result: ok. 3 passed; 0 failed; 0 ignored; 1841 filtered out"
exit 0
STUB
chmod +x "$BIN/safe-cargo.sh"
# write-test-receipt.sh is optional in the runner ([ -x ] guarded); leaving it
# absent keeps this harness from writing a receipt for a run that never compiled.
RUN="$BIN/test-contended.sh"

# A scratch repo, so the worktree clause has a real `git status` to read and the
# shared checkout is never touched.
REPO="$TMP/repo"; mkdir -p "$REPO/crates/amux-server/src" "$REPO/crates/amux-core/src"
git -C "$REPO" init -q
git -C "$REPO" config user.email t@example.invalid
git -C "$REPO" config user.name t
echo base > "$REPO/crates/amux-server/src/lib.rs"
echo base > "$REPO/crates/amux-core/src/lib.rs"
git -C "$REPO" add -A && git -C "$REPO" commit -qm base

r() { (cd "$REPO" && AMUX_RS_BUILD_LOCK="$TMP/nolock" "$RUN" "$@" 2>&1); }

echo "cell 1: --test announces that the LIB was not run"
o=$(r -p amux-server --test route_table)
ok "names the skipped lib" "$(printf '%s' "$o" | grep -c 'THE LIB WAS NOT RUN')" "1"

echo "cell 2: CONTROL — --lib does not claim the lib was skipped"
o=$(r -p amux-server --lib)
ok "no lib-was-not-run line" "$(printf '%s' "$o" | grep -c 'THE LIB WAS NOT RUN')" "0"

echo "cell 3: CONTROL — a full run selects no kind and says nothing about one"
o=$(r -p amux-server)
ok "no selector-kind clause" "$(printf '%s' "$o" | grep -c 'this invocation selected ')" "0"

echo "cell 4: a bare name FILTER is named back to the reader"
o=$(r -p amux-server --lib autofix)
ok "filter clause fires"   "$(printf '%s' "$o" | grep -c 'a name FILTER was passed')" "1"
ok "and names the filter"  "$(printf '%s' "$o" | grep -c 'passed (autofix)')" "1"

echo "cell 5: CONTROL — the VALUE of a value-taking flag is not a filter"
# `--test route_table` and `-p amux-server` both have a bare-looking operand.
# The first version of this loop had no _want_value state and reported both as
# filters, which would have printed a caveat about a narrowing that did not
# happen — a false positive in an instrument whose whole job is honesty.
o=$(r -p amux-server --test route_table)
ok "no filter clause for flag values" "$(printf '%s' "$o" | grep -c 'a name FILTER was passed')" "0"

echo "cell 6: CONTROL — args after -- are the harness's, not a cargo filter"
o=$(r -p amux-server --lib -- --test-threads=1)
ok "stops at the -- separator" "$(printf '%s' "$o" | grep -c 'a name FILTER was passed')" "0"

echo "cell 7: AF-336 — dirty files are counted AGAINST THE PACKAGE under test"
echo peer-draft > "$REPO/crates/amux-server/src/lib.rs"
o=$(r -p amux-server --lib)
ok "scoped count fires"    "$(printf '%s' "$o" | grep -c 'are under crates/amux-server/')" "1"
ok "and counts 1 of 1"     "$(printf '%s' "$o" | grep -c '1 of 1 are under crates/amux-server/')" "1"

echo "cell 8: THE CELL THAT MATTERS — a dirty tree OUTSIDE the package reads zero"
# This is the whole point of scoping. Unscoped, this run shows one dirty file
# and the reader has a reason to doubt a red. Scoped, the honest answer is that
# nothing a peer is editing was compiled into it.
git -C "$REPO" checkout -- crates/amux-server/src/lib.rs
echo peer-draft > "$REPO/crates/amux-core/src/lib.rs"
o=$(r -p amux-server --lib)
ok "still reports the dirty file" "$(printf '%s' "$o" | grep -c 'crates/amux-core/src/lib.rs')" "1"
ok "but scopes it to zero"        "$(printf '%s' "$o" | grep -c '0 of 1 are under crates/amux-server/')" "1"

echo "cell 9: CONTROL — with no -p, no scoped count is printed at all"
# An unscoped count printed as though it were scoped is the defect this adds, so
# the absence has to be asserted rather than assumed.
o=$(r --lib)
ok "silent without a package" "$(printf '%s' "$o" | grep -c 'are under ')" "0"

git -C "$REPO" checkout -- crates/amux-core/src/lib.rs

echo ""
echo "test-selector-clauses: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
