use infigraph_confluence::{
    parse_pipeline_template, ConfluenceClient, ConfluenceSync, CrawlOptions,
};

// ==================== CrawlOptions ====================

#[test]
fn test_crawl_options_default_follow() {
    let opts = CrawlOptions::default_follow();
    assert!(opts.follow_links);
    assert_eq!(opts.follow_depth, 1);
    assert_eq!(opts.max_pages, 100);
    assert!(opts.same_space_only);
}

#[test]
fn test_crawl_options_no_follow() {
    let opts = CrawlOptions::no_follow();
    assert!(!opts.follow_links);
    assert_eq!(opts.follow_depth, 0);
    assert_eq!(opts.max_pages, 0);
    assert!(opts.same_space_only);
}

// ==================== ConfluenceClient construction ====================

#[test]
fn test_client_new_trims_url() {
    let client = ConfluenceClient::new("https://wiki.example.com/", "fake-token");
    assert_eq!(client.base_url(), "https://wiki.example.com");
}

#[test]
fn test_client_new_basic() {
    let client =
        ConfluenceClient::new_basic("https://wiki.example.com", "user@test.com", "token123");
    assert_eq!(client.base_url(), "https://wiki.example.com");
}

// ==================== ConfluenceSync construction ====================

#[test]
fn test_sync_new_builds_source_id() {
    let client = ConfluenceClient::new("https://wiki.example.com", "token");
    let _sync = ConfluenceSync::new(client, "MYSPACE");
    // ConfluenceSync fields are private but construction should succeed
}

// ==================== parse_pipeline_template (integration via re-export) ====================

#[test]
fn test_parse_pipeline_full_via_reexport() {
    let content = r#"# Pipeline: Sales ETL

## Overview
Daily sales metrics pipeline.

## Source System Details
Source table: `sales_src.raw_transactions`

## Destination Tables
### sales_dm.fact_daily_sales
Daily aggregated sales metrics.

## Compliance
CCPA applicable. No PII stored.

## Scheduler
Airflow DAG runs at 3am UTC.

## DACI
**Driver:** Alice
**Approver:** Bob

## Business Logic
Aggregates daily transaction amounts by store, product category, and region.

## Github Repo
https://github.intuit.com/data-eng/sales-etl

## Data Quality
Null check on transaction_id, amount > 0 validation.

## Dependencies
### Upstream
- sales_src.raw_transactions
### Downstream
- sales_rpt.executive_dashboard
"#;

    let record = parse_pipeline_template(content, "Sales ETL", "doc::sales").unwrap();

    assert_eq!(record.name, "Sales ETL");
    assert_eq!(record.doc_id, "doc::sales");
    assert!(record.id.starts_with("pipeline::"));
    assert!(
        record.source_systems.contains("sales_src.raw_transactions"),
        "source: {}",
        record.source_systems
    );
    assert!(
        record.dest_tables.contains("sales_dm.fact_daily_sales"),
        "dest: {}",
        record.dest_tables
    );
    assert_eq!(record.scheduler_type, "Airflow");
    assert!(record.github_repo.contains("github.intuit.com"));
    assert!(record.daci.contains("Alice"));
    assert!(record.daci.contains("Bob"));
    assert!(!record.business_logic_summary.is_empty());
    assert!(!record.compliance.is_empty());
    assert!(!record.data_quality.is_empty());
}

#[test]
fn test_parse_pipeline_returns_none_for_nontemplate() {
    let content = "This is just a regular page with no headings matching pipeline sections.";
    assert!(parse_pipeline_template(content, "Random", "doc::random").is_none());
}

#[test]
fn test_parse_pipeline_various_scheduler_types() {
    let bpp_content = "## Scheduler\nBPP batch processing runs daily.";
    let r = parse_pipeline_template(bpp_content, "BPP", "doc::bpp").unwrap();
    assert_eq!(r.scheduler_type, "BPP");

    let cron_content = "## Scheduler\ncron job runs every hour.";
    let r = parse_pipeline_template(cron_content, "Cron", "doc::cron").unwrap();
    assert_eq!(r.scheduler_type, "Cron");

    let unknown_content = "## Scheduler\nManual trigger only.";
    let r = parse_pipeline_template(unknown_content, "Manual", "doc::manual").unwrap();
    assert_eq!(r.scheduler_type, "unknown");
}

#[test]
fn test_parse_pipeline_dependency_extraction() {
    let content = r#"## Dependencies
### Upstream
| Dependency | Owner | SLA |
|---|---|---|
| db_src.orders | Orders Team | 2am |
| db_src.products | Catalog Team | 1am |
### Downstream
| Dependency | Owner | SLA |
|---|---|---|
| analytics.daily_report | BI Team | 8am |
"#;
    let r = parse_pipeline_template(content, "Deps", "doc::deps").unwrap();
    assert!(
        r.dependencies_upstream.contains("db_src.orders"),
        "upstream: {}",
        r.dependencies_upstream
    );
    assert!(
        r.dependencies_upstream.contains("db_src.products"),
        "upstream: {}",
        r.dependencies_upstream
    );
    assert!(
        r.dependencies_downstream.contains("analytics.daily_report"),
        "downstream: {}",
        r.dependencies_downstream
    );
}
