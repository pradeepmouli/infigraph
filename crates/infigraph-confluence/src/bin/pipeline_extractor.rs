use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use infigraph_confluence::template::{parse_pipeline_template, fill_with_llm};

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    writeln!(out, "{}", json!({"ready": true, "plugin_id": "intuit", "version": "1.0"})).unwrap();
    out.flush().unwrap();

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let cmd: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                writeln!(out, "{}", json!({"status": "error", "message": format!("Invalid JSON: {e}")})).unwrap();
                out.flush().unwrap();
                continue;
            }
        };

        let command = cmd.get("command").and_then(|v| v.as_str()).unwrap_or("");

        match command {
            "extract" => {
                let content = cmd.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let title = cmd.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let doc_id = cmd.get("doc_id").and_then(|v| v.as_str()).unwrap_or("");

                match parse_pipeline_template(content, title, doc_id) {
                    Some(mut record) => {
                        fill_with_llm(&mut record, content, title);

                        let inputs = split_csv(&record.source_systems);
                        let outputs = split_csv(&record.dest_tables);

                        let response = json!({
                            "status": "ok",
                            "data": {
                                "core": {
                                    "name": record.name,
                                    "inputs": inputs,
                                    "outputs": outputs,
                                },
                                "properties": {
                                    "source_systems": record.source_systems,
                                    "dest_tables": record.dest_tables,
                                    "scheduler_type": record.scheduler_type,
                                    "scheduler_config": record.scheduler_config,
                                    "compliance": record.compliance,
                                    "github_repo": record.github_repo,
                                    "daci": record.daci,
                                    "idempotent": record.idempotent,
                                    "business_logic_summary": record.business_logic_summary,
                                    "data_quality": record.data_quality,
                                }
                            }
                        });
                        writeln!(out, "{response}").unwrap();
                    }
                    None => {
                        writeln!(out, "{}", json!({"status": "skip", "reason": "No pipeline template sections detected"})).unwrap();
                    }
                }
                out.flush().unwrap();
            }
            "schema" => {
                let response = json!({
                    "status": "ok",
                    "schema": {
                        "columns": [
                            {"name": "source_systems", "col_type": "STRING"},
                            {"name": "dest_tables", "col_type": "STRING"},
                            {"name": "scheduler_type", "col_type": "STRING"},
                            {"name": "scheduler_config", "col_type": "STRING"},
                            {"name": "compliance", "col_type": "STRING"},
                            {"name": "github_repo", "col_type": "STRING"},
                            {"name": "daci", "col_type": "STRING"},
                            {"name": "idempotent", "col_type": "STRING"},
                            {"name": "business_logic_summary", "col_type": "STRING"},
                            {"name": "data_quality", "col_type": "STRING"},
                        ],
                        "dependency_fields": {"inputs": "core.inputs", "outputs": "core.outputs"},
                        "searchable_fields": ["compliance", "business_logic_summary", "daci", "scheduler_type"],
                    }
                });
                writeln!(out, "{response}").unwrap();
                out.flush().unwrap();
            }
            _ => {
                writeln!(out, "{}", json!({"status": "error", "message": format!("Unknown command: {command}")})).unwrap();
                out.flush().unwrap();
            }
        }
    }
}

fn split_csv(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect()
}
