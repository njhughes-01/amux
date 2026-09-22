#!/usr/bin/env bash
# RR-0152 — soak harness for the Rust amux server.
#
# The plan item asks for 24h of stability evidence. A session cannot sit for
# 24h, so this is the INSTRUMENT rather than the run: it samples the server on
# an interval, appends one JSON object per sample to a JSONL file, and can be
# asked at any moment for a verdict over everything collected so far. Start it,
# leave it, come back.
#
#   scripts/soak.sh                      # sample every 60s until stopped
#   scripts/soak.sh --interval 30 --out /tmp/soak.jsonl
#   scripts/soak.sh --once               # one sample, print it, exit
#   scripts/soak.sh --verdict            # summarise an existing JSONL
#   scripts/soak.sh --duration 86400     # a real 24h run
#
# WHAT IT FLAGS, and why each one is here rather than a threshold someone
# picked (ethos rule 7: prefer the structurally-absent signal over the tuned
# parameter):
#
#   build_changed      /health `build` is a content hash of the running
#                      binary. This server re-execs whenever anyone installs a
#                      new one, and on a shared machine that is routinely not
#                      you. EVERY latency number in a window that spans a build
#                      change is measured against two different servers, so the
#                      verdict segments by build instead of averaging across it.
#   store_degraded     `store` != ok. Absent in the healthy state, so it needs
#                      no threshold.
#   unreachable        /health did not answer at all. Also absent when healthy.
#   proxied_families   /api/debug/boundary `proxied` must be EMPTY post-cutover
#                      — python is gone, so anything listed there is a family
#                      pointing at a server that does not exist. Not a
#                      threshold: the healthy value is exactly 0.
#   dead_pipes         /api/debug/logs `counts.unpiped` / `counts.stale`. A
#                      lane whose log pipe died goes silent, and silence reads
#                      as health — the exact failure the endpoint exists for.
#   invariants_worse   /api/health/invariants `checks.fail` above the baseline.
#                      Absolute failure counts are NOT flagged: the fleet
#                      already carries a standing set, and a monitor that fires
#                      permanently is one nobody reads. Only a REGRESSION fires.
#   p95_regression     per family, from /api/logs/stats. Needs a threshold, so
#                      it carries two guards: a ratio vs this family's own
#                      baseline AND an absolute floor, because a family that
#                      moves 0.3ms -> 1.2ms is 4x and means nothing. Baseline
#                      is per-family and per-build.
#   server_5xx         5xx counts from /api/logs/analyze. The healthy value is
#                      0, so no rate threshold.
#   unrouted_traffic   404/405 traffic grouped BY PATH, from /api/logs/analyze.
#                      This replaced a per-family `error_rate` flag, which was
#                      useless: /api/board sits at a permanent ~33% error rate
#                      that is almost all deliberate gate 409s, so the flag
#                      fired on healthy behaviour while the defect that mattered
#                      — 1147 POSTs/hour to an unrouted /api/git/staged-guard,
#                      which silently makes the commit guard fail open — was a
#                      rounding error inside the same number. Grouping by path
#                      makes the flag NAME the defect instead of counting it.
#
# KNOWN WINDOW LIMITATION: /api/logs/analyze and /api/logs/stats are queried
# with since_h=1, and that hour can span a build change — so `unrouted_top` may
# list a path that a NEWER build already mounts (observed: /api/sessions-git and
# /api/board/session-gates appeared as 404s from a previous build while
# answering 200 live). Confirm any unrouted path against the CURRENT build with
# a direct request before filing it.
#
# Every sample records WHICH endpoints answered and which did not, so a gap in
# the data is visible as a gap rather than as an absence of problems.
# /api/debug/board-drive is expected to 404 until that endpoint lands; a 404
# there is recorded as `absent`, not as an error.

set -uo pipefail

URL="${AMUX_SOAK_URL:-https://localhost:8824}"
INTERVAL=60
OUT="${AMUX_SOAK_OUT:-$HOME/.amux/soak.jsonl}"
DURATION=0
MODE=loop

