# Command gates on board transitions

## Status and objective

This document proposes machine-executed command gates for board transitions. The motivating rule is: a code card cannot enter `review` until its declared test command has passed against the exact pull request head. The design reuses reconciliation's command runner and the board's existing gate resolution and transition enforcement. It does not add another workflow engine.

The first release should guard `review`, `done`, and `verified`. The data model should allow any destination status because board gates already guard entry into a status rather than a named verb (`crates/amux-core/src/board.rs:346-365`), and every status-changing transition passes through one gate check (`crates/amux-core/src/board.rs:591-610`).

## Existing pieces to reuse

### Reconciliation command execution

Reconciliation already defines a structured command as `label`, `program`, `args`, and `timeout_secs`; unknown fields are rejected and the default timeout is 900 seconds (`crates/amux-server/src/reconciliation.rs:13-26`). It resolves the candidate to a commit and creates a detached temporary worktree at that exact SHA (`crates/amux-server/src/reconciliation.rs:115-145`). Commands run with that worktree as their current directory and capture stdout and stderr (`crates/amux-server/src/reconciliation.rs:148-157`).

The runner enforces a maximum 3,600 second timeout, kills and waits for timed-out children, stops after the first unsuccessful gate, and reports a distinct timeout failure (`crates/amux-server/src/reconciliation.rs:10-11`, `crates/amux-server/src/reconciliation.rs:105-113`, `crates/amux-server/src/reconciliation.rs:158-167`, `crates/amux-server/src/reconciliation.rs:187-197`). Captured output is bounded, then passed through the shared redaction function; the result records exit code, elapsed time, timeout state, truncation, and redaction count (`crates/amux-server/src/reconciliation.rs:89-95`, `crates/amux-server/src/reconciliation.rs:169-185`). The temporary worktree has a drop guard that removes it and warns when cleanup fails (`crates/amux-server/src/reconciliation.rs:58-86`).

The reconciliation API already runs this blocking work away from the async executor, stores the candidate SHA and full gate result set, and emits a durable creation event (`crates/amux-server/src/api/reconciliation.rs:62-95`, `crates/amux-server/src/api/reconciliation.rs:98-151`). Green runs also update the last-known-green snapshot for the repository (`crates/amux-server/src/api/reconciliation.rs:116-129`). Promotion is deliberately separate, accepts only green runs, records the actor, and does not push (`crates/amux-server/src/api/reconciliation.rs:200-239`, `crates/amux-server/src/api/reconciliation.rs:247-284`). Command gates must not promote or merge anything.

### Board gate semantics

Core represents a gate as a destination status plus required or optional criteria (`crates/amux-core/src/board.rs:346-377`). A criterion maps its verifier to an evidence kind and is satisfied only when matching evidence exists (`crates/amux-core/src/board.rs:380-391`). `why_blocked` and transition enforcement share the same predicate, returning the criterion, missing evidence kind, and a suggested command when available (`crates/amux-core/src/board.rs:433-460`).

`VerifierKind` already includes command verification, maps it to `CommandOutput`, and exposes the command in why-blocked output (`crates/amux-core/src/verification.rs:50-69`, `crates/amux-core/src/verification.rs:102-138`). Evidence includes a description, optional inspectable artifact, production time, and provenance; missing provenance defaults to self-reported rather than independent (`crates/amux-core/src/verification.rs:141-197`). A server-run reconciliation result therefore has the right semantic shape for independent command evidence.

The persisted board resolves gates in this precedence order: card, worker, merged groups, column, then type default; every absent or malformed tier inherits rather than opening the gate (`crates/amux-server/src/db/board_store.rs:1185-1204`, `crates/amux-server/src/db/board_store.rs:1321-1400`). It records every consulted tier as applied, outranked, silent, or not applicable, and formats that trail into the card's append-only history (`crates/amux-server/src/db/board_store.rs:1236-1307`). Command policy should use this resolution and trail rather than inventing a parallel priority scheme.

Today persisted text criteria are wrapped as required `ModelJudgment` criteria, which makes `gate_checked` an acknowledgement rather than an executed check (`crates/amux-server/src/db/board_store.rs:1617-1640`). The API requires `gate_checked` to cover the resolved criterion list, rejects incomplete lists with the gate source and remediation, and converts an accepted acknowledgement into evidence (`crates/amux-server/src/api/board.rs:11279-11350`, `crates/amux-server/src/api/board.rs:11441-11454`). Multi-criterion verification rejects blanket `gate_ack` because its claims fail independently (`crates/amux-server/src/api/board.rs:11351-11438`). Command evidence must sit beside this mechanism and cannot be minted by either acknowledgement field.

