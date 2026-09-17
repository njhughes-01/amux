//! SQLite store: WAL mode, single-writer task, read pool, migrations,
//! global revision counter (RR-0019, Invariants 35/36).
//!
//! Concurrency design (plan §SQLite concurrency design):
//! - One dedicated writer thread owns the only write connection. Mutations
//!   arrive over an mpsc channel as closures; the writer applies each inside
//!   a transaction that ALSO bumps the global revision when the mutation
//!   reports itself as a real change. Python's GIL serialized writes by
//!   accident; this serializes them by construction, so `SQLITE_BUSY` cannot
//!   happen under load.
//! - Readers come from an r2d2 pool of read-only connections with a 5s busy
//!   timeout.
//! - The revision lives in `_amux_rev` (single row) and is returned from
//!   every mutation so SSE/delta-sync can publish revisioned StateEvents
//!   (Invariant 35).

pub mod advance;
pub mod artifact_store;
pub mod attempts;
pub mod board_store;
pub mod task_graph_store;
pub mod trace_store;
pub mod throughput_store;
pub mod commands;
pub mod harness_store;
pub mod interactions;
pub mod memories;
pub mod migrate;
pub mod queries;
pub mod replay;
pub mod telegram;
pub mod verification_store;
pub mod workflow_store;

use amux_core::revision::{MutationKind, StateEvent, StateRevision};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

pub type ReadPool = r2d2::Pool<SqliteConnectionManager>;

pub(crate) enum ProjectionRead {
    Dedicated(Connection),
    Pooled(r2d2::PooledConnection<SqliteConnectionManager>),
}

impl std::ops::Deref for ProjectionRead {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Dedicated(conn) => conn,
            Self::Pooled(conn) => conn,
        }
    }
}

/// What a write closure reports back: did it change anything, and what
/// StateEvents should be published if it did. `applied: false` writes do NOT
/// bump the revision (Invariant 37: no-op mutations must be visible as
/// no-ops, not disguised as changes).
pub struct WriteOutcome {
    pub applied: bool,
    pub events: Vec<PendingEvent>,
}

/// A StateEvent minus the revision, which the writer assigns at commit time
/// so event order and revision order can never disagree.
pub struct PendingEvent {
    pub entity_type: amux_core::revision::EntityType,
    pub entity_id: String,
    pub mutation: MutationKind,
    /// RR-0111a: the POST-MUTATION snapshot of the entity row, journaled in
    /// the same transaction as the mutation so state can be replayed from
    /// events alone (plan Invariant 24, EventPayload::Inline). The row is in
    /// the writer's hand when the event is built, so a snapshot costs one
    /// serialization, never a re-read.
    ///
    /// `None` is honest, not lazy: it means this event records THAT the
    /// entity changed, without the state it changed into. Replay
    /// (`db::replay`) reports such entities under `pre_payload_horizon`
    /// instead of pretending an older snapshot is current. Worker and board
    /// (task) mutations populate this; other sites may stay `None` until
    /// their entities need replay.
    pub payload: Option<serde_json::Value>,
}

type WriteFn = Box<dyn FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send>;

struct WriteRequest {
    work: WriteFn,
    origin: &'static str,
    queued_at: std::time::Instant,
    interaction_id: Option<String>,
    reply: mpsc::Sender<rusqlite::Result<WriteReply>>,
}

pub struct WriteReply {
    pub applied: bool,
    pub rev: StateRevision,
    pub events: Vec<StateEvent>,
}

/// Handle to the store: cheap to clone, shared across the router and
/// background jobs.
#[derive(Clone)]
pub struct Store {
    write_tx: mpsc::Sender<WriteRequest>,
    /// `pub(crate)` so a test outside this module can put the pool under real
    /// saturation. The property "a request path takes no blocking acquire" is
    /// only testable by holding every connection, and the paths that must hold
    /// it (`api::policy::enforce`) live in other modules.
    pub(crate) read_pool: ReadPool,
    db_path: Arc<std::path::PathBuf>,
    pub(crate) health_probe: Arc<tokio::sync::Semaphore>,
    pub(crate) health_probe_started: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) health_probe_last_success: Arc<std::sync::atomic::AtomicU64>,
    /// Writes submitted to the writer thread and not yet answered (AMUX-4744).
    ///
    /// The writer is ONE thread behind an unbounded channel and `write_correlated`
    /// waits on `recv()` with no timeout, so a slow write delays every write
    /// behind it by an unbounded amount while reads are untouched. That
    /// asymmetry is invisible today: nothing times the wait and nothing counts
    /// the queue, so an 80s POST produces no log line at all.
    pub(crate) write_inflight: Arc<std::sync::atomic::AtomicUsize>,
    /// Longest write wait observed since start, in milliseconds. A gauge that
    /// only rises, so a stall that has already ended is still reportable.
    pub(crate) write_wait_max_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Longest wait for a `spawn_blocking` thread, milliseconds, rising only.
    /// See `record_blocking_dispatch`: this is the one number on the write path
    /// that is not measured from a thread we already hold.
    pub(crate) blocking_dispatch_max_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Broadcast of committed StateEvents for SSE fan-out.
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,
}