while [ $# -gt 0 ]; do
  case "$1" in
    --url)      URL="$2"; shift 2 ;;
    --interval) INTERVAL="$2"; shift 2 ;;
    --out)      OUT="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --once)     MODE=once; shift ;;
    --verdict)  MODE=verdict; shift ;;
    -h|--help)  sed -n '2,60p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "$(dirname "$OUT")"

# --------------------------------------------------------------------------
# One sample -> one JSON line on stdout.
# --------------------------------------------------------------------------
sample() {
python3 - "$URL" <<'PY'
import json, ssl, sys, time, urllib.request

BASE = sys.argv[1]
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE

def fetch(path, timeout=20):
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(BASE + path, timeout=timeout, context=CTX) as r:
            body = r.read().decode("utf-8", "replace")
            ms = (time.monotonic() - t0) * 1000
            try:
                return r.getcode(), json.loads(body), ms
            except Exception:
                return r.getcode(), None, ms
    except urllib.error.HTTPError as e:
        return e.code, None, (time.monotonic() - t0) * 1000
    except Exception:
        return 0, None, (time.monotonic() - t0) * 1000

s = {"ts": time.time(), "url": BASE, "endpoints": {}}

code, health, ms = fetch("/health")
s["endpoints"]["/health"] = code
s["health_ms"] = round(ms, 2)
if health:
    for k in ("build", "store", "rev", "uptime_s", "pid"):
        s[k] = health.get(k)
else:
    s["build"] = None
    s["store"] = None

code, boundary, _ = fetch("/api/debug/boundary")
s["endpoints"]["/api/debug/boundary"] = code
if isinstance(boundary, dict):
    pr = boundary.get("proxied")
    s["proxied_count"] = len(pr) if isinstance(pr, list) else None
    nat = boundary.get("native")
    s["native_count"] = len(nat) if isinstance(nat, list) else None

code, dlogs, _ = fetch("/api/debug/logs")
s["endpoints"]["/api/debug/logs"] = code
if isinstance(dlogs, dict):
    c = dlogs.get("counts") or {}
    s["log_pipes"] = {k: c.get(k) for k in ("total", "ok", "stale", "unpiped", "not_running", "idle", "recovering")}
    s["log_pipes_healthy"] = dlogs.get("healthy")

# Expected to 404 until the board-drive trace endpoint lands. Recorded as
# `absent` so a later 200 is visibly a change and not a silent one.
code, bdrive, _ = fetch("/api/debug/board-drive")
s["endpoints"]["/api/debug/board-drive"] = code
s["board_drive"] = "absent" if code == 404 else ("ok" if code == 200 else "error")
if isinstance(bdrive, dict):
    for k in ("assigned", "nudged", "skipped", "tick_secs", "lanes"):
        if k in bdrive:
            s.setdefault("board_drive_detail", {})[k] = bdrive[k]

code, stats, ms = fetch("/api/logs/stats?since_h=1", timeout=60)
s["endpoints"]["/api/logs/stats"] = code
s["stats_ms"] = round(ms, 2)
fams = {}
if isinstance(stats, dict):
    for f in stats.get("families", []) or []:
        family = f.get("family")
        method = f.get("method")
        if not family or not method:
            continue
        # Stats families are method-plus-path populations. Keep the method in
        # the snapshot key so a POST cannot overwrite a GET for the same path.
        fams[f"{method} {family}"] = {
            "count": f.get("count"),
            "p50": f.get("p50_ms"),
            "p95": f.get("p95_ms"),
            "err": f.get("error_rate"),
            "errs": f.get("error_count"),
            "proxy": f.get("proxy_count"),
            "py": (f.get("origins") or {}).get("python", 0),
        }
s["families"] = fams
s["family_count"] = len(fams)
s["python_origin_total"] = sum((v.get("py") or 0) for v in fams.values())
s["proxy_count_total"] = sum((v.get("proxy") or 0) for v in fams.values())

# /api/logs/analyze is the only endpoint that breaks errors down BY STATUS and
# BY PATH, and it already writes the verdict in words ("no route exists at this
# path..."). Without it the error-rate signal is unusable: /api/board sits at a
# permanent ~33% "error rate" that is almost entirely deliberate gate 409s,
# while the thing that actually matters — 1106 POSTs/hour to an unrouted
# /api/git/staged-guard, which makes the commit guard fail open silently — is a
# rounding error in the same number. Sampling the verdicts turns a metric that
# fires on healthy behaviour into one that names the defect (ethos rule 4: a
# tag in a store the reader never opens is the same as no tag).
code, an, _ = fetch("/api/logs/analyze?since_h=1", timeout=90)
s["endpoints"]["/api/logs/analyze"] = code
if isinstance(an, dict):
    s["total_errors"] = an.get("total_errors")
    s["verdicts"] = (an.get("verdicts") or [])[:12]
    # 5xx is the server's own fault; 4xx includes gates and genuine 404s, so
    # keep them apart rather than summing into one "errors" number.
    sx = ux = 0
    unrouted = {}
    for g in an.get("groups", []) or []:
        st, n = g.get("status") or 0, g.get("count") or 0
        if st >= 500:
            sx += n
        elif st in (404, 405):
            ux += n
            key = f"{g.get('method','?')} {g.get('target','?')}"
            unrouted[key] = unrouted.get(key, 0) + n
    s["err_5xx"] = sx
    s["err_unrouted_4xx"] = ux
    s["unrouted_top"] = dict(sorted(unrouted.items(), key=lambda kv: -kv[1])[:8])

code, inv, _ = fetch("/api/health/invariants", timeout=60)
s["endpoints"]["/api/health/invariants"] = code
if isinstance(inv, dict):
    ch = inv.get("checks") or {}
    s["invariants"] = {k: ch.get(k) for k in ("pass", "fail", "unknown", "total")}
    s["invariant_confidence"] = inv.get("confidence")

s["rss_kb"] = None
if s.get("pid"):
    try:
        import subprocess
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(s["pid"])],
                             capture_output=True, text=True, timeout=10).stdout.strip()
        s["rss_kb"] = int(out) if out else None
    except Exception:
        pass

