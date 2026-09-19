#!/usr/bin/env python3
"""Fail when runtime code hardcodes a specific person, machine or deployment.

amux is public and anyone can run install.sh. Names, email addresses, home
directories, hostnames and LAN addresses of whoever wrote a line must come
from configuration (server.env, the environment, git config, the OS), never
from source. See .claude/rules/no-hardcoded-specifics.md.

Scans tracked runtime files, skipping comment lines, Rust `#[cfg(test)]`
modules and test files. Known violations live in
scripts/fixtures/hardcoded-specifics-baseline.txt as `path<TAB>count<TAB>why`.
The baseline is a ratchet: a file above its count, or a new file, fails; a
file BELOW its count also fails until the baseline is lowered, so a fix
cannot leave slack for the next regression.

Run: python3 scripts/test-no-hardcoded-specifics.py   (exit 0 = clean)
"""
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "fixtures", "hardcoded-specifics-baseline.txt")

PATTERNS = [
    ("person name", re.compile(r"\bethan\b", re.I)),
    ("personal email", re.compile(r"\b[\w.+-]+@(mixpeek|trymixpeek)\.com\b", re.I)),
    ("upstream domain", re.compile(r"\b(try)?mixpeek\.com\b", re.I)),
    ("user home path", re.compile(r"/Users/[a-z][\w.-]*|/home/(?!\$|\{)[a-z][\w.-]*/")),
    ("github handle", re.compile(r"\besteininger\b", re.I)),
    ("LAN address", re.compile(r"\b(10\.\d{1,3}|192\.168|172\.(1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}\b")),
]

RUNTIME_GLOBS = ("crates/", "scripts/", "templates/", "skills/", "amux", "install.sh",
                 "uninstall.sh", "amux-remote")
SKIP_PARTS = ("/tests/", "/test_", "/fixtures/", "/node_modules/", "/testdata/")
SKIP_NAME = re.compile(r"^(test[-_].*|.*[-_]test\.\w+|.*\.test\.\w+|.*_tests?\.rs)$")
TEXT_EXT = {".rs", ".js", ".mjs", ".cjs", ".ts", ".py", ".sh", ".bash", ".html",
            ".css", ".toml", ".json", ".template", ".service", ".timer", ".env", ""}
COMMENT = re.compile(r"^\s*(//|#|/\*|\*|<!--|--|;)")


def tracked():
    out = subprocess.run(["git", "-C", ROOT, "ls-files", "-z"], capture_output=True, check=True).stdout
    for p in out.decode().split("\0"):
        if not p or not p.startswith(RUNTIME_GLOBS):
            continue
        if any(s in "/" + p for s in SKIP_PARTS) or SKIP_NAME.match(os.path.basename(p)):
            continue
        if os.path.splitext(p)[1] not in TEXT_EXT:
            continue
        yield p


def runtime_lines(path):
    try:
        with open(os.path.join(ROOT, path), encoding="utf-8") as f:
            lines = f.read().split("\n")
    except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
        return
    in_block = False
    for i, line in enumerate(lines, 1):
        s = line.strip()
        if path.endswith(".rs") and s == "#[cfg(test)]":
            nxt = lines[i].strip() if i < len(lines) else ""
            if re.match(r"(pub(\([\w:]+\))?\s+)?mod\s", nxt):
                return
        if in_block:
            if "*/" in s or "-->" in s:
                in_block = False
            continue
        if s.startswith("/*") and "*/" not in s:
            in_block = True
            continue
        if s.startswith("<!--") and "-->" not in s:
            in_block = True
            continue
        if COMMENT.match(line):
            continue
        # Drop a trailing `// ...` comment (not inside a URL like https://).
        code = re.sub(r"(?<![:\"'])\s//\s.*$", "", line)
        yield i, code


def scan():
    hits = {}
    for p in tracked():
        for n, code in runtime_lines(p):
            for label, rx in PATTERNS:
                if rx.search(code):
                    hits.setdefault(p, []).append((n, label, code.strip()[:120]))
                    break
    return hits


def load_baseline():
    base = {}
    if os.path.exists(BASELINE):
        for raw in open(BASELINE, encoding="utf-8"):
            if not raw.strip() or raw.startswith("#"):
                continue
            path, count, *_ = raw.rstrip("\n").split("\t")
            base[path] = int(count)
    return base


def main():
    hits = scan()
    base = load_baseline()
    if "--write-baseline" in sys.argv:
        with open(BASELINE, "w", encoding="utf-8") as f:
            f.write("# path\tcount\twhy (lower the count as each is fixed; never raise it)\n")
            for p in sorted(hits):
                f.write(f"{p}\t{len(hits[p])}\tknown upstream-specific runtime value, pending fix\n")
        print(f"wrote {len(hits)} entries to {os.path.relpath(BASELINE, ROOT)}")
        return 0
    fails = []
    for p, rows in sorted(hits.items()):
        allowed = base.get(p, 0)
        if len(rows) > allowed:
            fails.append(f"NEW  {p}: {len(rows)} hardcoded specific(s), baseline {allowed}")
            for n, label, code in rows:
                fails.append(f"       {p}:{n} [{label}] {code}")
    for p, allowed in sorted(base.items()):
        have = len(hits.get(p, []))
        if have < allowed:
            fails.append(f"LOWER {p}: baseline {allowed}, now {have}; lower the baseline to {have}")
    total = sum(len(v) for v in hits.values())
    print(f"hardcoded-specifics: {total} runtime hit(s) in {len(hits)} file(s); baseline allows {sum(base.values())}")
    if fails:
        print("\n".join(fails))
        print("FAIL  move the value into configuration (see .claude/rules/no-hardcoded-specifics.md)")
        return 1
    print("ok    no hardcoded person, machine or deployment beyond the recorded baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
