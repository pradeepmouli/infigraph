use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use kuzu::{Connection, Database, SystemConfig};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

use crate::chunk::Chunk;
use crate::extract::ExtractedDoc;

const CREATE_SCHEMA: &[&str] = &[
    "CREATE NODE TABLE IF NOT EXISTS Document(
        id STRING,
        title STRING,
        file STRING,
        format STRING,
        content_hash STRING,
        page_count INT64,
        chunk_count INT64,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS Chunk(
        id STRING,
        doc_file STRING,
        idx INT64,
        heading STRING,
        text STRING,
        start_offset INT64,
        end_offset INT64,
        page INT64,
        content_hash STRING,
        PRIMARY KEY(id)
    )",
    "CREATE REL TABLE IF NOT EXISTS HAS_CHUNK(FROM Document TO Chunk)",
    "CREATE NODE TABLE IF NOT EXISTS Source(
        id STRING,
        source_type STRING,
        base_url STRING,
        space_key STRING,
        last_synced STRING,
        PRIMARY KEY(id)
    )",
    "CREATE REL TABLE IF NOT EXISTS FROM_SOURCE(FROM Document TO Source)",
    "CREATE REL TABLE IF NOT EXISTS LINKS_TO(FROM Document TO Document, url STRING, link_type STRING)",
    "CREATE NODE TABLE IF NOT EXISTS Pipeline(
        id STRING,
        name STRING,
        doc_id STRING,
        source_systems STRING,
        dest_tables STRING,
        scheduler_type STRING,
        scheduler_config STRING,
        compliance STRING,
        github_repo STRING,
        daci STRING,
        idempotent STRING,
        business_logic_summary STRING,
        data_quality STRING,
        dependencies_upstream STRING,
        dependencies_downstream STRING,
        PRIMARY KEY(id)
    )",
    "CREATE REL TABLE IF NOT EXISTS DEFINED_IN(FROM Pipeline TO Document)",
    "CREATE REL TABLE IF NOT EXISTS DEPENDS_ON(FROM Pipeline TO Pipeline, dep_type STRING)",
    // PipelineCore (Phase A — plugin pipeline system)
    "CREATE NODE TABLE IF NOT EXISTS PipelineCore(id STRING, name STRING, doc_id STRING, plugin_id STRING, inputs STRING[], outputs STRING[], PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS PIPELINE_DEFINED_IN(FROM PipelineCore TO Document, ONE_MANY)",
    "CREATE REL TABLE IF NOT EXISTS PIPELINE_DEPENDS_ON(FROM PipelineCore TO PipelineCore, dep_type STRING, MANY_MANY)",
];

pub struct DocStore {
    db: Database,
}