/// A write wait past this is reported. A healthy write on this box is
/// sub-millisecond; the stalls on AMUX-4744 were 62s to 84s. One second is far
/// enough above normal contention to stay quiet and far enough below the
/// observed failures to catch all of them.
pub(crate) const WRITE_WAIT_WARN_MS: u64 = 1_000;

/// Record how long a `spawn_blocking` task waited to be given a thread, and say
/// so when it is long enough to be the reason a request is hanging.
///
/// Getting a blocking thread is normally instant. A large value here means the
/// blocking pool is the bottleneck, which is a DIFFERENT fault from a slow
/// query or a busy writer and has a different fix, so it gets its own verdict
/// rather than being folded into `writer_slow`.
pub(crate) fn record_blocking_dispatch(
    gauge: &std::sync::atomic::AtomicU64,
    waited: std::time::Duration,
) {
    let ms = waited.as_millis() as u64;
    gauge.fetch_max(ms, std::sync::atomic::Ordering::Relaxed);
    if ms >= WRITE_WAIT_WARN_MS {
        tracing::warn!(
            target: "store",
            verdict = "blocking_pool_saturated",
            waited_ms = ms,
            measured = true,
            n_considered = 1,
            "a db task waited for a blocking thread; the pool, not the query, is the delay"
        );
    }
}

