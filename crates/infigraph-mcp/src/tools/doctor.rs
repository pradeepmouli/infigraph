use serde_json::Value;

use infigraph_core::doctor::{assemble_context, format_report, run_doctor, DoctorScope};

pub fn tool_doctor(args: &Value) -> anyhow::Result<String> {
    let global = args.get("scope").and_then(|v| v.as_str()) == Some("global");

    let scope = if global {
        DoctorScope::Global
    } else {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        let canonical = path.canonicalize().unwrap_or(path);
        DoctorScope::Project(canonical)
    };

    let ctx = assemble_context(scope);
    let report = run_doctor(ctx);
    Ok(format_report(&report))
}