impl DocStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open docs kuzu db: {e}"))?;
        let store = Self { db };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.connection()?;
        for ddl in CREATE_SCHEMA {
            conn.query(ddl)
                .map_err(|e| anyhow::anyhow!("schema DDL failed: {e}"))?;
        }
        Ok(())
    }

    pub fn connection(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| anyhow::anyhow!("failed to create connection: {e}"))
    }

    pub fn get_doc_hashes(&self) -> Result<HashMap<String, String>> {
        let conn = self.connection()?;
        let result = conn
            .query("MATCH (d:Document) RETURN d.file, d.content_hash")
            .map_err(|e| anyhow::anyhow!("query doc hashes: {e}"))?;
        let mut hashes = HashMap::new();
        for row in result {
            if row.len() >= 2 {
                hashes.insert(row[0].to_string(), row[1].to_string());
            }
        }
        Ok(hashes)
    }

    pub fn upsert_all_parquet(&self, docs: &[&ExtractedDoc], chunks: &[&Chunk]) -> Result<()> {
        let conn = self.connection()?;

        // Delete existing data for changed files
        let file_list: Vec<String> = docs
            .iter()
            .map(|d| format!("'{}'", escape_str(&d.file)))
            .collect();
        if !file_list.is_empty() {
            let files_in = file_list.join(", ");
            let _ = conn.query(&format!(
                "MATCH (c:Chunk) WHERE c.doc_file IN [{}] DETACH DELETE c",
                files_in
            ));
            let _ = conn.query(&format!(
                "MATCH (d:Document) WHERE d.file IN [{}] DETACH DELETE d",
                files_in
            ));
        }

        let tmp_dir = tempfile::tempdir().context("create temp dir")?;

        // Write Document parquet
        {
            let ids: Vec<&str> = docs.iter().map(|d| d.file.as_str()).collect();
            let titles: Vec<Option<&str>> = docs.iter().map(|d| d.title.as_deref()).collect();
            let files: Vec<&str> = docs.iter().map(|d| d.file.as_str()).collect();
            let formats: Vec<&str> = docs.iter().map(|d| d.format.as_str()).collect();
            let hashes: Vec<&str> = docs.iter().map(|d| d.content_hash.as_str()).collect();
            let page_counts: Vec<i64> = docs
                .iter()
                .map(|d| d.page_count.unwrap_or(0) as i64)
                .collect();
            let chunk_counts: Vec<i64> = docs
                .iter()
                .map(|d| chunks.iter().filter(|c| c.doc_file == d.file).count() as i64)
                .collect();

            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::Utf8, false),
                Field::new("title", DataType::Utf8, true),
                Field::new("file", DataType::Utf8, false),
                Field::new("format", DataType::Utf8, false),
                Field::new("content_hash", DataType::Utf8, false),
                Field::new("page_count", DataType::Int64, false),
                Field::new("chunk_count", DataType::Int64, false),
            ]));

            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(ids)),
                    Arc::new(StringArray::from(titles)),
                    Arc::new(StringArray::from(files)),
                    Arc::new(StringArray::from(formats)),
                    Arc::new(StringArray::from(hashes)),
                    Arc::new(Int64Array::from(page_counts)),
                    Arc::new(Int64Array::from(chunk_counts)),
                ],
            )?;

            let path = tmp_dir.path().join("documents.parquet");
            let file = std::fs::File::create(&path)?;
            let props = WriterProperties::builder().build();
            let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
            writer.write(&batch)?;
            writer.close()?;

            conn.query(&format!("COPY Document FROM '{}'", path.to_string_lossy()))
                .map_err(|e| anyhow::anyhow!("COPY Document: {e}"))?;
        }

        // Write Chunk parquet
        if !chunks.is_empty() {
            let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
            let doc_files: Vec<&str> = chunks.iter().map(|c| c.doc_file.as_str()).collect();
            let indices: Vec<i64> = chunks.iter().map(|c| c.index as i64).collect();
            let headings: Vec<Option<&str>> = chunks.iter().map(|c| c.heading.as_deref()).collect();
            let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
            let start_offsets: Vec<i64> = chunks.iter().map(|c| c.start_offset as i64).collect();
            let end_offsets: Vec<i64> = chunks.iter().map(|c| c.end_offset as i64).collect();
            let pages: Vec<i64> = chunks.iter().map(|c| c.page.unwrap_or(0) as i64).collect();
            let hashes: Vec<&str> = chunks.iter().map(|c| c.content_hash.as_str()).collect();

            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::Utf8, false),
                Field::new("doc_file", DataType::Utf8, false),
                Field::new("idx", DataType::Int64, false),
                Field::new("heading", DataType::Utf8, true),
                Field::new("text", DataType::Utf8, false),
                Field::new("start_offset", DataType::Int64, false),
                Field::new("end_offset", DataType::Int64, false),
                Field::new("page", DataType::Int64, false),
                Field::new("content_hash", DataType::Utf8, false),
            ]));

            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(ids)),
                    Arc::new(StringArray::from(doc_files)),
                    Arc::new(Int64Array::from(indices)),
                    Arc::new(StringArray::from(headings)),
                    Arc::new(StringArray::from(texts)),
                    Arc::new(Int64Array::from(start_offsets)),
                    Arc::new(Int64Array::from(end_offsets)),
                    Arc::new(Int64Array::from(pages)),
                    Arc::new(StringArray::from(hashes)),
                ],
            )?;

            let path = tmp_dir.path().join("chunks.parquet");
            let file = std::fs::File::create(&path)?;
            let props = WriterProperties::builder().build();
            let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
            writer.write(&batch)?;
            writer.close()?;

            conn.query(&format!("COPY Chunk FROM '{}'", path.to_string_lossy()))
                .map_err(|e| anyhow::anyhow!("COPY Chunk: {e}"))?;
        }

        // Write HAS_CHUNK edges
        if !chunks.is_empty() {
            let froms: Vec<&str> = chunks.iter().map(|c| c.doc_file.as_str()).collect();
            let tos: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();

            let schema = Arc::new(Schema::new(vec![
                Field::new("from", DataType::Utf8, false),
                Field::new("to", DataType::Utf8, false),
            ]));

            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(froms)),
                    Arc::new(StringArray::from(tos)),
                ],
            )?;

            let path = tmp_dir.path().join("has_chunk.parquet");
            let file = std::fs::File::create(&path)?;
            let props = WriterProperties::builder().build();
            let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
            writer.write(&batch)?;
            writer.close()?;

            conn.query(&format!("COPY HAS_CHUNK FROM '{}'", path.to_string_lossy()))
                .map_err(|e| anyhow::anyhow!("COPY HAS_CHUNK: {e}"))?;
        }

        Ok(())
    }

    pub fn upsert_source(
        &self,
        id: &str,
        source_type: &str,
        base_url: &str,
        space_key: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        let _ = conn.query(&format!(
            "MATCH (s:Source) WHERE s.id = '{}' DETACH DELETE s",
            escape_str(id)
        ));
        conn.query(&format!(
            "CREATE (s:Source {{id: '{}', source_type: '{}', base_url: '{}', space_key: '{}', last_synced: '{}'}})",
            escape_str(id),
            escape_str(source_type),
            escape_str(base_url),
            escape_str(space_key),
            chrono::Utc::now().to_rfc3339(),
        ))
        .map_err(|e| anyhow::anyhow!("create Source: {e}"))?;
        Ok(())
    }

    pub fn link_doc_to_source(&self, doc_id: &str, source_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.query(&format!(
            "MATCH (d:Document), (s:Source) WHERE d.id = '{}' AND s.id = '{}' CREATE (d)-[:FROM_SOURCE]->(s)",
            escape_str(doc_id),
            escape_str(source_id),
        ))
        .map_err(|e| anyhow::anyhow!("link FROM_SOURCE: {e}"))?;
        Ok(())
    }

    pub fn get_docs_by_source(&self, source_id: &str) -> Result<Vec<String>> {
        let conn = self.connection()?;
        let result = conn
            .query(&format!(
                "MATCH (d:Document)-[:FROM_SOURCE]->(s:Source) WHERE s.id = '{}' RETURN d.id",
                escape_str(source_id)
            ))
            .map_err(|e| anyhow::anyhow!("query docs by source: {e}"))?;
        let mut ids = Vec::new();
        for row in result {
            if !row.is_empty() {
                ids.insert(ids.len(), row[0].to_string());
            }
        }
        Ok(ids)
    }

    pub fn delete_docs_by_ids(&self, doc_ids: &[&str]) -> Result<()> {
        if doc_ids.is_empty() {
            return Ok(());
        }
        let conn = self.connection()?;
        let id_list: String = doc_ids
            .iter()
            .map(|id| format!("'{}'", escape_str(id)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = conn.query(&format!(
            "MATCH (c:Chunk) WHERE c.doc_file IN [{}] DETACH DELETE c",
            id_list
        ));
        let _ = conn.query(&format!(
            "MATCH (d:Document) WHERE d.id IN [{}] DETACH DELETE d",
            id_list
        ));
        Ok(())
    }

    pub fn create_link(
        &self,
        from_doc_id: &str,
        to_doc_id: &str,
        url: &str,
        link_type: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        conn.query(&format!(
            "MATCH (a:Document), (b:Document) WHERE a.id = '{}' AND b.id = '{}' \
             CREATE (a)-[:LINKS_TO {{url: '{}', link_type: '{}'}}]->(b)",
            escape_str(from_doc_id),
            escape_str(to_doc_id),
            escape_str(url),
            escape_str(link_type),
        ))
        .map_err(|e| anyhow::anyhow!("create LINKS_TO: {e}"))?;
        Ok(())
    }

    pub fn delete_links_from(&self, doc_id: &str) -> Result<()> {
        let conn = self.connection()?;
        let _ = conn.query(&format!(
            "MATCH (a:Document)-[r:LINKS_TO]->() WHERE a.id = '{}' DELETE r",
            escape_str(doc_id),
        ));
        Ok(())
    }

    pub fn stats(&self) -> Result<DocStoreStats> {
        let conn = self.connection()?;
        let doc_count = count_query(&conn, "MATCH (d:Document) RETURN count(d)");
        let chunk_count = count_query(&conn, "MATCH (c:Chunk) RETURN count(c)");
        let pipeline_count = count_query(&conn, "MATCH (p:Pipeline) RETURN count(p)");
        Ok(DocStoreStats {
            document_count: doc_count,
            chunk_count,
            pipeline_count,
        })
    }

    pub fn upsert_pipeline(&self, pipeline: &PipelineRecord) -> Result<()> {
        let conn = self.connection()?;
        let _ = conn.query(&format!(
            "MATCH (p:Pipeline) WHERE p.id = '{}' DETACH DELETE p",
            escape_str(&pipeline.id)
        ));
        conn.query(&format!(
            "CREATE (p:Pipeline {{id: '{}', name: '{}', doc_id: '{}', source_systems: '{}', dest_tables: '{}', scheduler_type: '{}', scheduler_config: '{}', compliance: '{}', github_repo: '{}', daci: '{}', idempotent: '{}', business_logic_summary: '{}', data_quality: '{}', dependencies_upstream: '{}', dependencies_downstream: '{}'}})",
            escape_str(&pipeline.id),
            escape_str(&pipeline.name),
            escape_str(&pipeline.doc_id),
            escape_str(&pipeline.source_systems),
            escape_str(&pipeline.dest_tables),
            escape_str(&pipeline.scheduler_type),
            escape_str(&pipeline.scheduler_config),
            escape_str(&pipeline.compliance),
            escape_str(&pipeline.github_repo),
            escape_str(&pipeline.daci),
            escape_str(&pipeline.idempotent),
            escape_str(&pipeline.business_logic_summary),
            escape_str(&pipeline.data_quality),
            escape_str(&pipeline.dependencies_upstream),
            escape_str(&pipeline.dependencies_downstream),
        ))
        .map_err(|e| anyhow::anyhow!("create Pipeline: {e}"))?;
        Ok(())
    }

    pub fn link_pipeline_to_doc(&self, pipeline_id: &str, doc_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.query(&format!(
            "MATCH (p:Pipeline), (d:Document) WHERE p.id = '{}' AND d.id = '{}' CREATE (p)-[:DEFINED_IN]->(d)",
            escape_str(pipeline_id),
            escape_str(doc_id),
        ))
        .map_err(|e| anyhow::anyhow!("link DEFINED_IN: {e}"))?;
        Ok(())
    }

    pub fn create_depends_on(&self, from_id: &str, to_id: &str, dep_type: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.query(&format!(
            "MATCH (a:Pipeline), (b:Pipeline) WHERE a.id = '{}' AND b.id = '{}' CREATE (a)-[:DEPENDS_ON {{dep_type: '{}'}}]->(b)",
            escape_str(from_id),
            escape_str(to_id),
            escape_str(dep_type),
        ))
        .map_err(|e| anyhow::anyhow!("create DEPENDS_ON: {e}"))?;
        Ok(())
    }

    pub fn get_pipeline(&self, pipeline_id: &str) -> Result<Option<PipelineRecord>> {
        let conn = self.connection()?;
        let mut result = conn
            .query(&format!(
                "MATCH (p:Pipeline) WHERE p.id = '{}' RETURN p.id, p.name, p.doc_id, p.source_systems, p.dest_tables, p.scheduler_type, p.scheduler_config, p.compliance, p.github_repo, p.daci, p.idempotent, p.business_logic_summary, p.data_quality, p.dependencies_upstream, p.dependencies_downstream",
                escape_str(pipeline_id)
            ))
            .map_err(|e| anyhow::anyhow!("query pipeline: {e}"))?;
        if let Some(row) = result.next() {
            if row.len() >= 15 {
                return Ok(Some(PipelineRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    source_systems: row[3].to_string(),
                    dest_tables: row[4].to_string(),
                    scheduler_type: row[5].to_string(),
                    scheduler_config: row[6].to_string(),
                    compliance: row[7].to_string(),
                    github_repo: row[8].to_string(),
                    daci: row[9].to_string(),
                    idempotent: row[10].to_string(),
                    business_logic_summary: row[11].to_string(),
                    data_quality: row[12].to_string(),
                    dependencies_upstream: row[13].to_string(),
                    dependencies_downstream: row[14].to_string(),
                }));
            }
        }
        Ok(None)
    }

    pub fn get_all_pipelines(&self) -> Result<Vec<PipelineRecord>> {
        let conn = self.connection()?;
        let mut result = conn
            .query("MATCH (p:Pipeline) RETURN p.id, p.name, p.doc_id, p.source_systems, p.dest_tables, p.scheduler_type, p.scheduler_config, p.compliance, p.github_repo, p.daci, p.idempotent, p.business_logic_summary, p.data_quality, p.dependencies_upstream, p.dependencies_downstream")
            .map_err(|e| anyhow::anyhow!("query all pipelines: {e}"))?;
        let mut pipelines = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 15 {
                pipelines.push(PipelineRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    source_systems: row[3].to_string(),
                    dest_tables: row[4].to_string(),
                    scheduler_type: row[5].to_string(),
                    scheduler_config: row[6].to_string(),
                    compliance: row[7].to_string(),
                    github_repo: row[8].to_string(),
                    daci: row[9].to_string(),
                    idempotent: row[10].to_string(),
                    business_logic_summary: row[11].to_string(),
                    data_quality: row[12].to_string(),
                    dependencies_upstream: row[13].to_string(),
                    dependencies_downstream: row[14].to_string(),
                });
            }
        }
        Ok(pipelines)
    }

    pub fn link_pipeline_dependencies(&self) -> Result<usize> {
        let pipelines = self.get_all_pipelines()?;
        if pipelines.len() < 2 {
            return Ok(0);
        }

        let conn = self.connection()?;
        let _ = conn.query("MATCH ()-[r:DEPENDS_ON]->() DELETE r");

        let mut count = 0;
        for producer in &pipelines {
            let dest_tables: Vec<&str> = producer.dest_tables
                .split(',')
                .map(|t| t.trim())
                .filter(|t| !t.is_empty() && t.contains('.'))
                .collect();
            if dest_tables.is_empty() {
                continue;
            }

            for consumer in &pipelines {
                if consumer.id == producer.id {
                    continue;
                }
                let source_text = consumer.source_systems.to_lowercase();
                let upstream_text = consumer.dependencies_upstream.to_lowercase();

                for table in &dest_tables {
                    let table_lower = table.to_lowercase();
                    if source_text.contains(&table_lower) || upstream_text.contains(&table_lower) {
                        conn.query(&format!(
                            "MATCH (a:Pipeline), (b:Pipeline) WHERE a.id = '{}' AND b.id = '{}' CREATE (b)-[:DEPENDS_ON {{dep_type: 'data'}}]->(a)",
                            escape_str(&producer.id),
                            escape_str(&consumer.id),
                        ))
                        .map_err(|e| anyhow::anyhow!("create DEPENDS_ON: {e}"))?;
                        count += 1;
                        break;
                    }
                }
            }
        }
        Ok(count)
    }

    pub fn query_pipelines_by_compliance(&self, scope: &str) -> Result<Vec<PipelineRecord>> {
        let conn = self.connection()?;
        let scope_lower = scope.to_lowercase();
        let mut result = conn
            .query(&format!(
                "MATCH (p:Pipeline) WHERE lower(p.compliance) CONTAINS '{}' RETURN p.id, p.name, p.doc_id, p.source_systems, p.dest_tables, p.scheduler_type, p.scheduler_config, p.compliance, p.github_repo, p.daci, p.idempotent, p.business_logic_summary, p.data_quality, p.dependencies_upstream, p.dependencies_downstream",
                escape_str(&scope_lower)
            ))
            .map_err(|e| anyhow::anyhow!("compliance query: {e}"))?;

        let mut pipelines = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 15 {
                pipelines.push(PipelineRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    source_systems: row[3].to_string(),
                    dest_tables: row[4].to_string(),
                    scheduler_type: row[5].to_string(),
                    scheduler_config: row[6].to_string(),
                    compliance: row[7].to_string(),
                    github_repo: row[8].to_string(),
                    daci: row[9].to_string(),
                    idempotent: row[10].to_string(),
                    business_logic_summary: row[11].to_string(),
                    data_quality: row[12].to_string(),
                    dependencies_upstream: row[13].to_string(),
                    dependencies_downstream: row[14].to_string(),
                });
            }
        }
        Ok(pipelines)
    }

    pub fn impact_analysis(&self, table_name: &str, max_depth: u32) -> Result<Vec<ImpactResult>> {
        let table_lower = table_name.to_lowercase();
        let pipelines = self.get_all_pipelines()?;

        let mut affected: Vec<ImpactResult> = Vec::new();
        let mut affected_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for p in &pipelines {
            let sources_lower = p.source_systems.to_lowercase();
            let upstream_lower = p.dependencies_upstream.to_lowercase();
            if sources_lower.contains(&table_lower) || upstream_lower.contains(&table_lower) {
                affected_ids.insert(p.id.clone());
                affected.push(ImpactResult {
                    pipeline_id: p.id.clone(),
                    pipeline_name: p.name.clone(),
                    impact_type: "direct".to_string(),
                    depth: 1,
                    path: format!("{} → {}", table_name, p.name),
                });
            }
        }

        if max_depth > 1 && !affected_ids.is_empty() {
            let conn = self.connection()?;
            for depth in 2..=max_depth {
                let current_ids: Vec<String> = affected_ids.iter().cloned().collect();
                let mut new_ids = Vec::new();

                for src_id in &current_ids {
                    let mut result = conn
                        .query(&format!(
                            "MATCH (a:Pipeline)-[:DEPENDS_ON]->(b:Pipeline) WHERE b.id = '{}' RETURN a.id, a.name",
                            escape_str(src_id)
                        ))
                        .map_err(|e| anyhow::anyhow!("transitive impact query: {e}"))?;

                    while let Some(row) = result.next() {
                        if row.len() >= 2 {
                            let dep_id = row[0].to_string();
                            if !affected_ids.contains(&dep_id) {
                                let dep_name = row[1].to_string();
                                let src_name = pipelines.iter()
                                    .find(|p| p.id == *src_id)
                                    .map(|p| p.name.as_str())
                                    .unwrap_or(src_id);
                                affected.push(ImpactResult {
                                    pipeline_id: dep_id.clone(),
                                    pipeline_name: dep_name,
                                    impact_type: "transitive".to_string(),
                                    depth,
                                    path: format!("{} → ... → {}", table_name, src_name),
                                });
                                new_ids.push(dep_id);
                            }
                        }
                    }
                }

                if new_ids.is_empty() {
                    break;
                }
                affected_ids.extend(new_ids);
            }
        }

        Ok(affected)
    }

    pub fn get_pipeline_deps(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.connection()?;
        let mut result = conn
            .query("MATCH (a:Pipeline)-[r:DEPENDS_ON]->(b:Pipeline) RETURN a.name, b.name, r.dep_type")
            .map_err(|e| anyhow::anyhow!("query pipeline deps: {e}"))?;
        let mut deps = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 3 {
                deps.push((row[0].to_string(), row[1].to_string(), row[2].to_string()));
            }
        }
        Ok(deps)
    }

    pub fn link_pipelines_to_repo_files(&self) -> Result<usize> {
        let pipelines = self.get_all_pipelines()?;
        if pipelines.is_empty() {
            return Ok(0);
        }

        let conn = self.connection()?;

        let mut doc_result = conn
            .query("MATCH (d:Document) WHERE NOT starts_with(d.file, 'confluence://') RETURN d.id, d.file")
            .map_err(|e| anyhow::anyhow!("query repo docs: {e}"))?;
        let mut repo_docs: Vec<(String, String)> = Vec::new();
        while let Some(row) = doc_result.next() {
            if row.len() >= 2 {
                repo_docs.push((row[0].to_string(), row[1].to_string()));
            }
        }
        if repo_docs.is_empty() {
            return Ok(0);
        }

        let mut chunk_result = conn
            .query("MATCH (c:Chunk) WHERE NOT starts_with(c.doc_file, 'confluence://') RETURN c.doc_file, c.text")
            .map_err(|e| anyhow::anyhow!("query repo chunks: {e}"))?;
        let mut file_content: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        while let Some(row) = chunk_result.next() {
            if row.len() >= 2 {
                file_content.entry(row[0].to_string())
                    .or_default()
                    .push_str(&row[1].to_string());
            }
        }

        let mut count = 0;
        for pipeline in &pipelines {
            let tables: Vec<&str> = pipeline.source_systems.split(',')
                .chain(pipeline.dest_tables.split(','))
                .map(|t| t.trim())
                .filter(|t| t.contains('.') && t.len() > 3)
                .collect();
            if tables.is_empty() {
                continue;
            }

            for (doc_id, _doc_file) in &repo_docs {
                let content = match file_content.get(doc_id) {
                    Some(c) => c,
                    None => continue,
                };
                let content_lower = content.to_lowercase();

                let matches = tables.iter().any(|table| {
                    content_lower.contains(&table.to_lowercase())
                });

                if matches {
                    let _ = conn.query(&format!(
                        "MATCH (p:Pipeline), (d:Document) WHERE p.id = '{}' AND d.id = '{}' CREATE (p)-[:DEFINED_IN]->(d)",
                        escape_str(&pipeline.id),
                        escape_str(doc_id),
                    ));
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    pub fn get_pipeline_search_docs(&self) -> Result<Vec<(String, String)>> {
        let pipelines = self.get_all_pipelines()?;
        let mut docs = Vec::new();
        for p in &pipelines {
            let text = format!(
                "Pipeline: {name}\nSource Systems: {sources}\nDestination Tables: {dest}\nScheduler: {sched_type} {sched_config}\nCompliance: {compliance}\nOwner (DACI): {daci}\nGitHub Repo: {repo}\nBusiness Logic: {logic}\nUpstream Dependencies: {up}\nDownstream Dependencies: {down}",
                name = p.name,
                sources = p.source_systems,
                dest = p.dest_tables,
                sched_type = p.scheduler_type,
                sched_config = p.scheduler_config,
                compliance = p.compliance,
                daci = p.daci,
                repo = p.github_repo,
                logic = p.business_logic_summary,
                up = p.dependencies_upstream,
                down = p.dependencies_downstream,
            );
            docs.push((format!("pipeline::{}", p.id), text));
        }
        Ok(docs)
    }

    // ── PipelineCore methods (Phase A — plugin pipeline system) ──────────

    /// Create a per-plugin node table from schema definition.
    pub fn ensure_plugin_table(&self, plugin_id: &str, columns: &[(String, String)]) -> Result<()> {
        let conn = self.connection()?;
        let mut col_defs = String::from("id STRING");
        for (name, col_type) in columns {
            col_defs.push_str(&format!(", {} {}", name, col_type));
        }
        let ddl = format!(
            "CREATE NODE TABLE IF NOT EXISTS Pipeline_{}({}, PRIMARY KEY(id))",
            plugin_id, col_defs
        );
        conn.query(&ddl)
            .map_err(|e| anyhow::anyhow!("ensure_plugin_table DDL: {e}"))?;
        Ok(())
    }

    /// Upsert a pipeline core record.
    pub fn upsert_pipeline_core(&self, record: &PipelineCoreRecord) -> Result<()> {
        let conn = self.connection()?;
        // Delete existing if present
        let _ = conn.query(&format!(
            "MATCH (p:PipelineCore) WHERE p.id = '{}' DETACH DELETE p",
            escape_str(&record.id)
        ));
        // Build list literals
        let inputs_str = record
            .inputs
            .iter()
            .map(|s| format!("'{}'", escape_str(s)))
            .collect::<Vec<_>>()
            .join(",");
        let outputs_str = record
            .outputs
            .iter()
            .map(|s| format!("'{}'", escape_str(s)))
            .collect::<Vec<_>>()
            .join(",");
        conn.query(&format!(
            "CREATE (p:PipelineCore {{id: '{}', name: '{}', doc_id: '{}', plugin_id: '{}', inputs: [{}], outputs: [{}]}})",
            escape_str(&record.id),
            escape_str(&record.name),
            escape_str(&record.doc_id),
            escape_str(&record.plugin_id),
            inputs_str,
            outputs_str,
        ))
        .map_err(|e| anyhow::anyhow!("create PipelineCore: {e}"))?;
        Ok(())
    }

    /// Upsert plugin-specific properties into Pipeline_<plugin_id> table.
    pub fn upsert_plugin_properties(
        &self,
        pipeline_id: &str,
        plugin_id: &str,
        properties: &serde_json::Map<String, serde_json::Value>,
        schema: &[(String, String)],
    ) -> Result<()> {
        let conn = self.connection()?;
        let table = format!("Pipeline_{}", plugin_id);
        let esc_id = escape_str(pipeline_id);
        // Delete existing
        let _ = conn.query(&format!(
            "MATCH (p:{}) WHERE p.id = '{}' DELETE p",
            table, esc_id
        ));
        // Build property assignments
        let mut props = format!("id: '{}'", esc_id);
        for (col_name, _col_type) in schema {
            if let Some(val) = properties.get(col_name.as_str()) {
                let s = match val {
                    serde_json::Value::String(s) => escape_str(s),
                    other => escape_str(&other.to_string()),
                };
                props.push_str(&format!(", {}: '{}'", col_name, s));
            }
        }
        conn.query(&format!("CREATE (p:{} {{{}}})", table, props))
            .map_err(|e| anyhow::anyhow!("upsert plugin properties: {e}"))?;
        Ok(())
    }

    /// Link a pipeline core to a document via PIPELINE_DEFINED_IN edge.
    pub fn link_pipeline_core_to_doc(&self, pipeline_id: &str, doc_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.query(&format!(
            "MATCH (p:PipelineCore), (d:Document) WHERE p.id = '{}' AND d.id = '{}' CREATE (p)-[:PIPELINE_DEFINED_IN]->(d)",
            escape_str(pipeline_id),
            escape_str(doc_id),
        ))
        .map_err(|e| anyhow::anyhow!("link PIPELINE_DEFINED_IN: {e}"))?;
        Ok(())
    }

    /// Link pipeline dependencies using PipelineCore inputs/outputs matching.
    /// Cross-plugin: if plugin A's output matches plugin B's input, creates PIPELINE_DEPENDS_ON edge.
    pub fn link_pipeline_dependencies_v2(&self) -> Result<usize> {
        let cores = self.get_all_pipeline_cores(None)?;
        if cores.len() < 2 {
            return Ok(0);
        }

        let conn = self.connection()?;
        // Clear old edges
        let _ = conn.query("MATCH ()-[r:PIPELINE_DEPENDS_ON]->() DELETE r");

        let mut count = 0;
        for producer in &cores {
            if producer.outputs.is_empty() {
                continue;
            }
            for consumer in &cores {
                if consumer.id == producer.id || consumer.inputs.is_empty() {
                    continue;
                }
                // Check if any output of producer matches any input of consumer
                let has_match = producer
                    .outputs
                    .iter()
                    .any(|out| consumer.inputs.iter().any(|inp| inp == out));
                if has_match {
                    conn.query(&format!(
                        "MATCH (a:PipelineCore), (b:PipelineCore) WHERE a.id = '{}' AND b.id = '{}' CREATE (a)-[:PIPELINE_DEPENDS_ON {{dep_type: 'data'}}]->(b)",
                        escape_str(&consumer.id),
                        escape_str(&producer.id),
                    ))
                    .map_err(|e| anyhow::anyhow!("create PIPELINE_DEPENDS_ON: {e}"))?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// Get all PipelineCore records, optionally filtered by plugin_id.
    pub fn get_all_pipeline_cores(
        &self,
        plugin_id: Option<&str>,
    ) -> Result<Vec<PipelineCoreRecord>> {
        let conn = self.connection()?;
        let query = match plugin_id {
            Some(pid) => format!(
                "MATCH (p:PipelineCore) WHERE p.plugin_id = '{}' RETURN p.id, p.name, p.doc_id, p.plugin_id, p.inputs, p.outputs",
                escape_str(pid)
            ),
            None => "MATCH (p:PipelineCore) RETURN p.id, p.name, p.doc_id, p.plugin_id, p.inputs, p.outputs".to_string(),
        };
        let mut result = conn
            .query(&query)
            .map_err(|e| anyhow::anyhow!("query pipeline cores: {e}"))?;
        let mut records = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 6 {
                records.push(PipelineCoreRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    plugin_id: row[3].to_string(),
                    inputs: parse_string_list(&row[4].to_string()),
                    outputs: parse_string_list(&row[5].to_string()),
                });
            }
        }
        Ok(records)
    }

    /// Get a PipelineCore record by id.
    pub fn get_pipeline_core(&self, pipeline_id: &str) -> Result<Option<PipelineCoreRecord>> {
        let conn = self.connection()?;
        let mut result = conn
            .query(&format!(
                "MATCH (p:PipelineCore) WHERE p.id = '{}' RETURN p.id, p.name, p.doc_id, p.plugin_id, p.inputs, p.outputs",
                escape_str(pipeline_id)
            ))
            .map_err(|e| anyhow::anyhow!("query pipeline core: {e}"))?;
        if let Some(row) = result.next() {
            if row.len() >= 6 {
                return Ok(Some(PipelineCoreRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    plugin_id: row[3].to_string(),
                    inputs: parse_string_list(&row[4].to_string()),
                    outputs: parse_string_list(&row[5].to_string()),
                }));
            }
        }
        Ok(None)
    }

    /// Impact analysis using PipelineCore inputs/outputs.
    pub fn impact_analysis_v2(
        &self,
        table_name: &str,
        max_depth: u32,
    ) -> Result<Vec<ImpactResult>> {
        let conn = self.connection()?;
        let esc = escape_str(table_name);
        let mut results = Vec::new();

        // Direct impact: pipelines that consume this table
        let mut direct = conn
            .query(&format!(
                "MATCH (p:PipelineCore) WHERE list_contains(p.inputs, '{}') RETURN p.id, p.name",
                esc
            ))
            .map_err(|e| anyhow::anyhow!("impact_analysis_v2 direct: {e}"))?;
        let mut affected_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        while let Some(row) = direct.next() {
            if row.len() >= 2 {
                let id = row[0].to_string();
                let name = row[1].to_string();
                affected_ids.insert(id.clone());
                results.push(ImpactResult {
                    pipeline_id: id,
                    pipeline_name: name,
                    impact_type: "direct".to_string(),
                    depth: 1,
                    path: table_name.to_string(),
                });
            }
        }

        // Transitive impact via PIPELINE_DEPENDS_ON edges
        if max_depth > 1 && !affected_ids.is_empty() {
            for depth in 2..=max_depth {
                let current_ids: Vec<String> = affected_ids.iter().cloned().collect();
                let mut new_ids = Vec::new();

                for src_id in &current_ids {
                    let mut trans = conn
                        .query(&format!(
                            "MATCH (a:PipelineCore)-[:PIPELINE_DEPENDS_ON]->(b:PipelineCore) WHERE b.id = '{}' RETURN a.id, a.name",
                            escape_str(src_id)
                        ))
                        .map_err(|e| anyhow::anyhow!("impact_analysis_v2 transitive: {e}"))?;

                    while let Some(row) = trans.next() {
                        if row.len() >= 2 {
                            let id = row[0].to_string();
                            if !affected_ids.contains(&id) {
                                results.push(ImpactResult {
                                    pipeline_id: id.clone(),
                                    pipeline_name: row[1].to_string(),
                                    impact_type: "transitive".to_string(),
                                    depth,
                                    path: format!("{} → ... (depth {})", table_name, depth),
                                });
                                new_ids.push(id);
                            }
                        }
                    }
                }

                if new_ids.is_empty() {
                    break;
                }
                affected_ids.extend(new_ids);
            }
        }

        Ok(results)
    }

    /// Get all PIPELINE_DEPENDS_ON edges as (from_name, to_name, dep_type) tuples.
    pub fn get_pipeline_deps_v2(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.connection()?;
        let mut result = conn
            .query(
                "MATCH (c:PipelineCore)-[r:PIPELINE_DEPENDS_ON]->(p:PipelineCore) \
                 RETURN c.name, p.name, r.dep_type",
            )
            .map_err(|e| anyhow::anyhow!("query pipeline deps v2: {e}"))?;
        let mut deps = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 3 {
                deps.push((row[0].to_string(), row[1].to_string(), row[2].to_string()));
            }
        }
        Ok(deps)
    }

    /// Query a plugin-specific table by field value.
    pub fn query_plugin_table(
        &self,
        plugin_id: &str,
        field: &str,
        value: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.connection()?;
        let table = format!("Pipeline_{}", plugin_id);
        let esc_val = escape_str(value);
        let mut result = conn
            .query(&format!(
                "MATCH (p:{}) WHERE lower(p.{}) CONTAINS lower('{}') RETURN p.*",
                table, field, esc_val
            ))
            .map_err(|e| anyhow::anyhow!("query plugin table: {e}"))?;
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            let vals: Vec<serde_json::Value> = row
                .iter()
                .map(|v| serde_json::Value::String(v.to_string()))
                .collect();
            rows.push(serde_json::Value::Array(vals));
        }
        Ok(rows)
    }

    /// Pipeline count for stats (using PipelineCore).
    pub fn pipeline_core_count(&self) -> Result<usize> {
        let conn = self.connection()?;
        Ok(count_query(&conn, "MATCH (p:PipelineCore) RETURN count(p)"))
    }

    pub fn get_all_chunks(&self) -> Result<Vec<(String, String)>> {
        let conn = self.connection()?;
        let result = conn
            .query("MATCH (c:Chunk) RETURN c.id, c.text")
            .map_err(|e| anyhow::anyhow!("query chunks: {e}"))?;
        let mut chunks = Vec::new();
        for row in result {
            if row.len() >= 2 {
                chunks.push((row[0].to_string(), row[1].to_string()));
            }
        }
        Ok(chunks)
    }

    pub fn get_chunk_ids(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.connection()?;
        let result = conn
            .query("MATCH (c:Chunk) RETURN c.id")
            .map_err(|e| anyhow::anyhow!("query chunk ids: {e}"))?;
        let mut ids = std::collections::HashSet::new();
        for row in result {
            if !row.is_empty() {
                ids.insert(row[0].to_string());
            }
        }
        Ok(ids)
    }

    pub fn get_chunk_details(&self, chunk_ids: &[&str]) -> Result<Vec<ChunkDetail>> {
        let conn = self.connection()?;
        let id_list: String = chunk_ids
            .iter()
            .map(|id| format!("'{}'", escape_str(id)))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "MATCH (c:Chunk) WHERE c.id IN [{}] RETURN c.id, c.doc_file, c.idx, c.heading, c.text, c.start_offset, c.end_offset, c.page",
            id_list
        );
        let result = conn
            .query(&query)
            .map_err(|e| anyhow::anyhow!("chunk details: {e}"))?;
        let mut details = Vec::new();
        for row in result {
            if row.len() >= 8 {
                let heading_str = row[3].to_string();
                let page_val: i64 = row[7].to_string().parse().unwrap_or(0);
                details.push(ChunkDetail {
                    id: row[0].to_string(),
                    doc_file: row[1].to_string(),
                    index: row[2].to_string().parse().unwrap_or(0),
                    heading: if heading_str.is_empty() {
                        None
                    } else {
                        Some(heading_str)
                    },
                    text: row[4].to_string(),
                    start_offset: row[5].to_string().parse().unwrap_or(0),
                    end_offset: row[6].to_string().parse().unwrap_or(0),
                    page: if page_val > 0 {
                        Some(page_val as usize)
                    } else {
                        None
                    },
                });
            }
        }
        Ok(details)
    }
}

#[derive(Debug)]
pub struct DocStoreStats {
    pub document_count: usize,
    pub chunk_count: usize,
    pub pipeline_count: usize,
}

#[derive(Debug, Clone)]
pub struct ChunkDetail {
    pub id: String,
    pub doc_file: String,
    pub index: usize,
    pub heading: Option<String>,
    pub text: String,
    pub start_offset: usize,
    pub end_offset: usize,
    pub page: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct PipelineRecord {
    pub id: String,
    pub name: String,
    pub doc_id: String,
    pub source_systems: String,
    pub dest_tables: String,
    pub scheduler_type: String,
    pub scheduler_config: String,
    pub compliance: String,
    pub github_repo: String,
    pub daci: String,
    pub idempotent: String,
    pub business_logic_summary: String,
    pub data_quality: String,
    pub dependencies_upstream: String,
    pub dependencies_downstream: String,
}

#[derive(Debug, Clone)]
pub struct PipelineCoreRecord {
    pub id: String,
    pub name: String,
    pub doc_id: String,
    pub plugin_id: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactResult {
    pub pipeline_id: String,
    pub pipeline_name: String,
    pub impact_type: String,
    pub depth: u32,
    pub path: String,
}

fn count_query(conn: &Connection<'_>, query: &str) -> usize {
    conn.query(query)
        .ok()
        .and_then(|mut r| r.next().map(|row| row[0].to_string().parse().unwrap_or(0)))
        .unwrap_or(0)
}

fn escape_str(s: &str) -> String {
    s.replace('\'', "\\'")
}

/// Parse a Kuzu STRING[] column rendered via `.to_string()`.
/// Kuzu returns STRING[] as "[val1,val2,val3]".
fn parse_string_list(s: &str) -> Vec<String> {
    let trimmed = s.trim_matches(|c| c == '[' || c == ']');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().trim_matches('\'').trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