`Force` is already the attributed bypass: core documents that it skips gates but must persist the named actor and reason (`crates/amux-core/src/board.rs:536-543`), while ordinary transitions check gates on entry (`crates/amux-core/src/board.rs:633-652`). Command gates should follow exactly that policy.

## Declaration model

Use two layers with distinct responsibilities.

1. **Repository command catalog.** A versioned `.amux/command-gates.json` maps stable IDs to the existing `GateCommand` shape. Example IDs are `rust-unit` and `dashboard-e2e`. The server reads the catalog from the trusted base commit, normally `origin/main`, never from the candidate PR head. A PR must not be able to weaken the command that judges that same PR.
2. **Board policy attachment.** Card, worker, group, column, and type policy may attach catalog IDs to destination statuses. Resolution follows the existing five-tier gate trail. A card-owned attachment may add a one-off gate but cannot remove a command required by a higher administrative scope. The resolved response includes source and scope so refusal UX can say who imposed it.

This split gives repositories portable commands without placing workflow policy in source code. Group settings can say that every code card entering `review` requires `rust-unit`; a card can add `dashboard-e2e`; and the same repository catalog works in another installation. No command contains a user path, host, or fork identity.

The catalog uses `program` plus `args`, not a shell string. V1 does not support inline environment values, interpolation, pipes, redirects, or command substitution. The only substitutions are server-owned tokens such as `${CANDIDATE_SHA}` passed as individual arguments. Unknown IDs and malformed catalogs fail closed.

## Candidate identity and execution flow

A command result is valid only for a tuple:

```
(repository identity, candidate SHA, destination status,
 resolved command-policy digest, command-catalog digest)
```

The PR artifact supplies repository identity, PR number, and current head SHA. The CLI refuses to infer a head from the worker's mutable checkout. If the card has no unambiguous PR artifact, the 409 response asks the worker to register one.

Execution is a two-phase flow so a long test never holds the board's SQLite writer:

1. `amux board gate-run <CARD> --to review` asks the server to resolve policy and the current PR head.
2. The server loads the catalog from the trusted base, computes both digests, and calls the reconciliation runner for the head SHA.
3. The server stores the reconciliation row and creates independent `CommandOutput` evidence whose artifact is the reconciliation ID. It also links that ID to the card and target status.
4. `amux board review` submits the run ID. Inside the transition transaction the server re-resolves the current PR head and policy digests. Any mismatch is stale evidence and fails closed.
5. The existing checked-criteria gate runs as usual. Entry succeeds only when both the human criteria and every required command gate pass.

The normal CLI makes this one user action: when a transition returns `command_gate_required`, it runs `gate-run` once, prints progress, then retries with the resulting ID. Claude and Codex lanes both use the same `amux board` commands, so provider-specific hooks or prompt conventions are unnecessary. Direct API clients can perform the same two calls.

Do not silently reuse a green result after the PR head moves. A retry on the same tuple may reuse the stored run; a changed SHA or policy digest always runs again. Concurrent transitions use the card revision plus tuple comparison so a green result cannot authorize a different head after the test completes.

## Runner hardening

Reusing the reconciliation runner requires one security change before board policy can invoke it. The current `Command::new` call does not clear inherited environment variables (`crates/amux-server/src/reconciliation.rs:152-157`). A repository test could read server credentials even though its output is redacted. Board command runs must use `env_clear()` and restore only a documented allowlist needed for builds, plus an isolated `HOME`, `TMPDIR`, and cache/target directories. Secrets and amux authentication headers are never passed.

Keep the existing detached worktree, timeout, kill-and-wait behavior, bounded capture, shared redaction, and cleanup warnings. Add a total run deadline and process-group termination so a timed-out command cannot leave descendants running. Network access remains an operator policy choice; V1 commands are trusted catalog entries, not arbitrary card text.

## Evidence and history

The transition stores a compact evidence line containing:

- command gate ID and label;
- candidate SHA and PR number;
- exit code, duration, timeout flag, and redaction/truncation counts;
- reconciliation ID linking to bounded output;
- source scope and policy digest.

The full output remains in the reconciliation record. Card history receives the existing gate authorization trail plus one `command-gate` line per run. This makes the transition explainable without copying large logs or secrets into the card.