impl Store {
    /// Open the store: apply migrations, start the writer thread, build the
    /// read pool.
    pub fn open(db_path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Migrations run on a dedicated connection before anything else may
        // touch the DB. Health returns 503 until `open` completes.
        let mut conn = Connection::open(db_path)?;
        configure_connection(&conn)?;
        migrate::apply_all_guarded(&mut conn, db_path)?;

        let (write_tx, write_rx) = mpsc::channel::<WriteRequest>();
        let (events_tx, _) = tokio::sync::broadcast::channel(4096);
        let events_for_writer = events_tx.clone();

        // The writer thread. Plain OS thread, not a tokio task: rusqlite is
        // synchronous and a blocked writer must never stall the async
        // runtime's worker pool.
        std::thread::Builder::new()
            .name("amux-writer".into())
            .spawn(move || writer_loop(conn, write_rx, events_for_writer))
            .expect("spawn writer thread");

        let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
            configure_connection(c)?;
            // Readers never write; enforce it so a bug cannot sneak a write
            // past the single-writer discipline.
            c.pragma_update(None, "query_only", "ON")?;
            Ok(())
        });
        let read_pool = r2d2::Pool::builder()
            .max_size(std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4))
            // FAIL FAST, because a blocked acquire pins a tokio worker (AF-640).
            //
            // r2d2's default is 30 SECONDS and it was never set, which is why
            // the 2026-09-08 outage produced rows at exactly 30032, 30100 and
            // 30104 ms: sixteen 500s in 22 minutes, every one a caller that
            // waited half a minute to be told no.
            //
            // WHY WAITING IS WORSE THAN FAILING HERE. `read()` is synchronous
            // and there is no `read_async` to match `write_async`, whose own
            // doc says it exists so a handler "can await a write without
            // pinning a runtime worker". So every one of the ~440 `read()` call
            // sites blocks its thread for the whole acquire. The pool's
            // max_size is `available_parallelism`, which is ALSO tokio's default
            // worker count, so a saturated pool can pin every worker at once
            // and each one holds for 30s. That is self-sustaining, which is why
            // it lasted 22 minutes and recurred five more times that day.
            //
            // A healthy acquire is microseconds. Anything approaching seconds
            // means the pool is already saturated, and a caller that waits
            // longer does not make a connection appear; it just holds a worker
            // that could be shedding load. Five seconds keeps a generous margin
            // over any legitimate contention while cutting the pin by 6x.
            .connection_timeout(std::time::Duration::from_secs(5))
            // AND THE SAME KNOB GOVERNS POOL STARTUP, which the paragraph above
            // did not account for (AMUX-4739).
            //
            // `Pool::build` calls `wait_for_initialization`, which waits for
            // `min_idle.unwrap_or(max_size)` connections to exist and bounds
            // that wait by THIS timeout (r2d2 0.8.10 lib.rs:391-395). min_idle
            // was unset, so opening a store waited for `available_parallelism`
            // connections. Cutting 30s -> 5s to bound acquisition therefore made
            // startup six times more likely to fail outright, and it fails as
            // `Store::open` returning Err("timed out waiting for connection"),
            // which reads as a broken database rather than a busy one.
            //
            // MEASURED, 2026-09-17: a full `cargo test -p amux-server --lib` on
            // an unmodified origin/main produced 39 of these and 43 failed tests
            // across modules that share nothing but this call. A second run
            // failed a DIFFERENT 29, which is what made it look like unrelated
            // flakiness for as long as it did.
            //
            // One connection is enough to serve the first reader; the pool still
            // grows to max_size on demand. This changes what `open` WAITS FOR,
            // not how many connections a loaded server ends up with.
            .min_idle(Some(1))
            .build(manager)?;

        Ok(Store {
            write_tx,
            read_pool,
            db_path: Arc::new(db_path.to_path_buf()),
            health_probe: Arc::new(tokio::sync::Semaphore::new(1)),
            health_probe_started: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            health_probe_last_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            write_inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            write_wait_max_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            blocking_dispatch_max_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            events_tx,
        })
    }

    /// Run a mutation on the writer thread and wait for commit. Returns the
    /// revision assigned to this write (unchanged if the write was a no-op).
    pub fn write<F>(&self, f: F) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        self.write_correlated(f, interactions::current_id())
    }

    fn write_correlated<F>(&self, f: F, interaction_id: Option<String>) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        use std::sync::atomic::Ordering;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.write_inflight.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let sent = self
            .write_tx
            .send(WriteRequest {
                work: Box::new(f),
                origin: std::any::type_name::<F>(),
                queued_at: std::time::Instant::now(),
                interaction_id,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("writer thread is gone"));
        let out = match sent {
            Ok(()) => reply_rx
                .recv()
                .map_err(anyhow::Error::from)
                .and_then(|r| r.map_err(anyhow::Error::from)),
            Err(e) => Err(e),
        };
        self.write_inflight.fetch_sub(1, Ordering::Relaxed);

        // THE GAUGE, NOT A SECOND WARNING (AMUX-4744). `recv()` above has no
        // timeout, so an 80-second write used to produce exactly as much output
        // as a fast one: none.
        //
        // The LOG half of that gap is already closed, by `writer_slow` in
        // `writer_loop` (1f0cca3e, codex-board-execution-contract), which lands
        // the same minute as this and is strictly better placed: it carries
        // `origin`, the type name of the blocking closure, so it NAMES the slow
        // mutation instead of only reporting that something was slow. A second
        // warn here would fire on the same event with less information, and two
        // lines per stall is how a verdict becomes noise a sweep learns to skip.
        //
        // What that warn cannot answer is "is it happening RIGHT NOW, and how
        // deep", because a log line is a record of something already over.
        // These two atomics are readable on /api/health at any instant, which
        // is where someone looks while a POST is hanging in front of them.
        let waited_ms = started.elapsed().as_millis() as u64;
        self.write_wait_max_ms.fetch_max(waited_ms, Ordering::Relaxed);
        out
    }

    /// Async wrapper: parks the wait on the blocking pool so an API handler
    /// can await a write without pinning a runtime worker.
    pub async fn write_async<F>(&self, f: F) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        let this = self.clone();
        let interaction_id = interactions::current_id();
        let dispatch = self.blocking_dispatch_max_ms.clone();
        let queued = std::time::Instant::now();
        tokio::task::spawn_blocking(move || {
            // TIME SPENT WAITING FOR A BLOCKING THREAD, which every other
            // instrument on this path is structurally blind to (AMUX-4744).
            //
            // `writer_slow` and `write_wait_max_ms` are both measured INSIDE
            // `write_correlated`, which by then is already running on a blocking
            // thread. Neither can see the wait to GET that thread. So if the
            // blocking pool is saturated, a request stalls for a minute and
            // every existing verdict stays silent and truthful.
            //
            // That makes this the discriminator rather than another counter:
            // a 90s request with a small `queued_ms` and a large value here
            // means the writer was never the problem.
            record_blocking_dispatch(&dispatch, queued.elapsed());
            this.write_correlated(f, interaction_id)
        })
        .await?
    }

    /// Run a read WITHOUT pinning a runtime worker (AF-640 / AMUX-4744).
    ///
    /// THE MISSING HALF THAT AF-640 NAMES. The comment on the read pool's
    /// `connection_timeout` says it plainly: "`read()` is synchronous and there
    /// is no `read_async` to match `write_async`, whose own doc says it exists
    /// so a handler can await a write without pinning a runtime worker. So
    /// every one of the ~440 `read()` call sites blocks its thread for the
    /// whole acquire."
    ///
    /// That is self-sustaining, and the same comment says why: the pool's
    /// `max_size` is `available_parallelism`, which is ALSO tokio's default
    /// worker count, so a saturated pool can pin every worker at once. The
    /// 2026-09-08 outage ran 22 minutes and recurred five times that day. The
    /// timeout was cut from 30s to 5s, which bounds the pin; it does not remove
    /// it. This removes it, for callers that can await.
    ///
    /// The acquire happens on the BLOCKING pool, where blocking is what the
    /// threads are for, so a saturated read pool costs latency instead of
    /// costing the runtime its ability to poll anything else.
    ///
    /// ADDITIVE ON PURPOSE. `read()` keeps working and keeps its slow-acquire
    /// warning; converting ~440 call sites in one change is not a reviewable
    /// diff and most of them are on background jobs that already run on the
    /// maintenance runtime (AMUX-4225) where a pinned thread costs far less.
    /// The callers worth moving are the ones on the request path.
    pub async fn read_async<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        let dispatch = self.blocking_dispatch_max_ms.clone();
        let queued = std::time::Instant::now();
        tokio::task::spawn_blocking(move || {
            // Same blind spot as `write_async`: everything below this line runs
            // on a blocking thread, so nothing below can measure the wait to be
            // GIVEN one. Reads only started paying this cost when `read_async`
            // was introduced, so if the pool is the bottleneck, that change
            // moved reads into the same queue as writes rather than out of it.
            record_blocking_dispatch(&dispatch, queued.elapsed());
            let conn = this.read()?;
            f(&conn)
        })
        .await?
    }

    /// A read acquire this slow means the pool is already saturated. Well under
    /// `connection_timeout` so the warning arrives BEFORE the failures do, which
    /// is the difference between a signal and a post-mortem.
    const SLOW_ACQUIRE: std::time::Duration = std::time::Duration::from_millis(250);

    /// Borrow a read-only connection from the pool.
    ///
    /// SAYS WHEN IT IS SLOW, because the only signal the 2026-09-08 exhaustion
    /// left was a 30-second 500 with `timed out waiting for connection` and no
    /// pool state beside it (AF-640). "How many connections were out, and how
    /// many were idle" is the first question anyone asks and nothing recorded
    /// it, so the cause had to be reconstructed from the source afterwards.
    ///
    /// Silent on the happy path: a healthy acquire is microseconds, so the
    /// threshold below is never reached in normal operation and this stays off
    /// a hot path rather than logging 200k times a day.
    pub fn read(&self) -> anyhow::Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        let t0 = std::time::Instant::now();
        let got = self.read_pool.get();
        let waited = t0.elapsed();
        match got {
            Ok(conn) => {
                if waited >= Self::SLOW_ACQUIRE {
                    let st = self.read_pool.state();
                    tracing::warn!(
                        verdict = "read_pool_slow_acquire",
                        waited_ms = waited.as_millis() as u64,
                        connections = st.connections,
                        idle = st.idle_connections,
                        max_size = self.read_pool.max_size(),
                        "read pool acquire was slow; the pool is saturated and every waiter                          is pinning a thread (AF-640)"
                    );
                }
                Ok(conn)
            }
            Err(e) => {
                let st = self.read_pool.state();
                tracing::warn!(
                    verdict = "read_pool_exhausted",
                    waited_ms = waited.as_millis() as u64,
                    connections = st.connections,
                    idle = st.idle_connections,
                    max_size = self.read_pool.max_size(),
                    error = %e,
                    "read pool acquire FAILED; callers are getting 500s (AF-640)"
                );
                Err(e.into())
            }
        }
    }

    /// Health must report pool exhaustion without waiting behind fleet probes.
    pub fn try_read(&self) -> Option<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.read_pool.try_get()
    }

    /// Open a read-only connection outside the request pool for a bounded,
    /// heavyweight projection.
    ///
    /// The sessions projection deliberately shells out while it assembles its
    /// answer. Even with one build in flight, lending that work one of the
    /// request pool's connections makes unrelated, cheap API reads wait behind
    /// tmux/git. A dedicated reader keeps the pool available while preserving
    /// SQLite's WAL snapshot semantics; callers must still single-flight and
    /// bound their external work.
    pub(crate) fn dedicated_read(&self) -> anyhow::Result<ProjectionRead> {
        // SQLite gives each `:memory:` connection an independent database, so
        // a new connection would silently see an empty store. Preserve the
        // previous pooled behavior for that test/development configuration.
        if self.db_path.as_path() == Path::new(":memory:") {
            return Ok(ProjectionRead::Pooled(self.read()?));
        }
        let conn = Connection::open_with_flags(
            self.db_path.as_ref(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "query_only", "ON")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(ProjectionRead::Dedicated(conn))
    }

    /// Current global revision.
    pub fn current_rev(&self) -> anyhow::Result<StateRevision> {
        let conn = self.read()?;
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        Ok(StateRevision(rev))
    }

    /// Subscribe to committed StateEvents (SSE fan-out).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StateEvent> {
        self.events_tx.subscribe()
    }

    /// StateEvents since a revision, for delta sync (RR-0024). Returns
    /// (events, full_sync_required): when the requested window is no longer
    /// in the event journal the client must full-sync rather than trust a
    /// silently incomplete delta (Invariant 40 — an omission must announce
    /// itself).
    pub fn events_since(&self, since: StateRevision, limit: usize) -> anyhow::Result<(Vec<StateEvent>, bool)> {
        let conn = self.read()?;
        let oldest: Option<u64> = conn
            .query_row("SELECT MIN(rev) FROM _amux_state_events", [], |r| r.get(0))
            .unwrap_or(None);
        // Gap check: if the journal's oldest retained event is newer than
        // since+1 and the client is behind that, the delta would be missing
        // events it has no way to detect.
        if let Some(oldest) = oldest {
            if since.0 + 1 < oldest {
                return Ok((vec![], true));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT rev, entity_type, entity_id, mutation, at FROM _amux_state_events
             WHERE rev > ?1 ORDER BY rev ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![since.0, limit as i64], |r| {
            let rev: u64 = r.get(0)?;
            let entity_type: String = r.get(1)?;
            let entity_id: String = r.get(2)?;
            let mutation: String = r.get(3)?;
            let at: String = r.get(4)?;
            Ok((rev, entity_type, entity_id, mutation, at))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (rev, entity_type, entity_id, mutation, at) = row?;
            events.push(StateEvent {
                rev: StateRevision(rev),
                entity_type: parse_entity_type(&entity_type),
                entity_id,
                mutation: serde_json::from_str(&mutation)
                    .unwrap_or(MutationKind::Updated),
                at: at.parse().unwrap_or_default(),
            });
        }
        Ok((events, false))
    }
}

/// Parse a stored `entity_type` column back into the enum: the bare tag
/// ("worker", "fleet_progress" — the current storage format), with tolerance
/// for the legacy adjacently-tagged object ({"kind":"worker"} /
/// {"kind":"other","data":"x"}) that rows written before the bare-tag fix
/// still carry. Unknown tags land in `Other(tag)` — the open-enum contract.
/// (The previous reader wrapped the raw value in quotes and fed it to serde,
/// which CANNOT parse an adjacently-tagged unit variant from a JSON string —
/// so every event round-tripped as Other(...), for typed variants too.)
fn parse_entity_type(raw: &str) -> amux_core::revision::EntityType {
    use amux_core::revision::EntityType;
    if raw.starts_with('{') {
        if let Ok(t) = serde_json::from_str::<EntityType>(raw) {
            return t;
        }
    }
    serde_json::from_str::<EntityType>(&format!("{{\"kind\":\"{raw}\"}}"))
        .unwrap_or_else(|_| EntityType::Other(raw.to_string()))
}

fn configure_connection(c: &Connection) -> rusqlite::Result<()> {
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn writer_loop(
    conn: Connection,
    rx: mpsc::Receiver<WriteRequest>,
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,
) {
    while let Ok(req) = rx.recv() {
        let queued_ms = req.queued_at.elapsed().as_millis() as u64;
        let started = std::time::Instant::now();
        // A panicking caller must not kill the sole writer and strand every
        // later mutation. The transaction guard rolls back during unwinding.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply_write(&conn, req.work, &events_tx, req.interaction_id.as_deref())
        })).unwrap_or_else(|_| {
            tracing::error!(target: "store", verdict = "writer_mutation_panicked",
                "mutation panicked; transaction rolled back, writer remains available");
            Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other("writer mutation panicked; transaction rolled back"))))
        });
        let work_ms = started.elapsed().as_millis() as u64;
        if work_ms >= 250 || queued_ms >= 1000 {
            // Function identity only; never record the request body or SQL
            // values. Separate the slow writer from callers waiting behind it.
            tracing::warn!(target: "store", verdict = "writer_slow", origin = req.origin,
                queued_ms, work_ms, ok = result.is_ok(), measured = true, n_considered = 1,
                "serialized write delayed; origin identifies the blocking mutation");
        }
        if let Err(error) = &result {
            tracing::warn!(target: "store", verdict = "writer_mutation_failed", %error,
                autocommit = conn.is_autocommit(), "mutation failed; no acknowledgement was issued");
        }
        // A dropped reply receiver just means the caller gave up waiting;
        // the write itself has already committed either way.
        let _ = req.reply.send(result);
    }
    // Channel closed = Store dropped = shutdown. Nothing to clean up: WAL
    // checkpoints on connection close.
}

