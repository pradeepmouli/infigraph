//! `DocAck`: entries reviewed against their code, in the local document store.
//!
//! One row per target (a symbol id): the commit its lines were last changed in
//! when the entry was reviewed, with that commit's time, so an entry is
//! current for code up to that time. The newest review wins; an older one is
//! ignored. Rows are keyed by target, not by document, so they survive the
//! document being re-indexed -- `DocIndex::reindex` carries them across the
//! wipe it makes, and only `clean-docs` or a corruption wipe loses them.

use anyhow::Result;
use infigraph_core::daemon_protocol::DocAck;
use infigraph_core::escape_str;

use crate::store::DocStore;

pub(crate) const DOC_ACK_DDL: &str = "CREATE NODE TABLE IF NOT EXISTS DocAck(
        target STRING,
        commit STRING,
        commit_time INT64,
        acked_at INT64,
        PRIMARY KEY(target)
    )";

impl DocStore {
    /// Records `acks` (an ack older than the row already held for its target
    /// is ignored) and forgets the acks of the targets in `drop`. Returns the
    /// rows written and the rows removed. The caller holds the exclusive docs
    /// lock.
    pub fn upsert_acks(&self, acks: &[DocAck], drop: &[String]) -> Result<(usize, usize)> {
        let _phase = infigraph_core::write_phase::enter(
            &"docs: acknowledge entries",
            (acks.len() + drop.len()) as u64,
        );
        let conn = self.connection()?;
        // Idempotent, so a store opened before this table existed gains it.
        conn.query(DOC_ACK_DDL)
            .map_err(|e| anyhow::anyhow!("create DocAck: {e}"))?;
        let held = crate::query::DocQuery::new(&conn).doc_acks(
            &acks
                .iter()
                .map(|a| a.target.clone())
                .chain(drop.iter().cloned())
                .collect::<Vec<_>>(),
        )?;
        let mut stored = 0;
        for ack in acks {
            if held
                .get(&ack.target)
                .is_some_and(|row| row.commit_time >= ack.commit_time)
            {
                continue;
            }
            conn.query(&format!(
                "MATCH (a:DocAck {{target: '{t}'}}) DELETE a",
                t = escape_str(&ack.target)
            ))
            .map_err(|e| anyhow::anyhow!("replace DocAck: {e}"))?;
            conn.query(&format!(
                "CREATE (:DocAck {{target: '{}', commit: '{}', commit_time: {}, acked_at: {}}})",
                escape_str(&ack.target),
                escape_str(&ack.commit),
                ack.commit_time,
                ack.acked_at
            ))
            .map_err(|e| anyhow::anyhow!("record DocAck: {e}"))?;
            stored += 1;
        }
        let mut dropped = 0;
        for target in drop {
            if held.contains_key(target) {
                conn.query(&format!(
                    "MATCH (a:DocAck {{target: '{}'}}) DELETE a",
                    escape_str(target)
                ))
                .map_err(|e| anyhow::anyhow!("drop DocAck: {e}"))?;
                dropped += 1;
            }
        }
        Ok((stored, dropped))
    }
}
