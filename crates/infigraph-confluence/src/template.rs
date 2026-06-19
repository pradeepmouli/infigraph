use regex::Regex;
use serde_json::Value;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Overview,
    Source,
    Destination,
    Compliance,
    Scheduler,
    Daci,
    DataQuality,
    BusinessLogic,
    Idempotency,
    GithubRepo,
    Dependencies,
}

struct SectionMatcher {
    keywords: Vec<(&'static str, Section)>,
    aliases: Vec<(&'static str, Section)>,
}

impl SectionMatcher {
    fn new() -> Self {
        Self {
            keywords: vec![
                ("overview", Section::Overview),
                ("source system", Section::Source),
                ("source systems", Section::Source),
                ("destination", Section::Destination),
                ("dest table", Section::Destination),
                ("destination table", Section::Destination),
                ("compliance", Section::Compliance),
                ("data classification", Section::Compliance),
                ("scheduler", Section::Scheduler),
                ("daci", Section::Daci),
                ("data quality", Section::DataQuality),
                ("business logic", Section::BusinessLogic),
                ("idempotency", Section::Idempotency),
                ("idempotent", Section::Idempotency),
                ("github repo", Section::GithubRepo),
                ("github", Section::GithubRepo),
                ("dependenc", Section::Dependencies),
            ],
            aliases: vec![
                ("bpp job", Section::Scheduler),
                ("design description", Section::BusinessLogic),
                ("error handling", Section::Idempotency),
                ("security", Section::Compliance),
                ("s3 partitioning", Section::Destination),
                ("target layer", Section::Destination),
                ("architecture", Section::Overview),
            ],
        }
    }

    fn classify(&self, heading: &str) -> Option<Section> {
        let lower = heading.to_lowercase();

        for &(alias, section) in &self.aliases {
            if lower.contains(alias) {
                return Some(section);
            }
        }

        for &(keyword, section) in &self.keywords {
            if lower.contains(keyword) {
                return Some(section);
            }
        }

        None
    }
}

#[derive(Debug)]
struct SectionContent {
    section: Section,
    _heading: String,
    text: String,
}

pub fn parse_pipeline_template(content: &str, title: &str, doc_id: &str) -> Option<PipelineRecord> {
    let heading_re = Regex::new(r"(?m)^(#{1,6})\s+(.+)$").unwrap();
    let matcher = SectionMatcher::new();

    let mut sections: Vec<SectionContent> = Vec::new();

    let heading_positions: Vec<(usize, usize, String)> = heading_re
        .captures_iter(content)
        .map(|cap| {
            let m = cap.get(0).unwrap();
            let level = cap[1].len();
            let text = cap[2].trim().to_string();
            (m.start(), level, text)
        })
        .collect();

    for i in 0..heading_positions.len() {
        let (start, level, ref heading) = heading_positions[i];

        if let Some(section) = matcher.classify(heading) {
            let end = heading_positions[i + 1..]
                .iter()
                .find(|(_, l, _)| *l <= level)
                .map(|(s, _, _)| *s)
                .unwrap_or(content.len());

            let body_start = content[start..]
                .find('\n')
                .map(|p| start + p + 1)
                .unwrap_or(end);
            let body = content[body_start..end].trim().to_string();
            sections.push(SectionContent {
                section,
                _heading: heading.clone(),
                text: body,
            });
        }
    }

    if sections.is_empty() {
        return None;
    }

    let mut record = PipelineRecord {
        id: format!("pipeline::{}", doc_id),
        name: title.to_string(),
        doc_id: doc_id.to_string(),
        ..Default::default()
    };

    for sc in &sections {
        match sc.section {
            Section::Source => {
                record.source_systems = extract_sources(&sc.text);
            }
            Section::Destination => {
                record.dest_tables = extract_tables(&sc.text);
            }
            Section::Scheduler => {
                let (stype, config) = extract_scheduler(&sc.text);
                record.scheduler_type = stype;
                record.scheduler_config = config;
            }
            Section::Compliance => {
                record.compliance = summarize_section(&sc.text, 500);
            }
            Section::GithubRepo => {
                record.github_repo = extract_github_repo(&sc.text);
            }
            Section::Daci => {
                record.daci = extract_daci(&sc.text);
            }
            Section::Idempotency => {
                record.idempotent = summarize_section(&sc.text, 300);
            }
            Section::BusinessLogic => {
                record.business_logic_summary = summarize_section(&sc.text, 500);
            }
            Section::DataQuality => {
                record.data_quality = summarize_section(&sc.text, 300);
            }
            Section::Dependencies => {
                let (up, down) = extract_dependencies(&sc.text);
                if !up.is_empty() {
                    record.dependencies_upstream = up;
                }
                if !down.is_empty() {
                    record.dependencies_downstream = down;
                }
            }
            Section::Overview => {}
        }
    }

    Some(record)
}

fn extract_sources(text: &str) -> String {
    let mut sources = Vec::new();

    let table_re =
        Regex::new(r"(?i)(?:table|schema|database|source)[:\s]*[`]?(\w+\.\w+(?:\.\w+)?)[`]?")
            .unwrap();
    for cap in table_re.captures_iter(text) {
        sources.push(cap[1].to_string());
    }

    let qualified_re = Regex::new(r"\b(\w+_(?:src|dm|rpt|raw|stg)\.\w+)\b").unwrap();
    for cap in qualified_re.captures_iter(text) {
        let t = cap[1].to_string();
        if !sources.contains(&t) {
            sources.push(t);
        }
    }

    if sources.is_empty() {
        summarize_section(text, 200)
    } else {
        sources.join(", ")
    }
}

fn extract_tables(text: &str) -> String {
    let mut tables = Vec::new();

    let heading_table_re =
        Regex::new(r"(?i)(?:^|\n)#{1,6}\s+(?:Table[:\s]*)?[`]?(\w+\.\w+(?:\.\w+)?)[`]?").unwrap();
    for cap in heading_table_re.captures_iter(text) {
        let t = cap[1].to_string();
        if !tables.contains(&t) {
            tables.push(t);
        }
    }

    let qualified_re = Regex::new(r"\b(\w+_(?:dm|rpt|src|raw|stg|mart)\.\w+)\b").unwrap();
    for cap in qualified_re.captures_iter(text) {
        let t = cap[1].to_string();
        if !tables.contains(&t) {
            tables.push(t);
        }
    }

    let backtick_re = Regex::new(r"`(\w+\.\w+(?:\.\w+)?)`").unwrap();
    for cap in backtick_re.captures_iter(text) {
        let t = cap[1].to_string();
        if !tables.contains(&t) {
            tables.push(t);
        }
    }

    if tables.is_empty() {
        summarize_section(text, 200)
    } else {
        tables.join(", ")
    }
}

fn extract_scheduler(text: &str) -> (String, String) {
    let lower = text.to_lowercase();
    let stype = if lower.contains("bpp") || lower.contains("batch processing") {
        "BPP".to_string()
    } else if lower.contains("airflow") {
        "Airflow".to_string()
    } else if lower.contains("cron") {
        "Cron".to_string()
    } else if lower.contains("quicketl") || lower.contains("quick_etl") {
        "QuickETL".to_string()
    } else {
        "unknown".to_string()
    };

    let config = summarize_section(text, 300);
    (stype, config)
}

fn extract_github_repo(text: &str) -> String {
    let url_re = Regex::new(r"https?://(?:github\.com|github\.intuit\.com)/[^\s\)]+").unwrap();
    if let Some(m) = url_re.find(text) {
        return m.as_str().to_string();
    }
    let repo_re = Regex::new(r"(?i)(?:repo|repository)[:\s]*[`]?([a-zA-Z0-9_/-]+)[`]?").unwrap();
    if let Some(cap) = repo_re.captures(text) {
        return cap[1].to_string();
    }
    text.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn extract_daci(text: &str) -> String {
    let mut parts = Vec::new();
    let role_re = Regex::new(
        r"(?im)^\s*\**\s*(Driver|Approver|Contributor|Informed|Accountable)[:\s*]*(.+)$",
    )
    .unwrap();
    for cap in role_re.captures_iter(text) {
        parts.push(format!("{}: {}", &cap[1], cap[2].trim()));
    }
    if parts.is_empty() {
        summarize_section(text, 200)
    } else {
        parts.join("; ")
    }
}

fn extract_dependencies(text: &str) -> (String, String) {
    let mut upstream = Vec::new();
    let mut downstream = Vec::new();
    let mut current_section = "";

    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if lower.contains("upstream") && (trimmed.starts_with('#') || trimmed.starts_with("**")) {
            current_section = "up";
            continue;
        }
        if lower.contains("downstream") && (trimmed.starts_with('#') || trimmed.starts_with("**")) {
            current_section = "down";
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("|--")
            || trimmed.starts_with("| --")
            || trimmed.chars().all(|c| c == '-' || c == '|' || c == ' ')
        {
            continue;
        }
        if trimmed.starts_with('|')
            && (lower.contains("dependency") || lower.contains("owner") || lower.contains("sla"))
        {
            continue;
        }
        if trimmed.starts_with('|') {
            let cells: Vec<&str> = trimmed
                .split('|')
                .map(|c| c.trim())
                .filter(|c| !c.is_empty())
                .collect();
            if let Some(first) = cells.first() {
                let item = first.to_string();
                if !item.is_empty() {
                    match current_section {
                        "up" => upstream.push(item),
                        "down" => downstream.push(item),
                        _ => {}
                    }
                }
            }
            continue;
        }
        let item = trimmed.trim_start_matches(['-', '*', '•', ' ']);
        if item.is_empty() {
            continue;
        }
        match current_section {
            "up" => upstream.push(item.to_string()),
            "down" => downstream.push(item.to_string()),
            _ => {}
        }
    }

    (upstream.join(", "), downstream.join(", "))
}

fn summarize_section(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.len() <= max_chars {
        cleaned
    } else {
        let boundary = cleaned.floor_char_boundary(max_chars);
        format!("{}...", &cleaned[..boundary])
    }
}

fn needs_llm_fallback(record: &PipelineRecord) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if record.source_systems.is_empty() {
        missing.push("source_systems");
    }
    if record.dest_tables.is_empty() {
        missing.push("dest_tables");
    }
    if record.scheduler_type.is_empty() || record.scheduler_type == "unknown" {
        missing.push("scheduler_type");
    }
    if record.github_repo.is_empty() {
        missing.push("github_repo");
    }
    if record.daci.is_empty() {
        missing.push("daci");
    }
    if record.business_logic_summary.is_empty() {
        missing.push("business_logic_summary");
    }
    missing
}

fn build_extraction_prompt(content: &str, title: &str, missing_fields: &[&str]) -> String {
    let fields_desc: Vec<&str> = missing_fields.iter().map(|f| match *f {
        "source_systems" => "source_systems: comma-separated list of source tables/systems (e.g. tax_dm.fact_tax_w2_metric, commerce_profile)",
        "dest_tables" => "dest_tables: comma-separated list of destination/target tables (e.g. tax_rpt.rpt_marketing_attributes)",
        "scheduler_type" => "scheduler_type: one of BPP, Airflow, Cron, QuickETL, or unknown",
        "github_repo" => "github_repo: GitHub repository URL or name",
        "daci" => "daci: Driver, Approver, Contributors, Informed roles and names",
        "business_logic_summary" => "business_logic_summary: 1-2 sentence summary of the pipeline's business logic",
        _ => "",
    }).filter(|s| !s.is_empty()).collect();

    format!(
        "Extract the following fields from this pipeline design document. Return ONLY a JSON object with the requested fields. No explanation.\n\nFields needed:\n{}\n\nDocument title: {}\n\nDocument content (truncated):\n{}",
        fields_desc.join("\n"),
        title,
        &content[..content.len().min(6000)]
    )
}

fn call_claude_extract(prompt: &str) -> Option<Value> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
    let model = std::env::var("INFIGRAPH_LLM_MODEL")
        .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
    let base_url = std::env::var("INFIGRAPH_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": prompt}],
    });

    let resp = ureq::post(&format!("{}/v1/messages", base_url))
        .set("x-api-key", &api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_string(&body.to_string())
        .ok()?;

    let resp_body: Value = resp.into_json().ok()?;
    let text = resp_body["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|block| block["text"].as_str())?;

    let json_str = if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            &text[start..=end]
        } else {
            text
        }
    } else {
        text
    };

    serde_json::from_str(json_str).ok()
}

