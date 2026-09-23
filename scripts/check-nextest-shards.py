#!/usr/bin/env python3
"""Account for every Rust test across the nextest CI shards.

A green shard proves only that the tests IT ran passed. A wrong partition (a
matrix and a --partition denominator that disagree, a shard that never
uploaded) can skip tests while every job stays green. This compares the
population `cargo nextest list` reported with the union of the shards' JUnit
testcases, by identity (binary-id, test name), not by count.

usage: check-nextest-shards.py <junit-dir> <list.json> <shards>
       check-nextest-shards.py --self-test
Exit 0 when listed == executed; 1 on any missing shard, missing, duplicated
or unlisted test. The verdict is printed as JSON with the counts beside it.
"""
import collections
import json
import pathlib
import sys
import tempfile
import xml.etree.ElementTree as ET


def listed(list_json):
    suites = json.loads(pathlib.Path(list_json).read_text())['rust-suites']
    return {(binary, name) for binary, suite in suites.items()
            for name, case in suite['testcases'].items()
            if not case['ignored'] and case['filter-match']['status'] == 'matches'}


def verdict(junit_dir, list_json, shards):
    files = sorted(pathlib.Path(junit_dir).rglob('*.xml'))
    ran = collections.Counter(
        (case.get('classname'), case.get('name'))
        for path in files for case in ET.parse(path).iter('testcase'))
    expected = listed(list_json)
    problems = {
        'missing_shards': max(0, shards - len(files)),
        'missing': sorted('%s %s' % t for t in expected - set(ran)),
        'duplicated': sorted('%s %s' % t for t, n in ran.items() if n > 1),
        'unlisted': sorted('%s %s' % t for t in set(ran) - expected),
    }
    ok = len(files) == shards and not any(problems[k] for k in ('missing', 'duplicated', 'unlisted'))
    return ok, dict(ok=ok, measured=bool(files), shard_files=len(files), shards=shards,
                    listed=len(expected), executed=sum(ran.values()), **problems)


def self_test():
    cases = [('a', 't1'), ('a', 't2'), ('b', 't3'), ('b', 't4')]
    suites = {'a': {'testcases': {}}, 'b': {'testcases': {}}}
    for binary, name in cases:
        suites[binary]['testcases'][name] = {'ignored': False, 'filter-match': {'status': 'matches'}}
    suites['b']['testcases']['slow'] = {'ignored': True, 'filter-match': {'status': 'mismatch'}}

    def run(shard_cases, n):
        with tempfile.TemporaryDirectory() as tmp:
            pathlib.Path(tmp, 'list.json').write_text(json.dumps({'rust-suites': suites}))
            for i, group in enumerate(shard_cases):
                body = ''.join('<testcase classname="%s" name="%s"/>' % c for c in group)
                pathlib.Path(tmp, 'j%d' % i).mkdir()
                pathlib.Path(tmp, 'j%d' % i, 'junit.xml').write_text('<testsuites>%s</testsuites>' % body)
            return verdict(tmp, pathlib.Path(tmp, 'list.json'), n)[1]

    full = [cases[:2], cases[2:]]
    # Each broken fixture must be refused for ITS reason, named in the verdict.
    checks = [
        ('complete', run(full, 2), lambda v: v['ok'] and v['executed'] == 4),
        ('missing shard', run(full, 3), lambda v: not v['ok'] and v['missing_shards'] == 1),
        ('missing test', run([cases[:2], cases[2:3]], 2), lambda v: not v['ok'] and v['missing'] == ['b t4']),
        ('duplicated test', run([cases[:3], cases[2:]], 2), lambda v: not v['ok'] and v['duplicated'] == ['b t3']),
        ('unlisted test', run([cases[:2], cases[2:] + [('b', 'slow')]], 2),
         lambda v: not v['ok'] and v['unlisted'] == ['b slow']),
    ]
    failed = [name for name, v, good in checks if not good(v)]
    print('self-test %s: %s' % ('FAIL' if failed else 'ok', failed or [c[0] for c in checks]))
    return 1 if failed else 0


def main(argv):
    if argv[1:] == ['--self-test']:
        return self_test()
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    ok, result = verdict(argv[1], argv[2], int(argv[3]))
    print(json.dumps(result, indent=1))
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main(sys.argv))
