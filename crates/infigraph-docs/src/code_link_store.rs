//! The write side of `CodeLink`/`CodeLinkScan` in the local document store.
//!
//! A document's links are replaced as a unit: its rows are deleted, the new
//! ones copied in, and the `CodeLinkScan` row written LAST. A reader (and the
//! next index run) treats a document without a scan row at the current
//! version as not scanned, so a run that dies halfway leaves a document that
//! is scanned again, never one that looks complete and is not.

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use infigraph_core::escape_str;
use kuzu::Connection;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

use crate::code_links::{CodeLinkRow, CODE_LINKS_VERSION};
use crate::store::DocStore;

pub(crate) const CODE_LINK_DDL: [&str; 2] = [
    "CREATE NODE TABLE IF NOT EXISTS CodeLink(
        id STRING,
        doc_file STRING,
        kind STRING,
        target STRING,
        heading STRING,
        line_start INT64,
        line_end INT64,
        text STRING,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS CodeLinkScan(
        doc_file STRING,
        version INT64,
        PRIMARY KEY(doc_file)
    )",
];

/// Deletes every link row and scan row of `doc_ids`. Called by every path that
/// removes or replaces a document, so a document's links never outlive it.
pub(crate) fn delete_rows(conn: &Connection<'_>, doc_ids: &[&str]) -> Result<()> {
    if doc_ids.is_empty() {
        return Ok(());
    }
    let id_list = doc_ids
        .iter()
        .map(|id| format!("'{}'", escape_str(id)))
        .collect::<Vec<_>>()
        .join(", ");
    conn.query(&format!(
        "MATCH (l:CodeLink) WHERE l.doc_file IN [{id_list}] DELETE l"
    ))
    .map_err(|e| anyhow::anyhow!("delete code links: {e}"))?;
    conn.query(&format!(
        "MATCH (s:CodeLinkScan) WHERE s.doc_file IN [{id_list}] DELETE s"
    ))
    .map_err(|e| anyhow::anyhow!("delete code link scans: {e}"))?;
    Ok(())
}

fn copy_rows(conn: &Connection<'_>, rows: &[CodeLinkRow]) -> Result<()> {
    let col = |f: fn(&CodeLinkRow) -> &str| -> Arc<StringArray> {
        Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>()))
    };
    let num = |f: fn(&CodeLinkRow) -> usize| -> Arc<Int64Array> {
        Arc::new(Int64Array::from(
            rows.iter().map(|r| f(r) as i64).collect::<Vec<_>>(),
        ))
    };
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("doc_file", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("heading", DataType::Utf8, false),
        Field::new("line_start", DataType::Int64, false),
        Field::new("line_end", DataType::Int64, false),
        Field::new("text", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            col(|r| r.id.as_str()),
            col(|r| r.doc_file.as_str()),
            Arc::new(StringArray::from(kinds)),
            col(|r| r.target.as_str()),
            col(|r| r.heading.as_str()),
            num(|r| r.line_start),
            num(|r| r.line_end),
            col(|r| r.text.as_str()),
        ],
    )?;
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let path = tmp.path().join("code_links.parquet");
    let file = std::fs::File::create(&path)?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(WriterProperties::builder().build()))?;
    writer.write(&batch)?;
    writer.close()?;
    conn.query(&format!(
        "COPY CodeLink FROM '{}'",
        path.to_string_lossy().replace('\\', "/")
    ))
    .map_err(|e| anyhow::anyhow!("COPY CodeLink: {e}"))?;
    Ok(())
}

impl DocStore {
    /// Replaces `doc_file`'s links with `rows` and records that it was scanned
    /// under [`CODE_LINKS_VERSION`]. The caller holds the exclusive docs lock.
    pub fn replace_code_links(&self, doc_file: &str, rows: &[CodeLinkRow]) -> Result<()> {
        let _phase =
            infigraph_core::write_phase::enter(&"docs: replace code links", rows.len() as u64);
        let conn = self.connection()?;
        delete_rows(&conn, &[doc_file])?;
        if !rows.is_empty() {
            copy_rows(&conn, rows)?;
        }
        conn.query(&format!(
            "CREATE (:CodeLinkScan {{doc_file: '{}', version: {}}})",
            escape_str(doc_file),
            CODE_LINKS_VERSION
        ))
        .map_err(|e| anyhow::anyhow!("record code link scan: {e}"))?;
        Ok(())
    }
}