pub fn fill_with_llm(record: &mut PipelineRecord, content: &str, title: &str) -> usize {
    if std::env::var("INFIGRAPH_LLM_EXTRACT").is_err() {
        return 0;
    }

    let missing = needs_llm_fallback(record);
    if missing.is_empty() {
        return 0;
    }

    let prompt = build_extraction_prompt(content, title, &missing);
    let Some(json) = call_claude_extract(&prompt) else {
        eprintln!(
            "LLM extraction failed for pipeline '{}' (missing: {})",
            title,
            missing.join(", ")
        );
        return 0;
    };

    let mut filled = 0;
    for field in &missing {
        if let Some(val) = json.get(field).and_then(|v| v.as_str()) {
            if val.is_empty() {
                continue;
            }
            match *field {
                "source_systems" => {
                    record.source_systems = val.to_string();
                    filled += 1;
                }
                "dest_tables" => {
                    record.dest_tables = val.to_string();
                    filled += 1;
                }
                "scheduler_type" => {
                    record.scheduler_type = val.to_string();
                    filled += 1;
                }
                "github_repo" => {
                    record.github_repo = val.to_string();
                    filled += 1;
                }
                "daci" => {
                    record.daci = val.to_string();
                    filled += 1;
                }
                "business_logic_summary" => {
                    record.business_logic_summary = val.to_string();
                    filled += 1;
                }
                _ => {}
            }
        }
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_pipeline_content() -> &'static str {
        r#"# Pipeline: Mystery ETL

## Overview
This pipeline does something.

## Source System Details
Data comes from various internal systems via shared drives.

## Destination Tables
Output goes to the data lake.

## Scheduler
Runs nightly.

## DACI
Team owns it.

## Business Logic
Complex transformation logic.

## Dependencies
### Upstream
- system_a
### Downstream
- system_b
"#
    }

    fn full_pipeline_content() -> &'static str {
        r#"# Pipeline: W2 Metrics

## Overview
W2 metrics pipeline.

## Source System Details
Source table: `tax_src.raw_w2_data`
Schema: tax_dm.fact_w2_metric

## Destination Tables
### tax_rpt.rpt_w2_summary
Destination for W2 summary data.

## Compliance
PII — SSN masked. Data classification: Restricted.

## Scheduler
BPP job runs daily at 2am UTC. Job name: `w2_metrics_daily`.

## DACI
**Driver:** Alice
**Approver:** Bob
**Contributor:** Charlie, Dave
**Informed:** Eve

## Business Logic
Aggregates W2 forms by employer EIN, computes YTD totals, applies withholding rules per IRS pub 15.

## Github Repo
https://github.intuit.com/tax-data/w2-metrics-pipeline

## Dependencies
### Upstream
| Dependency | Owner | SLA |
|---|---|---|
| tax_src.raw_w2_data | Tax Ingestion | 1am UTC |
| ref_data.employer_dim | MDM | hourly |
### Downstream
| Dependency | Owner | SLA |
|---|---|---|
| tax_rpt.executive_dashboard | BI Team | 6am UTC |
"#
    }

    #[test]
    fn test_parse_full_pipeline_all_fields_extracted() {
        let content = full_pipeline_content();
        let record = parse_pipeline_template(content, "W2 Metrics", "doc::w2").unwrap();

        assert!(record.source_systems.contains("tax_src.raw_w2_data"));
        assert!(record.source_systems.contains("tax_dm.fact_w2_metric"));
        assert!(record.dest_tables.contains("tax_rpt.rpt_w2_summary"));
        assert_eq!(record.scheduler_type, "BPP");
        assert!(record.scheduler_config.contains("daily"));
        assert!(record.github_repo.contains("github.intuit.com"));
        assert!(record.daci.contains("Alice"));
        assert!(record.daci.contains("Bob"));
        assert!(!record.business_logic_summary.is_empty());
        assert!(!record.compliance.is_empty());

        let missing = needs_llm_fallback(&record);
        assert!(
            missing.is_empty(),
            "Full pipeline should have no missing fields, got: {:?}",
            missing
        );
    }

    #[test]
    fn test_parse_minimal_pipeline_identifies_missing_fields() {
        let content = minimal_pipeline_content();
        let record = parse_pipeline_template(content, "Mystery ETL", "doc::mystery").unwrap();

        assert_eq!(record.name, "Mystery ETL");
        assert_eq!(record.doc_id, "doc::mystery");

        assert!(
            !record.source_systems.is_empty(),
            "source_systems gets prose summary fallback"
        );
        assert!(
            !record.dest_tables.is_empty(),
            "dest_tables gets prose summary fallback"
        );
        assert!(!record.daci.is_empty(), "daci gets prose summary fallback");

        let missing = needs_llm_fallback(&record);
        assert!(
            missing.contains(&"scheduler_type"),
            "scheduler_type should be 'unknown' → needs LLM"
        );
        assert!(
            missing.contains(&"github_repo"),
            "github_repo should be empty — no section matched"
        );
    }

    #[test]
    fn test_truly_empty_fields_trigger_llm_fallback() {
        let record = PipelineRecord {
            id: "pipeline::test".to_string(),
            name: "Test".to_string(),
            doc_id: "doc::test".to_string(),
            source_systems: String::new(),
            dest_tables: String::new(),
            scheduler_type: "unknown".to_string(),
            github_repo: String::new(),
            daci: String::new(),
            business_logic_summary: String::new(),
            ..Default::default()
        };

        let missing = needs_llm_fallback(&record);
        assert_eq!(
            missing.len(),
            6,
            "All 6 fields should be flagged: {:?}",
            missing
        );
    }

    #[test]
    fn test_fill_with_llm_gated_by_env_var() {
        std::env::remove_var("INFIGRAPH_LLM_EXTRACT");

        let content = minimal_pipeline_content();
        let mut record = parse_pipeline_template(content, "Mystery ETL", "doc::mystery").unwrap();

        let missing_before = needs_llm_fallback(&record);
        assert!(!missing_before.is_empty(), "Should have missing fields");

        let filled = fill_with_llm(&mut record, content, "Mystery ETL");
        assert_eq!(
            filled, 0,
            "Should return 0 when INFIGRAPH_LLM_EXTRACT not set"
        );

        let missing_after = needs_llm_fallback(&record);
        assert_eq!(
            missing_before, missing_after,
            "Fields should be unchanged when env var not set"
        );
    }

    #[test]
    fn test_fill_with_llm_no_op_when_all_fields_present() {
        std::env::set_var("INFIGRAPH_LLM_EXTRACT", "1");

        let content = full_pipeline_content();
        let mut record = parse_pipeline_template(content, "W2 Metrics", "doc::w2").unwrap();

        let filled = fill_with_llm(&mut record, content, "W2 Metrics");
        assert_eq!(filled, 0, "Should return 0 when no fields are missing");

        std::env::remove_var("INFIGRAPH_LLM_EXTRACT");
    }

    #[test]
    fn test_dependency_table_extraction() {
        let dep_text = r#"### Upstream
| Dependency | Owner | SLA |
|---|---|---|
| tax_src.raw_w2_data | Tax Ingestion | 1am UTC |
| ref_data.employer_dim | MDM | hourly |
### Downstream
| Dependency | Owner | SLA |
|---|---|---|
| tax_rpt.executive_dashboard | BI Team | 6am UTC |
"#;
        let (up, down) = extract_dependencies(dep_text);
        assert!(
            up.contains("tax_src.raw_w2_data"),
            "upstream should contain tax_src.raw_w2_data, got: {}",
            up
        );
        assert!(
            up.contains("ref_data.employer_dim"),
            "upstream should contain ref_data.employer_dim, got: {}",
            up
        );
        assert!(
            down.contains("tax_rpt.executive_dashboard"),
            "downstream should contain tax_rpt.executive_dashboard, got: {}",
            down
        );
    }

    #[test]
    fn test_dependency_bullet_extraction() {
        let dep_text = r#"### Upstream
- system_alpha
- system_beta
### Downstream
- consumer_one
"#;
        let (up, down) = extract_dependencies(dep_text);
        assert!(up.contains("system_alpha"));
        assert!(up.contains("system_beta"));
        assert!(down.contains("consumer_one"));
    }

    #[test]
    fn test_build_extraction_prompt_includes_only_missing() {
        let missing = vec!["source_systems", "github_repo"];
        let prompt = build_extraction_prompt("doc content here", "Test Pipeline", &missing);
        assert!(prompt.contains("source_systems"));
        assert!(prompt.contains("github_repo"));
        assert!(
            !prompt.contains("scheduler_type"),
            "Should not include non-missing fields"
        );
        assert!(
            !prompt.contains("daci"),
            "Should not include non-missing fields"
        );
    }

    #[test]
    fn test_no_sections_returns_none() {
        let content = "Just some plain text with no headings at all.";
        let result = parse_pipeline_template(content, "Empty", "doc::empty");
        assert!(
            result.is_none(),
            "Should return None when no sections found"
        );
    }
}
