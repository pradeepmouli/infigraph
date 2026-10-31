//! How many rows each keyless edge table holds, and how many distinct
//! (source, target) pairs those rows cover.
//!
//! A keyless table has no uniqueness, so a writer that appends a pair the
//! graph already holds leaves a second row. One row per pair is the invariant:
//! `doctor` checks it, and the sequence test checks it after every operation.
//! Both ask through this module, so the query text and its parsing live once.

use std::time::{Duration, Instant};

use anyhow::Result;

/// The keyless edge tables and the labels of their endpoints.
pub const KEYLESS_EDGE_TABLES: [(&str, &str, &str); 7] = [
    ("CALLS", "Symbol", "Symbol"),
    ("INHERITS", "Symbol", "Symbol"),
    ("TESTED_BY", "Symbol", "Symbol"),
    ("IMPORTS", "Module", "Module"),
    ("CONTAINS", "Module", "Symbol"),
    ("DEFINES", "File", "Symbol"),
    ("HAS_STATEMENT", "Symbol", "Statement"),
];

/// Time `doctor` may spend counting, across all tables. A whole census of a
/// 2M-row graph takes about a second, so this bounds a slow disk, not a
/// normal run. A table not reached inside it is reported as not checked.
pub const CENSUS_BUDGET: Duration = Duration::from_secs(30);

/// One table's rows and distinct pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCount {
    pub table: String,
    pub rows: u64,
    pub pairs: u64,
}

impl TableCount {
    /// Rows beyond one per pair: the duplicates a rebuild would remove.
    pub fn extra(&self) -> u64 {
        self.rows.saturating_sub(self.pairs)
    }

    /// One result row of [`table_count_query`]: table, rows, pairs.
    pub fn from_cells(cells: &[String]) -> Option<Self> {
        let [table, rows, pairs] = cells else {
            return None;
        };
        Some(Self {
            table: unquote(table).to_string(),
            rows: unquote(rows).parse().ok()?,
            pairs: unquote(pairs).parse().ok()?,
        })
    }

    /// One line of `infigraph query` output for [`union_query`]:
    /// `TABLE | rows | pairs`.
    pub fn from_line(line: &str) -> Option<Self> {
        let cells: Vec<String> = line.split('|').map(|c| c.trim().to_string()).collect();
        Self::from_cells(&cells)
    }
}

fn unquote(cell: &str) -> &str {
    cell.trim().trim_matches('"')
}

/// The query that counts one table. The only place its text is written.
pub fn table_count_query(table: &str, src: &str, dst: &str) -> String {
    format!(
        "MATCH (a:{src})-[r:{table}]->(b:{dst}) \
         RETURN '{table}' AS t, count(r) AS rows, count(DISTINCT a.id + '|' + b.id) AS pairs"
    )
}

/// The given tables as one query, `UNION ALL` of [`table_count_query`]s.
///
/// Fine for a small graph. On a 2M-row graph it fails with "buffer pool is
/// full": the read-only buffer pool is 256 MB, and all seven aggregates
/// together do not fit in it, where one table alone does. [`census`] counts
/// one table at a time for that reason.
pub fn union_query(tables: &[(&str, &str, &str)]) -> String {
    tables
        .iter()
        .map(|(table, src, dst)| table_count_query(table, src, dst))
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// What a census counted, and what the budget left out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// Tables counted, in [`KEYLESS_EDGE_TABLES`] order.
    pub counted: Vec<TableCount>,
    /// Tables not reached before the budget ran out, in table order.
    pub not_checked: Vec<&'static str>,
}

impl Census {
    /// Duplicate rows across the tables counted.
    pub fn extra(&self) -> u64 {
        self.counted.iter().map(TableCount::extra).sum()
    }
}

/// Counts each keyless table in turn through `count`, and stops starting new
/// tables once `budget` is spent. A table not started goes into `not_checked`,
/// so a partial census says which tables it did not look at.
pub fn census(
    budget: Duration,
    mut count: impl FnMut(&str, &str, &str) -> Result<TableCount>,
) -> Result<Census> {
    let started = Instant::now();
    let mut out = Census::default();
    for (table, src, dst) in KEYLESS_EDGE_TABLES {
        if started.elapsed() >= budget {
            out.not_checked.push(table);
            continue;
        }
        out.counted.push(count(table, src, dst)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_of(table: &str, rows: u64, pairs: u64) -> TableCount {
        TableCount {
            table: table.into(),
            rows,
            pairs,
        }
    }

    #[test]
    fn a_result_line_parses_to_its_table_rows_and_pairs() {
        assert_eq!(
            TableCount::from_line("INHERITS | 2028836 | 14599"),
            Some(count_of("INHERITS", 2028836, 14599))
        );
        assert_eq!(TableCount::from_line("no separators here"), None);
    }

    #[test]
    fn a_quoted_cell_from_the_query_parses_the_same() {
        let cells: Vec<String> = vec!["\"CALLS\"".into(), "5".into(), "4".into()];
        assert_eq!(
            TableCount::from_cells(&cells),
            Some(count_of("CALLS", 5, 4))
        );
    }

    #[test]
    fn the_union_is_the_per_table_queries_joined() {
        let union = union_query(&KEYLESS_EDGE_TABLES[..2]);
        assert_eq!(
            union,
            format!(
                "{} UNION ALL {}",
                table_count_query("CALLS", "Symbol", "Symbol"),
                table_count_query("INHERITS", "Symbol", "Symbol")
            )
        );
    }

    #[test]
    fn a_census_with_every_table_inside_the_budget_counts_them_all() {
        let c = census(Duration::from_secs(60), |t, _, _| Ok(count_of(t, 2, 2))).unwrap();
        assert_eq!(c.counted.len(), KEYLESS_EDGE_TABLES.len());
        assert!(c.not_checked.is_empty());
        assert_eq!(c.extra(), 0);
    }

    #[test]
    fn a_census_past_its_budget_says_which_tables_it_did_not_check() {
        let c = census(Duration::ZERO, |t, _, _| Ok(count_of(t, 1, 1))).unwrap();
        assert!(c.counted.is_empty());
        assert_eq!(
            c.not_checked,
            KEYLESS_EDGE_TABLES
                .iter()
                .map(|(t, _, _)| *t)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_extra_rows_are_the_duplicates_summed_over_the_tables() {
        let c = Census {
            counted: vec![count_of("CALLS", 5, 4), count_of("INHERITS", 3, 3)],
            not_checked: vec![],
        };
        assert_eq!(c.extra(), 1);
    }
}
