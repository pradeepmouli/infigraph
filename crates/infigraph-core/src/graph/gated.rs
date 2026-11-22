//! The only connection type a [`GraphStore`] hands out. Every statement goes
//! through the store's [`FoldGate`](super::gate::FoldGate), so no lbug checkpoint
//! runs while another statement executes on that `Database`.
//!
//! The invariant lives here, once: no call site takes the gate itself, and the
//! raw `kuzu::Connection` is reachable only from `graph::` (the checkpoint, the
//! schema read at open). `tests/gate_guard.rs` fails on a raw `Connection::new`
//! on the code graph elsewhere.
//!
//! * a statement holds the gate **shared** (`query`, `prepare`). There is no
//!   `execute`: a prepared `COPY FROM` run that way would force its checkpoint
//!   outside the wrapper, and no production caller prepares anything but a read
//!   (`ensure_read_only`, then `query`);
//! * `CHECKPOINT` holds it **exclusive**;
//! * a `COPY ... FROM` forces a checkpoint at its commit, so outside a
//!   transaction it becomes `BEGIN`, the `COPY` (shared), then `COMMIT`
//!   (exclusive), with `ROLLBACK` on error. Inside a transaction it is
//!   refused: lbug aborts a manual transaction on any execution error, a
//!   second `BEGIN` is such an error, so the wrapper cannot ask lbug whether
//!   a transaction is open and never guesses about the caller's. Nothing in
//!   production does it (the bulk path is delete-transaction, autocommit
//!   `COPY`s, restore-transaction);
//! * `COPY (...) TO` reads: the database says so (`is_read_only`), not the text.

use super::store::GraphStore;
use kuzu::{Connection, Error, PreparedStatement, QueryResult};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct GatedConnection<'a> {
    conn: Connection<'a>,
    store: &'a GraphStore,
    /// Whether a `BEGIN` on this connection has not been answered by a
    /// `COMMIT`/`ROLLBACK`. An error inside the transaction leaves it set: lbug
    /// aborts on an execution error but not on every prepare error, so only the
    /// caller's `ROLLBACK` can clear it. A stale `true` only skips a fold.
    in_tx: AtomicBool,
}

#[derive(PartialEq)]
enum Kind {
    Begin,
    Commit,
    Rollback,
    Checkpoint,
    Copy,
    Other,
}

fn kind_of(sql: &str) -> Kind {
    let word: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    match word.as_str() {
        "BEGIN" => Kind::Begin,
        "COMMIT" => Kind::Commit,
        "ROLLBACK" => Kind::Rollback,
        "CHECKPOINT" => Kind::Checkpoint,
        "COPY" => Kind::Copy,
        _ => Kind::Other,
    }
}

impl<'a> GatedConnection<'a> {
    pub(super) fn new(store: &'a GraphStore, conn: Connection<'a>) -> Self {
        Self {
            conn,
            store,
            in_tx: AtomicBool::new(false),
        }
    }

