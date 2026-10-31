//! Keyless relationship tables (CALLS, INHERITS, ...) hold no key, so a writer
//! that appends an edge the graph already has makes a second copy, and the
//! copies accumulate. This drives a seeded sequence of edits, indexes, SCIP
//! enrichment and full rebuilds through the daemon-routed path, and after
//! every step checks that each table holds one row per distinct pair.
//!
//! The seed is in every failure message. `INFIGRAPH_SEQUENCE_SEED` picks
//! another; `INFIGRAPH_SEQUENCE_STEPS` the length.

mod support;

use std::path::Path;
use std::process::Output;

/// Keyless edge tables: (table, source label, target label).
const TABLES: &[(&str, &str, &str)] = &[
    ("CALLS", "Symbol", "Symbol"),
    ("INHERITS", "Symbol", "Symbol"),
    ("TESTED_BY", "Symbol", "Symbol"),
    ("IMPORTS", "Module", "Module"),
    ("CONTAINS", "Module", "Symbol"),
    ("DEFINES", "File", "Symbol"),
    ("HAS_STATEMENT", "Symbol", "Statement"),
];

const BASE: &str = "export interface Base1 { a: number }\n\
export interface Base2 { b: number }\n\
export function helper(x: number): number { return x + 1 }\n";
const CHILD: &str = "import { Base1, Base2, helper } from \"./base\";\n\
export interface Child extends Base1, Base2 { c: number }\n\
export function run(): number { return helper(2) }\n";
const MID: &str = "import { Base1 } from \"./base\";\n\
export interface Mid extends Base1 { m: number }\n";
const OTHER: &str = "import { helper } from \"./base\";\n\
export function other(): number { return helper(3) }\n";

#[derive(Clone, Copy, Debug)]
enum Op {
    Index,
    EditBase,
    EditChild,
    EditMid,
    EditOther,
    TouchBoth,
    Enrich,
    IndexFull,
}

const OPS: &[Op] = &[
    Op::Index,
    Op::EditBase,
    Op::EditChild,
    Op::EditMid,
    Op::EditOther,
    Op::TouchBoth,
    Op::Enrich,
    Op::IndexFull,
];

/// xorshift64: a seed fully determines the sequence, with no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next() % items.len() as u64) as usize]
    }
}

/// Stops the daemon the sequence started, on every exit path.
struct StopDaemon<'a>(&'a Path, &'a Path);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = routed(self.0, self.1, &["daemon-stop", "--wait"]);
    }
}

fn routed(root: &Path, home: &Path, args: &[&str]) -> Output {
    support::run(root, home, infigraph_core::DAEMON_BACKEND, args)
}

/// Rows and distinct (source, target) pairs of every table, through one daemon
/// query: one CLI spawn per check, not one per table.
fn counts(root: &Path, home: &Path) -> Vec<(String, i64, i64)> {
    let cypher = TABLES
        .iter()
        .map(|(table, src, dst)| {
            format!(
                "MATCH (a:{src})-[r:{table}]->(b:{dst}) RETURN '{table}' AS t, count(r) AS rows, count(DISTINCT a.id + '|' + b.id) AS pairs"
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let out = routed(root, home, &["query", &cypher]);
    support::assert_ok(&out, "count keyless tables");
    support::stdout(&out)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('|').map(str::trim);
            let table = parts.next()?;
            let rows = parts.next()?.parse().ok()?;
            let pairs = parts.next()?.parse().ok()?;
            Some((table.to_string(), rows, pairs))
        })
        .collect()
}

fn write(root: &Path, name: &str, text: &str) {
    std::fs::write(root.join(name), text).unwrap();
}

/// Appends a line that changes the file's content, so the next index sees it.
fn touch(root: &Path, name: &str, step: usize) {
    let path = root.join(name);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("// edit {step}\n"));
    std::fs::write(path, text).unwrap();
}