fn apply_write(
    conn: &Connection,
    work: WriteFn,
    events_tx: &tokio::sync::broadcast::Sender<StateEvent>,
    interaction_id: Option<&str>,
) -> rusqlite::Result<WriteReply> {
    // Roll back EVERY failure path, including revision/event writes, failed
    // COMMIT and unwinding. A bare BEGIN left the connection in a transaction
    // after those errors, making all later mutations fail until restart.
    let transaction = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let outcome = work(&transaction)?;
    let mut committed_events = Vec::new();
    let rev = if outcome.applied {
        // Bump the global revision once per applied transaction; every event
        // from this transaction shares the revision, which is what makes
        // "give me everything after rev N" exact.
        conn.execute("UPDATE _amux_rev SET rev = rev + 1 WHERE id = 1", [])?;
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        let now = chrono::Utc::now();
        if let Some(id) = interaction_id {
            conn.execute("UPDATE _amux_interactions SET applied_writes=applied_writes+1,
                unjournaled_writes=unjournaled_writes+?2, updated_at=?3 WHERE id=?1",
                rusqlite::params![id, i64::from(outcome.events.is_empty()), now.timestamp_millis()])?;
        }
        for ev in outcome.events {
            // The COLUMN stores the BARE tag ("worker", "task",
            // "fleet_progress"), never serde's adjacently-tagged object.
            // Three consumers filter on `entity_type = '<tag>'` — the
            // redistribute dedupe, /api/metrics/fleet's last-event lookups,
            // and the breaker's window_stats — and all three silently
            // matched NOTHING while this column held {"kind":"task"}
            // (the previous trim_matches('"') stripped quotes from a shape
            // serde never produces for this enum; caught by RR-0111a's
            // replay work + the redistribute dedupe test). Old rows in
            // existing DBs may still carry the object shape, so READERS
            // stay tolerant of both: parse_entity_type below,
            // db::replay::entity_tag.
            let entity_type_str = match &ev.entity_type {
                amux_core::revision::EntityType::Other(s) => s.clone(),
                t => serde_json::to_value(t)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "other".into()),
            };
            let mutation_json = serde_json::to_string(&ev.mutation).unwrap_or_default();
            // Snapshot rides in the same INSERT as the event it describes —
            // journal row and payload cannot disagree about which transaction
            // produced them (RR-0111a).
            let payload_json = ev.payload.as_ref().map(|p| p.to_string());
            conn.execute(
                "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![rev, entity_type_str, ev.entity_id, mutation_json, now.to_rfc3339(), payload_json],
            )?;
            if let Some(id) = interaction_id {
                conn.execute("INSERT INTO _amux_interaction_effects (interaction_id,event_id,kind,entity_kind,entity_id,rev)
                    VALUES (?1,?2,?3,?4,?5,?6)", rusqlite::params![id, conn.last_insert_rowid(),
                        mutation_json, entity_type_str, ev.entity_id, rev])?;
            }
            committed_events.push(StateEvent {
                rev: StateRevision(rev),
                entity_type: ev.entity_type,
                entity_id: ev.entity_id,
                mutation: ev.mutation,
                at: now,
            });
        }
        StateRevision(rev)
    } else {
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        StateRevision(rev)
    };
    transaction.commit()?;
    // Publish only after commit: a subscriber must never see an event whose
    // transaction later rolled back.
    for ev in &committed_events {
        let _ = events_tx.send(ev.clone());
    }
    Ok(WriteReply {
        applied: outcome.applied,
        rev,
        events: committed_events,
    })
}