    /// The raw connection, for the checkpoint itself and the open path. Not
    /// `pub`: a statement on it skips the gate.
    pub(super) fn raw(&self) -> &Connection<'a> {
        &self.conn
    }

    fn set_tx(&self, open: bool) {
        if self.in_tx.swap(open, Ordering::SeqCst) != open {
            self.store.note_transaction(open);
        }
    }

    pub fn query(&self, sql: &str) -> Result<QueryResult<'a>, Error> {
        match kind_of(sql) {
            Kind::Begin => {
                let _s = self.store.gate().shared();
                let r = self.conn.query(sql);
                if r.is_ok() {
                    self.set_tx(true);
                }
                r
            }
            Kind::Commit | Kind::Rollback => {
                let _s = self.store.gate().shared();
                let r = self.conn.query(sql);
                // Of any outcome: lbug has no transaction left after either.
                self.set_tx(false);
                r
            }
            Kind::Checkpoint => {
                let _x = self.store.gate().exclusive();
                self.conn.query(sql)
            }
            Kind::Copy => match self.copy_is_a_write(sql) {
                true => self.copy_from(sql),
                false => self.run(sql),
            },
            Kind::Other => self.run(sql),
        }
    }

    /// One autocommit (or in-transaction) statement under the shared gate.
    fn run(&self, sql: &str) -> Result<QueryResult<'a>, Error> {
        let r = {
            let _s = self.store.gate().shared();
            self.conn.query(sql)
        };
        if r.is_ok() && !self.in_tx.load(Ordering::SeqCst) {
            self.store.fold_if_mid_write_wal_is_large();
        }
        r
    }

    /// Whether `sql` (a `COPY ...`) writes: the database's own answer. A statement
    /// that does not prepare is run as written, so its error is the one lbug gives.
    fn copy_is_a_write(&self, sql: &str) -> bool {
        let _s = self.store.gate().shared();
        self.conn
            .prepare(sql)
            .map(|p| !p.is_read_only())
            .unwrap_or(false)
    }

    fn copy_from(&self, sql: &str) -> Result<QueryResult<'a>, Error> {
        if self.in_tx.load(Ordering::SeqCst) {
            return Err(Error::FailedQuery(
                "COPY FROM inside a transaction is refused: its commit forces a checkpoint that \
                 must run under the graph gate, and a second BEGIN would abort the open transaction"
                    .to_string(),
            ));
        }
        // The one BEGIN path: the open-transaction count moves while the shared
        // gate is still held, so a fold that takes the gate exclusive reads a count
        // that already includes this transaction (a count raised after the guard was
        // dropped left a window for a fold to wait on this BEGIN's write transaction
        // while the COPY waited on the gate).
        self.query("BEGIN TRANSACTION")?;
        let copied = {
            let _s = self.store.gate().shared();
            self.conn.query(sql)
        };
        match copied {
            Ok(result) => {
                let committed = {
                    let _wait = crate::phase_clock::SlowOp::start(|| {
                        "graph gate: waiting for running statements before a COPY commit"
                            .to_string()
                    });
                    let _x = self.store.gate().exclusive();
                    drop(_wait);
                    let _phase = crate::write_phase::enter(&"COPY FROM: COMMIT", 0);
                    self.conn.query("COMMIT")
                };
                self.set_tx(false);
                committed.map(|_| result)
            }
            Err(e) => {
                let _s = self.store.gate().shared();
                // lbug already aborted the transaction on the error, so this
                // usually answers "No active transaction": expected.
                let _ = self.conn.query("ROLLBACK");
                self.set_tx(false);
                Err(e)
            }
        }
    }

    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement, Error> {
        // Binding and planning read the catalog and statistics.
        let _s = self.store.gate().shared();
        self.conn.prepare(sql)
    }
}