fn enrichment_available() -> bool {
    ["scip-typescript"].iter().all(|tool| {
        std::process::Command::new("which")
            .arg(tool)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn apply(op: Op, root: &Path, home: &Path, step: usize) -> String {
    match op {
        Op::Index => {}
        Op::EditBase => {
            touch(root, "base.ts", step);
        }
        Op::EditChild => {
            touch(root, "child.ts", step);
        }
        Op::EditMid => {
            touch(root, "mid.ts", step);
        }
        Op::EditOther => {
            touch(root, "other.ts", step);
        }
        Op::TouchBoth => {
            touch(root, "base.ts", step);
            touch(root, "child.ts", step);
        }
        Op::Enrich | Op::IndexFull => {}
    }
    match op {
        Op::IndexFull => {
            let out = routed(root, home, &["index", "--full"]);
            support::assert_ok(&out, "index --full");
        }
        Op::Enrich => {
            // An indexer is needed for enrichment to change anything; without one
            // the step is a plain index, so the sequence is the same either way.
            if enrichment_available() {
                let out = support::infigraph()
                    .args(["scip-enrich"])
                    .current_dir(root)
                    .env("HOME", home)
                    .env(infigraph_core::BACKEND_ENV, infigraph_core::DAEMON_BACKEND)
                    .env("INFIGRAPH_NO_WATCH", "1")
                    .env_remove("INFIGRAPH_WATCH_DAEMON")
                    .env_remove(infigraph_core::scip_switch::enabled_env_name())
                    .output()
                    .unwrap();
                support::assert_ok(&out, "scip-enrich");
            }
            let out = routed(root, home, &["index"]);
            support::assert_ok(&out, "index after enrich");
        }
        _ => {
            let out = routed(root, home, &["index"]);
            support::assert_ok(&out, "index");
        }
    }
    format!("{op:?}")
}

/// The default run: short enough for every `cargo test --all`.
#[test]
fn keyless_tables_hold_one_row_per_pair_through_a_seeded_sequence() {
    run_sequence(8);
}

/// The long run, 24 steps (about 150s). `cargo test -- --ignored` runs it.
#[test]
#[ignore = "24 steps, about 150s"]
fn keyless_tables_hold_one_row_per_pair_over_a_long_sequence() {
    run_sequence(24);
}

fn run_sequence(default_steps: usize) {
    let seed: u64 = std::env::var("INFIGRAPH_SEQUENCE_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7);
    let steps: usize = std::env::var("INFIGRAPH_SEQUENCE_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default_steps);

    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(root.path(), "base.ts", BASE);
    write(root.path(), "child.ts", CHILD);
    write(root.path(), "mid.ts", MID);
    write(root.path(), "other.ts", OTHER);
    let _stop = StopDaemon(root.path(), home.path());

    let mut rng = Rng(seed.max(1));
    let mut history = Vec::new();

    let first = routed(root.path(), home.path(), &["index"]);
    support::assert_ok(&first, "first index");
    history.push("Index".to_string());
    check(root.path(), home.path(), seed, &history);

    for step in 1..=steps {
        let op = rng.pick(OPS);
        let name = apply(op, root.path(), home.path(), step);
        history.push(name);
        check(root.path(), home.path(), seed, &history);
    }
}

/// Every table: rows == distinct pairs. The failure names the seed, the
/// operations so far, and the table with its counts.
fn check(root: &Path, home: &Path, seed: u64, history: &[String]) {
    let counted = counts(root, home);
    for (table, _, _) in TABLES {
        // An empty match yields no row from this engine, so a missing table is 0 rows.
        let (rows, distinct) = counted
            .iter()
            .find(|(name, _, _)| name == table)
            .map_or((0, 0), |(_, rows, pairs)| (*rows, *pairs));
        assert_eq!(
            rows,
            distinct,
            "seed {seed}, after {} step(s) {:?}: {table} has {rows} rows for {distinct} distinct pairs",
            history.len() - 1,
            history,
        );
    }
}
