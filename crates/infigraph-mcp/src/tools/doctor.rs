use serde_json::Value;

use infigraph_core::doctor::{
    assemble_context, format_report, projects_in_scope, run_doctor, DoctorScope,
};

pub fn tool_doctor(args: &Value) -> anyhow::Result<String> {
    let global = args.get("scope").and_then(|v| v.as_str()) == Some("global");

    let scope = if global {
        DoctorScope::Global
    } else {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(super::helpers::startup_project);
        let canonical = path.canonicalize().unwrap_or(path);
        DoctorScope::Project(canonical)
    };

    let ctx = assemble_context(scope);
    let projects = projects_in_scope(&ctx);
    let mut report = run_doctor(ctx);
    // Document links: keys are always checked. The git-based sync check takes
    // minutes on a large project, longer than a tool call should, so it is
    // `infigraph doctor --docs-sync` only; the report says so.
    for project in &projects {
        report
            .checks
            .extend(infigraph_docs::link_check::check_project(project, false));
    }
    // Never colorize: this text goes back to an LLM as tool-call output, not a terminal.
    Ok(format_report(
        &report,
        infigraph_core::doctor::ReportStyle::Plain,
    ))
}