impl Drop for GatedConnection<'_> {
    fn drop(&mut self) {
        // lbug rolls a connection's open transaction back when it is dropped.
        self.set_tx(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::store::GraphStore;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join(crate::graph::names::STEM)).unwrap();
        let conn = store.connection().unwrap();
        for id in ["a", "b", "c"] {
            conn.query(&format!(
                "CREATE (:Symbol {{id: '{id}', name: '{id}', kind: 'Function', file: 'f.rs'}})"
            ))
            .unwrap();
        }
        drop(conn);
        (dir, store)
    }

    fn csv(dir: &tempfile::TempDir, name: &str, rows: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, rows).unwrap();
        path.to_string_lossy().replace('\\', "/")
    }

    fn calls(conn: &GatedConnection<'_>) -> i64 {
        let mut r = conn
            .query("MATCH ()-[r:CALLS]->() RETURN count(r)")
            .unwrap();
        r.next().unwrap()[0].to_string().parse().unwrap()
    }

    fn until(cond: impl Fn() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "condition never held"
            );
            std::thread::yield_now();
        }
    }

    /// A statement already running holds off the COPY's commit (the fold lbug forces
    /// there must not overlap it); the COPY completes once the statement is done.
    #[test]
    fn a_read_in_flight_holds_off_a_copy_commit() {
        let (dir, store) = store();
        let file = csv(&dir, "e.csv", "a,b\nb,c\n");
        let in_flight = store.gate().shared(); // a read mid-statement
        let finished = AtomicBool::new(false);
        std::thread::scope(|s| {
            let copy = s.spawn(|| {
                let conn = store.connection().unwrap();
                conn.query(&format!("COPY CALLS FROM '{file}'")).unwrap();
                finished.store(true, SeqCst);
            });
            // The COPY has run and is now waiting to commit.
            until(|| store.gate().waiting_exclusive() == 1);
            assert!(
                !finished.load(SeqCst),
                "the COPY committed beside a running statement"
            );
            drop(in_flight);
            copy.join().unwrap();
        });
        assert_eq!(calls(&store.connection().unwrap()), 2);
        assert_eq!(store.open_transactions(), 0);
    }

    /// Every kind of statement waits while a checkpoint holds the gate, and runs when it lets go.
    #[test]
    fn every_statement_kind_waits_for_a_running_checkpoint() {
        let (_dir, store) = store();
        let conn = store.connection().unwrap();
        let ran = std::sync::atomic::AtomicUsize::new(0);
        let fold = store.gate().exclusive();
        std::thread::scope(|s| {
            s.spawn(|| {
                conn.query("RETURN 1").unwrap();
                ran.fetch_add(1, SeqCst);
            });
            s.spawn(|| {
                conn.prepare("RETURN 2").unwrap();
                ran.fetch_add(1, SeqCst);
            });
            until(|| store.gate().blocked_shared() == 2);
            assert_eq!(ran.load(SeqCst), 0, "a statement ran beside the checkpoint");
            drop(fold);
        });
        assert_eq!(ran.load(SeqCst), 2);
    }

    /// While a COPY commit waits, a new statement waits behind it (no starvation of
    /// the commit by a stream of reads), and runs once the commit is done.
    #[test]
    fn a_pending_copy_commit_holds_off_new_reads() {
        let (dir, store) = store();
        let file = csv(&dir, "e.csv", "a,b\n");
        let in_flight = store.gate().shared();
        let read_ran = AtomicBool::new(false);
        std::thread::scope(|s| {
            let copy = s.spawn(|| {
                store
                    .connection()
                    .unwrap()
                    .query(&format!("COPY CALLS FROM '{file}'"))
                    .unwrap();
            });
            until(|| store.gate().waiting_exclusive() == 1);
            let reader = s.spawn(|| {
                store.connection().unwrap().query("RETURN 1").unwrap();
                read_ran.store(true, SeqCst);
            });
            until(|| store.gate().blocked_shared() == 1);
            assert!(!read_ran.load(SeqCst), "a read overtook the waiting commit");
            drop(in_flight);
            copy.join().unwrap();
            reader.join().unwrap();
        });
        assert!(read_ran.load(SeqCst));
    }

    /// An optional fold does not wait for ever: it answers Busy within its bound,
    /// and leaves nothing that keeps statements out.
    #[test]
    fn an_optional_fold_returns_busy_within_its_bound() {
        let (dir, store) = store();
        let db = dir.path().join(crate::graph::names::STEM);
        let in_flight = store.gate().shared();
        let started = Instant::now();
        let r = store.checkpoint_now_waiting(&db, Duration::from_millis(100));
        assert!(
            matches!(r, Err(crate::graph::store::FoldError::Busy(_))),
            "{r:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(store.gate().waiting_exclusive(), 0);
        drop(in_flight);
        assert!(store
            .checkpoint_now_waiting(&db, Duration::from_secs(5))
            .is_ok());
    }

    /// The exclusive gate is never held while lbug waits for a write transaction: a
    /// transaction open on ANY connection makes the fold stand down at once, before it
    /// takes the gate, so the writer's next statement is never blocked behind a fold
    /// that is waiting for that writer.
    #[test]
    fn a_fold_stands_down_while_a_transaction_is_open_on_any_connection() {
        let (dir, store) = store();
        let db = dir.path().join(crate::graph::names::STEM);
        let writer = store.connection().unwrap();
        writer.query("BEGIN TRANSACTION").unwrap();
        assert_eq!(store.open_transactions(), 1);
        let started = Instant::now();
        let r = store.checkpoint_now_waiting(&db, Duration::from_secs(30));
        assert!(
            matches!(r, Err(crate::graph::store::FoldError::Busy(_))),
            "{r:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the fold waited ({:?}) instead of standing down",
            started.elapsed()
        );
        // and it took no gate: the writer carries on.
        assert!(store.gate().try_shared().is_some());
        writer
            .query("CREATE (:Symbol {id: 'd', name: 'd', kind: 'Function', file: 'f.rs'})")
            .unwrap();
        writer.query("COMMIT").unwrap();
        assert_eq!(store.open_transactions(), 0);
        assert!(store
            .checkpoint_now_waiting(&db, Duration::from_secs(5))
            .is_ok());
    }

    /// A connection dropped inside a transaction (lbug rolls it back) leaves no
    /// phantom open transaction behind.
    #[test]
    fn a_connection_dropped_in_a_transaction_closes_it_in_the_count() {
        let (_dir, store) = store();
        {
            let c = store.connection().unwrap();
            c.query("BEGIN TRANSACTION").unwrap();
            assert_eq!(store.open_transactions(), 1);
        }
        assert_eq!(store.open_transactions(), 0);
    }

    /// COPY FROM inside a transaction is refused, and the refusal leaves the
    /// transaction alone: a second BEGIN would have aborted it.
    #[test]
    fn a_copy_inside_a_transaction_is_refused_and_the_transaction_survives() {
        let (dir, store) = store();
        let file = csv(&dir, "e.csv", "a,b\n");
        let conn = store.connection().unwrap();
        conn.query("BEGIN TRANSACTION").unwrap();
        conn.query("CREATE (:Symbol {id: 'd', name: 'd', kind: 'Function', file: 'f.rs'})")
            .unwrap();
        let err = conn
            .query(&format!("COPY CALLS FROM '{file}'"))
            .map(|_| ())
            .expect_err("refused");
        assert!(err.to_string().contains("refused"), "{err}");
        conn.query("COMMIT").unwrap();
        let mut r = conn
            .query("MATCH (s:Symbol {id: 'd'}) RETURN count(s)")
            .unwrap();
        assert_eq!(
            r.next().unwrap()[0].to_string(),
            "1",
            "the caller's work was lost"
        );
        assert_eq!(calls(&conn), 0, "a refused COPY wrote");
    }

    /// A COPY that fails on a bad record reports lbug's own error, leaves the
    /// connection usable and outside any transaction, holds no gate, and a retry on a
    /// fresh connection (what `copy_edges_with_bad_record_retry` does) succeeds.
    #[test]
    fn a_bad_copy_rolls_back_and_the_connection_and_retry_work() {
        let (dir, store) = store();
        let bad = csv(&dir, "bad.csv", "a,b\nb,__nope__\n");
        let good = csv(&dir, "good.csv", "a,b\nb,c\n");
        let conn = store.connection().unwrap();
        let err = conn
            .query(&format!("COPY CALLS FROM '{bad}'"))
            .map(|_| ())
            .expect_err("a bad record fails");
        // The text the retry loop matches on (`MISSING_PK_MARKER`) is lbug's, untouched.
        assert!(
            err.to_string().contains("Unable to find primary key value"),
            "{err}"
        );
        assert_eq!(store.open_transactions(), 0);
        assert!(
            store.gate().try_exclusive_for(Duration::ZERO).is_some(),
            "the gate leaked"
        );
        assert_eq!(calls(&conn), 0, "a failed COPY left edges behind");
        // the same connection is usable, and a COPY on it still works
        conn.query(&format!("COPY CALLS FROM '{good}'")).unwrap();
        assert_eq!(calls(&conn), 2);
    }

    /// `COPY (...) TO` reads: it must not be wrapped in a write transaction or wait
    /// for the exclusive gate. lbug says so (`is_read_only`), not the text.
    #[test]
    fn copy_to_is_a_read_and_takes_no_exclusive_gate() {
        let (dir, store) = store();
        let out = dir
            .path()
            .join("o.csv")
            .to_string_lossy()
            .replace('\\', "/");
        let _in_flight = store.gate().shared();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let conn = store.connection().unwrap();
                let r = conn.query(&format!("COPY (MATCH (s:Symbol) RETURN s.id) TO '{out}'"));
                tx.send((r.is_ok(), store.open_transactions())).unwrap();
            });
            // A wrapped COPY would sit waiting for the exclusive gate behind the
            // statement held above; a read finishes at once.
            let (ok, txs) = rx
                .recv_timeout(Duration::from_secs(20))
                .expect("COPY TO waited for the exclusive gate");
            assert!(ok);
            assert_eq!(txs, 0, "COPY TO was wrapped in a transaction");
        });
    }

    /// A long write that holds the write lock across many autocommit statements folds
    /// the WAL itself once it is large, since lbug's own threshold is out of reach.
    #[test]
    fn a_long_write_under_the_write_lock_folds_a_large_wal_mid_write() {
        let (dir, store) = store();
        let db = dir.path().join(crate::graph::names::STEM);
        store.set_mid_write_fold_bytes_for_test(64 * 1024);
        let _lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        let mut folded = false;
        let mut peak = 0;
        for i in 0..4000 {
            conn.query(&format!(
                "CREATE (:Symbol {{id: 's{i}', name: 'n{i}', kind: 'Function', file: 'f.rs'}})"
            ))
            .unwrap();
            let wal = crate::graph::store_util::graph_wal_bytes(&db);
            peak = peak.max(wal);
            if wal < peak / 2 {
                folded = true;
                break;
            }
        }
        assert!(
            folded,
            "the WAL grew to {peak} bytes with no mid-write fold"
        );
        assert!(peak < 1024 * 1024, "the fold came too late: {peak}");
    }

    /// The same statements fold nothing from a thread that does not hold the write
    /// lock, with a transaction open, or with the write lock moved to another thread:
    /// each could be waiting on, or be, the transaction the fold would wait for.
    #[test]
    fn a_mid_write_fold_needs_the_lock_holders_thread_and_no_open_transaction() {
        let (dir, store) = store();
        let db = dir.path().join(crate::graph::names::STEM);
        store.set_mid_write_fold_bytes_for_test(64 * 1024);
        let grow = |conn: &GatedConnection<'_>, n: usize, tag: &str| {
            for i in 0..n {
                conn.query(&format!(
                    "CREATE (:Symbol {{id: '{tag}{i}', name: 'n', kind: 'Function', file: 'f.rs'}})"
                ))
                .unwrap();
            }
        };
        let wal = || crate::graph::store_util::graph_wal_bytes(&db);

        // no write lock held at all
        let conn = store.connection().unwrap();
        grow(&conn, 600, "free");
        assert!(
            wal() > 64 * 1024,
            "the test never made the WAL large: {}",
            wal()
        );

        // lock held on this thread, but a manual transaction is open
        let lock = store.write_lock().unwrap(); // folds the 1 MiB-class WAL first if large
        let before = wal();
        conn.query("BEGIN TRANSACTION").unwrap();
        grow(&conn, 10, "tx");
        assert!(wal() >= before, "a fold ran inside a transaction");
        conn.query("COMMIT").unwrap();

        // the lock moved to another thread: that thread is not the one that took it,
        // so its statements are not the owner's and fold nothing
        let size = wal();
        std::thread::scope(|s| {
            let moved = lock;
            let (store, grow) = (&store, &grow);
            s.spawn(move || {
                let _held = moved;
                grow(&store.connection().unwrap(), 400, "other");
            })
            .join()
            .unwrap();
        });
        assert!(wal() >= size, "a thread that did not take the lock folded");
    }

    /// A writer waiting for the write lock holds no gate: the fold that used to run
    /// before the lock is taken now runs after it, so a blocked writer cannot sit on
    /// the exclusive gate while the holder, with a transaction open, waits on a statement.
    #[test]
    fn a_writer_blocked_on_the_write_lock_holds_no_gate() {
        let (_dir, store) = store();
        let holder = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        conn.query("BEGIN TRANSACTION").unwrap();
        let entered = AtomicBool::new(false);
        let started = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                started.store(true, SeqCst);
                let _lock = store.write_lock().unwrap();
                entered.store(true, SeqCst);
            });
            // The waiter is stuck on the lock; the holder's transaction proceeds and
            // nothing exclusive is held or pending, however often it is looked at.
            until(|| started.load(SeqCst));
            for _ in 0..2000 {
                assert!(!entered.load(SeqCst));
                assert_eq!(store.gate().waiting_exclusive(), 0);
                std::thread::yield_now();
            }
            conn.query("CREATE (:Symbol {id: 'z', name: 'z', kind: 'Function', file: 'f.rs'})")
                .unwrap();
            conn.query("COMMIT").unwrap();
            drop(holder);
            waiter.join().unwrap();
        });
        assert!(entered.load(SeqCst));
    }

    /// A writer that takes the lock the instant the previous holder lets go must
    /// still find ITSELF recorded as the holder afterwards. Dropping the file lock
    /// before the mark let this writer mark itself in the gap and then have its mark
    /// wiped by the previous lock's drop, so it never folded mid-write. The handover
    /// cannot be paused from a test, so one thread hands the lock over thousands of
    /// times while another spins on `try_write_lock` and checks its mark after each
    /// win. Probabilistic: it cannot raise a false alarm, but a clean run is evidence,
    /// not proof; `WriteLock`'s explicit `Drop` is what makes the order independent of
    /// field order.
    #[test]
    fn a_new_write_lock_holder_is_never_unmarked_by_the_previous_one_dropping() {
        let (_dir, store) = store();
        let done = AtomicBool::new(false);
        let wins = std::sync::atomic::AtomicUsize::new(0);
        let wrong = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..4000 {
                    drop(
                        store
                            .write_lock_with_timeout(Duration::from_secs(60))
                            .unwrap(),
                    );
                }
                done.store(true, SeqCst);
            });
            s.spawn(|| {
                while !done.load(SeqCst) {
                    if let Some(lock) = store.try_write_lock().unwrap() {
                        wins.fetch_add(1, SeqCst);
                        // Give a previous holder's drop every chance to run late.
                        for _ in 0..50 {
                            std::thread::yield_now();
                        }
                        if !store.writer_thread_is_me() {
                            wrong.fetch_add(1, SeqCst);
                        }
                        drop(lock);
                    }
                }
            });
        });
        assert!(wins.load(SeqCst) > 0, "the spinner never won the lock");
        assert_eq!(
            wrong.load(SeqCst),
            0,
            "a writer held the lock without being marked"
        );
    }

    /// A mid-write fold that is turned away by a running read is not retried by the
    /// next statement: each retry would wait for the gate and the turnstile would hold
    /// new reads back for all of it.
    #[test]
    fn a_refused_mid_write_fold_is_not_retried_by_the_next_statement() {
        let (_dir, store) = store();
        store.set_mid_write_fold_bytes_for_test(8 * 1024);
        let _lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        // Enough WAL, no hold yet: the statements below would fold at once, so first
        // pause the fold with a statement-in-flight stand-in that outlasts its wait.
        let in_flight = store.gate().shared();
        let t = Instant::now();
        for i in 0..200 {
            conn.query(&format!(
                "CREATE (:Symbol {{id: 'r{i}', name: 'n', kind: 'Function', file: 'f.rs'}})"
            ))
            .unwrap();
            if store.mid_fold_attempts() >= 1 {
                break;
            }
        }
        assert_eq!(
            store.mid_fold_attempts(),
            1,
            "the large WAL never triggered an attempt"
        );
        let first = t.elapsed();
        // two more statements while it is still being held: no second attempt
        for i in 0..2 {
            conn.query(&format!(
                "CREATE (:Symbol {{id: 'q{i}', name: 'n', kind: 'Function', file: 'f.rs'}})"
            ))
            .unwrap();
        }
        assert_eq!(
            store.mid_fold_attempts(),
            1,
            "a refused fold was retried at once"
        );
        assert!(
            first < Duration::from_secs(5),
            "the attempt waited as long as an optional fold ({first:?})"
        );
        drop(in_flight);
    }

    /// `COPY Folder` failing because a folder already exists is the normal
    /// incremental path (`upsert_folders_bulk_conn` falls back to UNWIND on the same
    /// connection): after the wrapper's rollback that connection is outside a
    /// transaction, and the rest of the fallback writes the right rows.
    #[test]
    fn a_folder_copy_that_fails_falls_back_to_unwind_on_the_same_connection() {
        let (_dir, store) = store();
        store
            .upsert_folders_bulk(&["src/a/x.rs", "src/b/y.rs"])
            .unwrap();
        // second pass: the folders exist, COPY Folder fails, the UNWIND path runs
        store
            .upsert_folders_bulk(&["src/a/x.rs", "src/b/y.rs", "src/c/z.rs"])
            .unwrap();
        assert_eq!(store.open_transactions(), 0);
        let conn = store.connection().unwrap();
        let mut folders = conn.query("MATCH (f:Folder) RETURN count(f)").unwrap();
        assert_eq!(
            folders.next().unwrap()[0].to_string(),
            "4",
            "src, src/a, src/b, src/c"
        );
        let mut edges = conn
            .query("MATCH (:Folder)-[r:CONTAINS_FOLDER]->(:Folder) RETURN count(r)")
            .unwrap();
        assert_eq!(edges.next().unwrap()[0].to_string(), "3");
    }

    /// The commit error of a COPY whose forced fold fails is lbug's, passed through
    /// untouched, so the callers that classify it by text still can.
    #[test]
    fn a_commit_error_reaches_the_caller_unchanged_for_classification() {
        let (_dir, store) = store();
        let conn = store.connection().unwrap();
        let err = conn
            .query("COMMIT")
            .map(|_| ())
            .expect_err("nothing to commit");
        assert_eq!(
            err.to_string(),
            "Query execution failed: No active transaction for COMMIT."
        );
        let stop = store.stop_if_committed_but_fold_failed(
            "Transaction committed successfully, but the post-commit checkpoint failed. \
             The committed data is durable and will be recovered on restart",
        );
        assert!(stop.is_some());
    }
}
