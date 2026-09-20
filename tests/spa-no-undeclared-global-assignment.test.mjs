// AF-940: ports a lost class-invariant regression test. The Python server
// carried test_no_client_assignment_to_the_dead_workers_global (an ast.parse
// check that no code assigned to an undeclared client global, e.g. the
// AF-10 bug `workers = msg.payload;` with `workers` never declared) — it was
// dropped when the Python server was deleted at 792ce1f and never ported.
//
// The enforcement mechanism already exists and is already wired into CI:
// eslint.config.mjs's `no-undef` (scripts/spa-lint.sh, rust.yml's e2e-shards
// job). This test is the missing PROOF that mechanism actually catches the
// historical bug's exact shape, run as its own regression check rather than
// left to an inference about what `no-undef` happens to cover — and it
// re-derives that proof on every run, not just once by hand.
import { readFileSync } from 'node:fs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { Linter } from 'eslint';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import path from 'node:path';

const repoRoot = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const appJsPath = path.join(repoRoot, 'crates/amux-dashboard/static/app.js');

// The generated globals allowlist (top-level declarations in app.js) must be
// fresh before linting, exactly as scripts/spa-lint.sh does — a stale file
// would either hide a real violation behind a stale global, or manufacture a
// false one for a name that's since been added.
execFileSync('node', ['scripts/gen-spa-globals.mjs'], { cwd: repoRoot, stdio: 'inherit' });

async function pageLanguageOptions() {
  // Cache-bust the import so the freshly regenerated globals file above is
  // actually read, not a copy of eslint.config.mjs from an earlier import.
  const url = `${pathToFileURL(path.join(repoRoot, 'eslint.config.mjs')).href}?t=${Date.now()}`;
  const { default: config } = await import(url);
  const pageConfig = config.find(
    (c) => Array.isArray(c.files) && c.files.includes('crates/amux-dashboard/static/*.js')
  );
  assert.ok(
    pageConfig,
    'eslint.config.mjs no longer has the app.js page-script block this guard reads languageOptions from'
  );
  return pageConfig.languageOptions;
}

function noUndefViolations(code, languageOptions) {
  const linter = new Linter();
  return linter
    .verify(code, { languageOptions, rules: { 'no-undef': 'error' } })
    .filter((m) => m.ruleId === 'no-undef');
}

test('app.js has no assignment to an undeclared client-side global (AF-940)', async () => {
  const languageOptions = await pageLanguageOptions();
  const src = readFileSync(appJsPath, 'utf8');

  const existing = noUndefViolations(src, languageOptions);
  assert.deepEqual(
    existing,
    [],
    `app.js references an undeclared identifier — the AF-940 bug class (dead/implicit ` +
      `global instead of declared state): ${JSON.stringify(existing)}`
  );

  // Mutation test: plant the historical bug shape (bare assignment to a name
  // never declared with var/let/const/window., appended so it can't collide
  // with any real top-level declaration) and confirm the guard fires.
  const probeName = '__af940_undeclared_global_probe__';
  const mutated = `${src}\nfunction __af940MutationProbe(msg) { ${probeName} = msg.payload; }\n`;

  const caught = noUndefViolations(mutated, languageOptions);
  assert.ok(
    caught.some((m) => m.message.includes(`'${probeName}' is not defined`)),
    'planting the AF-940 bug pattern (bare `x = value` with x never declared) did not ' +
      `trigger no-undef — the guard would not catch reintroduction: ${JSON.stringify(caught)}`
  );
});