/// Shared handle used by API state.
pub type SharedStore = Arc<Store>;

#[cfg(test)]
mod amux4739_pool_startup_tests {
    use super::*;

    /// AMUX-4739: opening the store must not wait for a FULL read pool.
    ///
    /// `Pool::build` waits for `min_idle.unwrap_or(max_size)` connections and
    /// bounds that wait by `connection_timeout` (r2d2 0.8.10, lib.rs:391-395).
    /// With min_idle unset that is every connection, so AF-640's 5s acquisition
    /// timeout silently became a 5s STARTUP budget for `available_parallelism`
    /// connections.
    ///
    /// The failure mode is the expensive part: `Store::open` returns
    /// Err("timed out waiting for connection"), so a busy machine presents as a
    /// broken database. Measured on an unmodified origin/main, one full lib run
    /// produced 39 of these across modules sharing nothing but this call, and a
    /// second run failed a different set, which is why it read as flakiness.
    #[test]
    fn opening_the_store_waits_for_one_connection_not_the_whole_pool() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("startup.db")).unwrap();

        let min_idle = store.read_pool.min_idle();
        let max_size = store.read_pool.max_size();
        assert_eq!(
            min_idle,
            Some(1),
            "open must block on a single connection; None means max_size ({max_size}) \
             connections inside the {:?} connection_timeout",
            store.read_pool.connection_timeout()
        );
        // The point is the RELATIONSHIP, not the literal. A min_idle equal to
        // max_size would satisfy "is set" while restoring the whole defect.
        assert!(
            min_idle.is_some_and(|m| m < max_size.max(2)),
            "min_idle {min_idle:?} is not below max_size {max_size}; startup would still \
             wait for the full pool"
        );
        // The pool must still be ABLE to grow, or this traded a startup stall
        // for a permanent one-connection bottleneck.
        assert!(
            max_size > 1,
            "max_size {max_size} leaves no room to grow beyond the startup minimum"
        );
    }

    /// The acquisition bound AF-640 set must survive this change. Startup and
    /// acquisition read the same field, so it is exactly the kind of pair where
    /// fixing one silently relaxes the other.
    #[test]
    fn the_acquisition_timeout_af640_set_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("timeout.db")).unwrap();
        assert_eq!(
            store.read_pool.connection_timeout(),
            std::time::Duration::from_secs(5),
            "AF-640 cut this from 30s to 5s because a blocked acquire pins a tokio \
             worker; raising it to make startup easier would undo that"
        );
    }
}