print(json.dumps(s, separators=(",", ":")))
PY
}

# --------------------------------------------------------------------------
# Verdict over an existing JSONL.
# --------------------------------------------------------------------------
verdict() {
python3 - "$OUT" <<'PY'
import json, sys, time
from collections import defaultdict

path = sys.argv[1]
rows = []
with open(path) as fh:
    for line in fh:
        line = line.strip()
        if line:
            try:
                rows.append(json.loads(line))
            except Exception:
                pass
if not rows:
    print(f"no samples in {path}")
    sys.exit(1)

# P95 regression guards. Both must trip: a ratio against this family's own
# baseline, and an absolute floor — a family moving 0.3ms -> 1.2ms is 4x and
# is noise.
P95_RATIO, P95_FLOOR_MS = 3.0, 250.0
ERR_RATE, ERR_MIN_COUNT = 0.25, 20

flags = defaultdict(list)
builds = []
for r in rows:
    b = r.get("build")
    if b and (not builds or builds[-1] != b):
        builds.append(b)

first, last = rows[0], rows[-1]
span_s = last["ts"] - first["ts"]

# Baselines come from the FIRST sample of the CURRENT build: comparing latency
# across a build change compares two different servers.
cur_build = last.get("build")
base = next((r for r in rows if r.get("build") == cur_build), first)
base_fams = base.get("families") or {}
base_inv_fail = (base.get("invariants") or {}).get("fail")

for r in rows:
    ts = r["ts"]
    if r.get("endpoints", {}).get("/health") != 200:
        flags["unreachable"].append((ts, f"/health -> {r.get('endpoints',{}).get('/health')}"))
    if r.get("store") not in (None, "ok"):
        flags["store_degraded"].append((ts, f"store={r.get('store')}"))
    if r.get("proxied_count") not in (None, 0):
        flags["proxied_families"].append((ts, f"proxied={r.get('proxied_count')}"))
    if (r.get("python_origin_total") or 0) > 0:
        flags["python_origin_traffic"].append((ts, f"python-origin requests={r['python_origin_total']}"))
    lp = r.get("log_pipes") or {}
    dead = (lp.get("unpiped") or 0) + (lp.get("stale") or 0)
    if dead > 0:
        flags["dead_pipes"].append((ts, f"unpiped={lp.get('unpiped')} stale={lp.get('stale')}"))
    inv = (r.get("invariants") or {}).get("fail")
    if inv is not None and base_inv_fail is not None and inv > base_inv_fail:
        flags["invariants_worse"].append((ts, f"fail {base_inv_fail} -> {inv}"))
    if r.get("build") == cur_build:
        for fam, v in (r.get("families") or {}).items():
            bp = (base_fams.get(fam) or {}).get("p95")
            p95 = v.get("p95")
            if bp and p95 and p95 >= P95_FLOOR_MS and p95 > bp * P95_RATIO:
                flags["p95_regression"].append((ts, f"{fam} p95 {bp:.0f}ms -> {p95:.0f}ms"))
    # 5xx: the server's own faults. No family/rate threshold — any sustained
    # 5xx is worth naming, and the healthy value is 0.
    if (r.get("err_5xx") or 0) > 0:
        flags["server_5xx"].append((ts, f"{r['err_5xx']} 5xx responses in the last hour"))
    # Unrouted traffic: a client calling a path the server does not mount. This
    # is the cutover's signature failure — the caller fails open or renders
    # nothing, and NOTHING errors loudly. Reported with the top offender so the
    # flag names the defect instead of just counting it.
    # NOT `path`: that name holds the JSONL being summarised, and shadowing it
    # here made the verdict header print a random API route as its own filename.
    for route, n in (r.get("unrouted_top") or {}).items():
        if n >= 50:
            flags["unrouted_traffic"].append((ts, f"{route} x{n}/h -> 404/405"))

def hhmm(s):
    return time.strftime("%H:%M:%S", time.localtime(s))

print(f"=== RR-0152 soak verdict — {path}")
print(f"samples      : {len(rows)} over {span_s/3600:.2f}h "
      f"({hhmm(first['ts'])} -> {hhmm(last['ts'])})")
print(f"builds seen  : {len(builds)} {builds if len(builds) <= 4 else builds[:4] + ['...']}")
rss = [r['rss_kb'] for r in rows if r.get('rss_kb')]
if rss:
    print(f"rss          : first={rss[0]/1024:.0f}MB last={rss[-1]/1024:.0f}MB "
          f"min={min(rss)/1024:.0f}MB max={max(rss)/1024:.0f}MB "
          f"drift={(rss[-1]-rss[0])/1024:+.0f}MB")
ups = [r.get('uptime_s') for r in rows if r.get('uptime_s') is not None]
restarts = sum(1 for a, b in zip(ups, ups[1:]) if b < a)
print(f"restarts     : {restarts} (uptime_s went backwards)")
inv_now = (last.get('invariants') or {})
if inv_now:
    print(f"invariants   : pass={inv_now.get('pass')} fail={inv_now.get('fail')} "
          f"confidence={last.get('invariant_confidence')}")
print(f"board_drive  : {last.get('board_drive')}")

if len(builds) > 1:
    flags["build_changed"] = [(0, " -> ".join(builds))]

print()
if not flags:
    print("VERDICT: CLEAN — no flags raised.")
else:
    print("VERDICT: FLAGGED")
    for k, v in sorted(flags.items(), key=lambda kv: -len(kv[1])):
        print(f"  {k:22} x{len(v):<5} e.g. {v[0][1]}")
    print()
    print("  (build_changed is informational: latency is compared only WITHIN a build.)")
sys.exit(0)
PY
}

case "$MODE" in
  once)
    sample
    ;;
  verdict)
    verdict
    ;;
  loop)
    echo "soak: url=$URL interval=${INTERVAL}s out=$OUT duration=${DURATION:-unbounded}s (pid $$)" >&2
    start=$(date +%s)
    while true; do
      line="$(sample)"
      if [ -n "$line" ]; then
        printf '%s\n' "$line" >> "$OUT"
      fi
      if [ "$DURATION" -gt 0 ]; then
        now=$(date +%s)
        [ $((now - start)) -ge "$DURATION" ] && break
      fi
      sleep "$INTERVAL"
    done
    verdict
    ;;
esac
