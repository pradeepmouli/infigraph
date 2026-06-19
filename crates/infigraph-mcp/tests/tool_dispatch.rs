use std::path::Path;
use std::sync::OnceLock;

use serde_json::json;

use infigraph_mcp::tools::graph::*;
use infigraph_mcp::tools::helpers::log_activity;
use infigraph_mcp::tools::index::tool_index_project;
use infigraph_mcp::tools::search::tool_search;

static PROJECT: OnceLock<SharedProject> = OnceLock::new();

struct SharedProject {
    _dir: tempfile::TempDir,
    path: String,
}

unsafe impl Sync for SharedProject {}

fn shared_project() -> &'static SharedProject {
    PROJECT.get_or_init(|| {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let files: &[(&str, &str)] = &[
            (
                "src/main.py",
                "\
from src.lib import process

def main():
    result = process(\"hello\")
    return result

def unused_helper():
    pass

if __name__ == \"__main__\":
    main()
",
            ),
            (
                "src/lib.py",
                "\
def process(data):
    validated = validate(data)
    return transform(validated)

def validate(data):
    if not data:
        raise ValueError(\"empty\")
    return data.strip()

def transform(data):
    return data.upper()
",
            ),
            (
                "src/models.py",
                "\
class BaseModel:
    def save(self):
        pass

    def delete(self):
        pass

class UserModel(BaseModel):
    def __init__(self, name):
        self.name = name

    def greet(self):
        return f\"Hello {self.name}\"
",
            ),
            (
                "tests/test_lib.py",
                "\
from src.lib import process

def test_process():
    assert process(\"hello\") == \"HELLO\"

def test_validate():
    from src.lib import validate
    assert validate(\"  hi  \") == \"hi\"
",
            ),
        ];
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        let path = dir.path().to_string_lossy().to_string();
        let args = json!({"path": &path});
        tool_index_project(&args).expect("index should succeed");
        SharedProject { _dir: dir, path }
    })
}

fn a(extra: serde_json::Value) -> serde_json::Value {
    let proj = shared_project();
    let mut map = extra.as_object().cloned().unwrap_or_default();
    map.insert("path".into(), json!(&proj.path));
    serde_json::Value::Object(map)
}

// Kuzu doesn't support concurrent connections to the same DB.
// All graph tool tests run sequentially in one test to avoid mmap errors.
#[test]
fn test_graph_tools() {
    // --- get_stats ---
    let result = tool_get_stats(&a(json!({}))).unwrap();
    assert!(
        result.contains("Symbol"),
        "get_stats: missing Symbol count: {result}"
    );
    assert!(
        result.contains("alls") || result.contains("edges"),
        "get_stats: missing edge info: {result}"
    );

    // --- get_symbols_in_file ---
    let result = tool_get_symbols_in_file(&a(json!({"file": "src/lib.py"}))).unwrap();
    assert!(
        result.contains("process"),
        "symbols_in_file: missing process: {result}"
    );
    assert!(
        result.contains("validate"),
        "symbols_in_file: missing validate: {result}"
    );
    assert!(
        result.contains("transform"),
        "symbols_in_file: missing transform: {result}"
    );

    // --- get_graph_schema ---
    let result = tool_get_graph_schema(&a(json!({}))).unwrap();
    assert!(
        result.contains("Symbol"),
        "schema: missing Symbol: {result}"
    );
    assert!(result.contains("CALLS"), "schema: missing CALLS: {result}");

    // --- get_code_snippet (existing) ---
    let result = tool_get_code_snippet(&a(json!({"symbol_id": "src/lib.py::process"}))).unwrap();
    assert!(
        result.contains("process"),
        "snippet: missing function name: {result}"
    );

    // --- get_code_snippet (missing) ---
    let result = tool_get_code_snippet(&a(json!({"symbol_id": "nonexistent::foo"})));
    assert!(result.is_err(), "snippet: missing symbol should error");

    // --- symbol_context ---
    let result = tool_symbol_context(&a(json!({"symbol_id": "src/lib.py::process"}))).unwrap();
    assert!(
        result.contains("process"),
        "symbol_context: missing name: {result}"
    );

    // --- get_complexity ---
    let result = tool_get_complexity(&a(json!({}))).unwrap();
    assert!(
        result.to_lowercase().contains("complexity"),
        "complexity: missing info: {result}"
    );

    // --- find_all_references ---
    let result =
        tool_find_all_references(&a(json!({"symbol_id": "src/lib.py::validate"}))).unwrap();
    assert!(
        result.contains("src/lib.py"),
        "refs: missing defining file: {result}"
    );

    // --- get_api_surface ---
    let result = tool_get_api_surface(&a(json!({}))).unwrap();
    assert!(!result.is_empty(), "api_surface: empty response: {result}");

    // --- get_file_deps ---
    let result = tool_get_file_deps(&a(json!({"file": "src/main.py"}))).unwrap();
    assert!(
        result.contains("src/main.py") || result.contains("dependencies"),
        "file_deps: missing file name: {result}"
    );

    // --- get_type_hierarchy ---
    let result =
        tool_get_type_hierarchy(&a(json!({"symbol_id": "src/models.py::UserModel"}))).unwrap();
    assert!(
        result.contains("BaseModel") || result.contains("UserModel"),
        "type_hierarchy: missing class: {result}"
    );

    // --- get_test_coverage ---
    let result = tool_get_test_coverage(&a(json!({}))).unwrap();
    let lower = result.to_lowercase();
    assert!(
        lower.contains("coverage") || lower.contains("%"),
        "test_coverage: missing info: {result}"
    );

    // --- generate_test_context ---
    let result = tool_generate_test_context(&a(json!({}))).unwrap();
    assert!(
        result.contains("Test Generation Context"),
        "test_ctx: missing header: {result}"
    );
    assert!(
        result.contains("Framework") || result.contains("framework"),
        "test_ctx: missing framework: {result}"
    );

    // --- generate_test_context with file filter ---
    let result = tool_generate_test_context(&a(json!({"file": "src/models.py"}))).unwrap();
    assert!(
        result.contains("Test Generation Context"),
        "test_ctx(file): missing header: {result}"
    );

    // --- generate_test_context with limit ---
    let result = tool_generate_test_context(&a(json!({"limit": 2}))).unwrap();
    assert!(
        result.contains("Test Generation Context"),
        "test_ctx(limit): missing header: {result}"
    );

    // --- list_files ---
    let result = tool_list_files(&a(json!({}))).unwrap();
    assert!(
        result.contains("main.py"),
        "list_files: missing main.py: {result}"
    );
    assert!(
        result.contains("lib.py"),
        "list_files: missing lib.py: {result}"
    );

    // --- list_files with glob ---
    let result = tool_list_files(&a(json!({"glob": "**/*.py"}))).unwrap();
    assert!(
        result.contains(".py"),
        "list_files(glob): missing .py: {result}"
    );

    // --- query_graph ---
    let result = tool_query_graph(&a(json!({
        "cypher": "MATCH (s:Symbol) WHERE s.kind = 'Function' RETURN s.name LIMIT 5"
    })))
    .unwrap();
    assert!(!result.is_empty(), "query_graph: empty result");

    // --- get_doc_context ---
    let result = tool_get_doc_context(&a(json!({"symbol_id": "src/lib.py::process"}))).unwrap();
    assert!(
        result.contains("process"),
        "doc_context: missing name: {result}"
    );

    // --- search by name ---
    let result = tool_search(&a(json!({"query": "process"}))).unwrap();
    assert!(
        result.contains("process"),
        "search(name): missing: {result}"
    );

    // --- search by kind ---
    let result = tool_search(&a(json!({"query": "model", "kind": "Class"}))).unwrap();
    assert!(
        result.contains("Model") || result.contains("Class"),
        "search(kind): missing: {result}"
    );

    // --- list_projects ---
    let proj = shared_project();
    let result = tool_list_projects(&json!({})).unwrap();
    assert!(
        result.contains(&proj.path) || result.contains("project"),
        "list_projects: missing: {result}"
    );

    // --- log_activity ---
    let args = a(json!({"query": "test"}));
    log_activity("search", &args);
    log_activity("search", &args);
    log_activity("get_latest_session", &args);

    // --- error cases ---
    let result = tool_get_stats(&json!({}));
    assert!(result.is_err(), "missing path should error");

    let result = tool_symbol_context(&a(json!({})));
    assert!(result.is_err(), "missing symbol_id should error");

    // --- list_languages (no DB needed) ---
    let result = tool_list_languages(&json!({})).unwrap();
    assert!(result.contains("python"), "should list python");
    assert!(result.contains("rust"), "should list rust");
    assert!(result.contains("typescript"), "should list typescript");

    // --- delete_project (uses separate project, must run last) ---
    let del_dir = tempfile::TempDir::new().expect("tmpdir");
    let dp = del_dir.path().join("hello.py");
    std::fs::write(&dp, "def hello(): pass").unwrap();
    let del_path = del_dir.path().to_string_lossy().to_string();
    tool_index_project(&json!({"path": &del_path})).expect("index");

    let result = tool_delete_project(&json!({"path": &del_path})).unwrap();
    assert!(
        result.contains("Deleted") || result.contains("deleted") || result.contains("Removed"),
        "should confirm deletion: {result}"
    );
    let dot_tg = Path::new(&del_path).join(".infigraph");
    assert!(
        !dot_tg.join("graph").exists(),
        ".infigraph/graph should be removed"
    );
}