#[cfg(test)]
mod amux4744_write_queue_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// AMUX-4744: a write that waits behind the writer thread must be
    /// MEASURABLE afterwards.
    ///
    /// The writer is one OS thread behind an unbounded channel, and
    /// `write_correlated` waits on `recv()` with no timeout. So a slow write
    /// delays every write behind it without bound while reads, which never
    /// touch this path, keep answering at full speed. Measured live on build
    /// 24716ccf: 20 paired samples gave POSTs of 81.4s and 83.8s while every
    /// GET in the same loop returned in ~8ms.
    ///
    /// Before this the wait produced NO output at all. Nothing timed it and
    /// nothing counted the queue, so the only instrument was a human holding a
    /// stopwatch on the client, which is how the stall survived five rounds of
    /// elimination.
    ///
    /// THE GAUGE MUST SURVIVE THE STALL IT RECORDS. `write_wait_max_ms` only
    /// rises, because a decaying gauge reads zero exactly when someone arrives
    /// to look at it, and a zero that means "recovered" is indistinguishable
    /// from a zero that means "never happened" (ethos rule 4).
    #[test]
    fn a_write_that_queues_behind_a_slow_one_is_measurable_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("wq.db")).unwrap();
        assert_eq!(
            store.write_wait_max_ms.load(Ordering::Relaxed),
            0,
            "a fresh store has recorded no wait; otherwise this cell cannot \
             tell its own write from leftover state"
        );

        // Occupy the writer for longer than the warn threshold. This is the
        // real writer thread and a real queued write, not a simulated delay.
        let hold = std::time::Duration::from_millis(WRITE_WAIT_WARN_MS + 400);
        let blocker = {
            let s = store.clone();
            std::thread::spawn(move || {
                s.write(move |_conn| {
                    std::thread::sleep(hold);
                    Ok(WriteOutcome { applied: false, events: vec![] })
                })
            })
        };
        // Let the slow write reach the writer before queueing behind it.
        std::thread::sleep(std::time::Duration::from_millis(150));

        let started = std::time::Instant::now();
        store
            .write(|conn| {
                conn.execute_batch("CREATE TABLE IF NOT EXISTS wq_probe (id INTEGER)")?;
                Ok(WriteOutcome { applied: false, events: vec![] })
            })
            .expect("the queued write still completes");
        let observed = started.elapsed();
        blocker.join().expect("blocker joins").expect("blocker write");

        assert!(
            observed >= std::time::Duration::from_millis(WRITE_WAIT_WARN_MS),
            "the second write did not actually queue ({observed:?}); the cell would \
             then be asserting about a gauge nothing exercised"
        );
        let recorded = store.write_wait_max_ms.load(Ordering::Relaxed);
        assert!(
            recorded >= WRITE_WAIT_WARN_MS,
            "a write waited {observed:?} and the store reports a maximum of \
             {recorded}ms; the wait is still invisible"
        );
        // The queue drains: a gauge that pinned in-flight high would report a
        // permanent stall on a healthy server.
        assert_eq!(
            store.write_inflight.load(Ordering::Relaxed),
            0,
            "in-flight must return to zero once writes are answered"
        );
    }

    /// The DIAGNOSTIC half. The gauge test above stays green if the warn is
    /// deleted, because an atomic and a log line are independent, and amux's
    /// two-fix rule asks for the log signal specifically: a fix with no
    /// counter, WARN or verdict field cannot announce its own regression.
    ///
    /// THIS PINS A PEER'S WARN, NOT ONE OF MY OWN, deliberately. `writer_slow`
    /// (1f0cca3e, codex-board-execution-contract) landed on this path the same
    /// minute as these gauges and is better placed than a caller-side warn:
    /// it carries `origin`, the type name of the blocking closure, so it names
    /// WHICH mutation held the writer. I dropped my duplicate rather than emit
    /// two lines per stall, which leaves these gauges depending on a verdict I
    /// do not own. Hence a test: nothing else here would notice if a later edit
    /// dropped `origin` and left a bare "something was slow".
    #[test]
    fn a_slow_write_reports_itself_under_a_greppable_verdict() {
        let src = include_str!("mod.rs");
        let body = src
            .split_once("\nfn writer_loop(")
            .expect("writer_loop exists")
            .1;
        // BOUND IT. An unbounded window sweeps past the function into the test
        // module below, which contains these same literals in its own asserts
        // and doc comments, and the scan then passes by matching itself. That
        // trap fired three separate times in one day across two repos.
        let body = body.split_once("\n}\n").expect("its closing brace").0;

        // STRIP COMMENTS BEFORE ASSERTING ANYTHING. A source scan that reads
        // prose passes on the description of the code instead of the code, and
        // it is invisible because the description is usually accurate.
        //
        // Measured, this cell, today: asserting `body.contains("origin")`
        // survived a mutation that deleted `origin = req.origin` outright,
        // because the line's own comment says "origin identifies the blocking
        // mutation". The scan matched the sentence explaining the field while
        // the field was gone. That is the fourth variant of this trap in a day
        // (AMUX-4720, an ugrep window, a 3000-char sweep, this), so it is
        // handled structurally here rather than by picking better literals.
        let body: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // LANDMARK FIRST: prove the window is the function, not some other
        // region that happens to mention the same words. Two of them, because
        // the first landmark I picked ("reply_rx.recv()") stopped existing the
        // moment the expression was split across lines, and a landmark that
        // breaks on formatting gets deleted as flaky rather than trusted.
        assert!(
            body.contains("rx.recv()") && body.contains("catch_unwind"),
            "the scan is not reading writer_loop; it has {} chars of something else",
            body.len()
        );
        assert!(
            body.contains("writer_slow"),
            "a delayed serialized write must report a greppable verdict; \
             writer_loop no longer names writer_slow"
        );
        // A verdict with no numbers says something was slow and not how slow,
        // and without `origin` it cannot say WHAT was slow, which is the field
        // that turns this line into a lead instead of a notification.
        for field in ["queued_ms", "work_ms", "origin = req.origin"] {
            assert!(
                body.contains(field),
                "the writer_slow verdict must carry `{field}`; without it the line \
                 reports that a stall happened and not what caused it"
            );
        }
    }

    /// The threshold has to be crossable in the direction that matters. A warn
    /// that fires on every write is noise a sweep learns to ignore.
    #[test]
    fn a_fast_write_reports_no_wait_and_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("wq2.db")).unwrap();
        for _ in 0..20 {
            store
                .write(|_conn| Ok(WriteOutcome { applied: false, events: vec![] }))
                .expect("write");
        }
        let recorded = store.write_wait_max_ms.load(Ordering::Relaxed);
        assert!(
            recorded < WRITE_WAIT_WARN_MS,
            "20 trivial writes recorded a {recorded}ms maximum, at or above the \
             {WRITE_WAIT_WARN_MS}ms warn threshold; the signal would fire constantly"
        );
    }
}