Checked criteria remain claims that a worker explicitly acknowledges. Command results remain independently produced evidence. `gate_checked` and `gate_ack` cannot substitute for a missing, failed, timed-out, stale, or wrong-head command result. Conversely, a green command does not acknowledge review criteria such as “Ready for another set of eyes.”

An attributed `--force --reason ...` bypasses both gate classes, as it does today. The durable event and card history must include the failed or missing command IDs, the candidate SHA, actor, and reason. Force must never create a green reconciliation record or command evidence.

## Failure and timeout UX

A failed transition returns HTTP 409 with stable code `command_gate_blocked` and all failures in one response. Each entry contains the gate ID, label, candidate SHA, outcome (`missing`, `failed`, `timed_out`, or `stale`), exit code, duration, bounded redacted output, reconciliation ID when present, and an exact retry command.

The CLI prints a short summary first, then the last bounded output. Timeout text names the configured limit and confirms descendant cleanup. Redaction and truncation are visible counts, not silent omissions. A launch error distinguishes a missing executable from a test failure. The response also names the policy source, matching the existing gate-source UX.

Dashboard history should display the same stored result; it should not rerun commands. A later UI can add a “run gate” button, but the API and CLI must be complete without it.

## Compatibility and upstream suitability

The design is provider-neutral, repository-neutral, and installation-neutral. It uses relative repository files, structured commands, configured scope policy, and PR artifacts. It does not assume GitHub beyond the initial PR-head resolver interface; another forge can implement the same `(repo, change, head SHA)` contract. Fork policy chooses which commands attach to which transitions, while the runner, evidence model, and transition rule remain suitable for an upstream offer.

## Ordered implementation cards

Each card below is independently reviewable and capped at 400 changed lines and 10 files.

### 1. `feat(gates): define command gate catalog and resolved policy`

**Paths:**

- `crates/amux-core/src/board.rs`
- `crates/amux-core/src/verification.rs`
- `crates/amux-server/src/db/board_store.rs`
- `crates/amux-server/src/db/migrate.rs`
- `crates/amux-server/migrations/<next>_board_command_gates.sql`

**Acceptance:** Structured command gate references deserialize with unknown fields rejected; card/worker/group/column/type resolution reports the winning source and additive requirements; absent or malformed configuration inherits and fails closed; unit tests cover precedence, additive card gates, and destination-status selection.

### 2. `fix(reconciliation): sandbox reusable command gate runs`

**Paths:**

- `crates/amux-server/src/reconciliation.rs`
- `crates/amux-server/src/api/reconciliation.rs`
- `crates/amux-server/tests/reconciliation_command_gates.rs`

**Acceptance:** A gate runs at an exact detached SHA with a cleared allowlisted environment and isolated writable directories; timeout kills descendants; output remains bounded and redacted; result includes catalog and policy digests; focused tests cover pass, nonzero exit, launch failure, timeout, redaction, truncation, and cleanup.

### 3. `feat(board): record command gate runs as transition evidence`

**Paths:**

- `crates/amux-server/src/api/board.rs`
- `crates/amux-server/src/db/board_store.rs`
- `crates/amux-server/src/db/artifact_store.rs`
- `crates/amux-server/tests/board_command_gates.rs`

**Acceptance:** The gate-run endpoint resolves an unambiguous PR head and stores independent evidence; review/done/verified reject missing, failed, timed-out, stale, and wrong-head runs; a matching green run and all checked criteria permit entry; the writer rechecks card revision, head SHA, and policy digests; 409 bodies contain every blocked command and exact remediation.

### 4. `feat(cli): run required board command gates before transition retry`

**Paths:**

- `amux`
- `scripts/test-board-command-gates.sh`

**Acceptance:** `amux board review`, `done`, and `verified` recognize `command_gate_required`, run once, stream a concise status, and retry with the run ID; `--force` requires and records a reason; failure and timeout output preserve the server's redaction/truncation notices; shell tests cover Claude-style and Codex-style callers through identical commands.

### 5. `feat(dashboard): render board command gate evidence`

**Paths:**

- `crates/amux-dashboard/static/app.js`
- `crates/amux-dashboard/static/app.css`
- `crates/amux-dashboard/static/sw.js`
- `e2e/board-command-gates.test.mjs`

**Acceptance:** Card history shows gate label, exact head, result, duration, policy source, and reconciliation link without exposing unredacted output; stale and timed-out states are distinct; the UI never runs a command; Playwright covers green, failed, timed-out, and stale records.