#[cfg(test)]
mod af640_read_pool_tests {
    use super::*;

    /// AF-640 / AMUX-4744: a read must not pin the runtime worker that awaits it.
    ///
    /// The pool's `max_size` is `available_parallelism`, which is ALSO tokio's
    /// default worker count, so a saturated pool could pin every worker at once.
    /// That is what made the 2026-09-08 exhaustion self-sustaining for 22
    /// minutes and recur five times the same day. Cutting the timeout 30s -> 5s
    /// bounded the pin; `read_async` removes it for callers that can await.
    ///
    /// SINGLE-WORKER RUNTIME ON PURPOSE. With one worker thread a blocking
    /// acquire makes everything else on that runtime unrunnable, so this cell
    /// cannot pass by falling back on a spare worker.
    #[test]
    fn a_read_async_leaves_the_runtime_worker_free_to_poll() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("ra.db")).unwrap();

        rt.block_on(async move {
            // Hold EVERY read connection, so any further acquire must wait.
            let held: Vec<_> = (0..store.read_pool.max_size())
                .map(|_| store.read_pool.get().expect("prefill"))
                .collect();

            let s2 = store.clone();
            let reader = tokio::spawn(async move {
                s2.read_async(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)).await
            });

            // THE POINT: while that read waits for a connection, the single
            // runtime worker must still poll something else. Before read_async
            // this task could not be polled at all, because the blocked acquire
            // owned the worker.
            let ticked = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                tokio::spawn(async { 42u8 }),
            )
            .await
            .expect("the runtime worker was pinned by a blocked read acquire")
            .expect("join");
            assert_eq!(ticked, 42);

            drop(held);
            let got = reader.await.expect("join");
            assert_eq!(got.unwrap(), 1, "the read still returns once a connection frees");
        });
    }

    /// The DIAGNOSTIC half, which the timeout test does not cover: mutating the
    /// warn away leaves that cell green, because a pool can fail fast and say
    /// nothing about why.
    ///
    /// `read_pool_exhausted` is the string the health payload already reports
    /// and the one a log sweep greps for, so it is the name that has to survive
    /// a rename, not just the presence of some warning.
    #[test]
    fn a_saturated_pool_reports_its_state_under_greppable_verdicts() {
        let src = include_str!("mod.rs");
        let body = src
            .split_once("\n    pub fn read(&self)")
            .expect("Store::read exists")
            .1;
        let body = body.split_once("\n    }\n").expect("its closing brace").0;

        // LANDMARK FIRST: prove the scan is reading `read`, not some other
        // region. Anchoring on a name that also appears quoted elsewhere has
        // silently read the wrong block three times today.
        assert!(
            body.contains("self.read_pool.get()"),
            "the scan is not reading Store::read; it has {} chars of something else",
            body.len()
        );

        for verdict in ["read_pool_exhausted", "read_pool_slow_acquire"] {
            assert!(
                body.contains(&format!("verdict = \"{verdict}\"")),
                "a saturated pool must report under `{verdict}`, which is what the health \
                 payload uses and what a sweep greps for"
            );
        }
        // The state is what makes it diagnosable. A verdict with no numbers is
        // the 30-second 500 again, wearing a better name.
        for field in ["connections", "idle", "max_size", "waited_ms"] {
            assert!(
                body.contains(&format!("{field} =")),
                "the warn must carry `{field}`; without it nobody can tell a saturated \
                 pool from a slow query"
            );
        }
    }

    /// AF-640. The pool must FAIL rather than pin a thread for half a minute,
    /// and the bound must be the one we set rather than r2d2's default.
    ///
    /// EXHAUSTS THE POOL FOR REAL. Asserting the builder was called with a
    /// duration would pass on a value that never reaches the pool; this holds
    /// every connection and measures what a caller actually experiences.
    #[test]
    fn an_exhausted_read_pool_fails_fast_instead_of_pinning_a_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pool.db")).unwrap();
        let max = store.read_pool.max_size() as usize;
        assert!(max >= 1, "a pool with no connections cannot be exhausted");

        // Hold every connection, so the next acquire has nowhere to go.
        let held: Vec<_> = (0..max).map(|_| store.read().expect("initial fill")).collect();
        assert_eq!(store.read_pool.state().idle_connections, 0, "the pool must be empty");

        let t0 = std::time::Instant::now();
        let denied = store.read();
        let waited = t0.elapsed();

        assert!(denied.is_err(), "an exhausted pool must refuse, not hand out a 29th connection");
        // THE POINT: it fails in ~5s, not r2d2's default 30s. The upper bound is
        // what this card is about; the lower bound catches a timeout set so
        // small that ordinary contention would start failing.
        assert!(
            waited < std::time::Duration::from_secs(12),
            "waited {waited:?}: that is r2d2's 30s default, not our timeout, and every \
             one of those seconds pins a tokio worker"
        );
        assert!(
            waited >= std::time::Duration::from_secs(2),
            "waited only {waited:?}: the timeout is so short that normal contention \
             would 500 rather than queue"
        );
        drop(held);

        // CONTROL: after releasing, a read must succeed again. Without this the
        // assertions above are satisfied by a pool that is simply broken.
        assert!(store.read().is_ok(), "the pool must recover once connections are returned");
    }
}
