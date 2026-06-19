use infigraph_languages::bundled_registry;

#[test]
fn test_registry_loads_all_languages() {
    let registry = bundled_registry().expect("bundled_registry should succeed");
    let count = registry.languages().count();
    // We have 55+ tree-sitter languages (may vary with ANTLR feature)
    assert!(count >= 50, "expected 50+ languages, got {count}");
}

#[test]
fn test_registry_extension_lookup() {
    let registry = bundled_registry().unwrap();

    let cases = vec![
        (".py", "python"),
        (".rs", "rust"),
        (".ts", "typescript"),
        (".js", "javascript"),
        (".go", "go"),
        (".java", "java"),
        (".c", "c"),
        (".cpp", "cpp"),
        (".rb", "ruby"),
        (".php", "php"),
        (".swift", "swift"),
        (".kt", "kotlin"),
        (".cs", "csharp"),
        (".scala", "scala"),
        (".lua", "lua"),
        (".zig", "zig"),
        (".ex", "elixir"),
        (".dart", "dart"),
        (".hs", "haskell"),
        (".pl", "perl"),
        (".r", "r"),
        (".sh", "bash"),
        (".sql", "sql"),
        (".jl", "julia"),
        (".proto", "proto"),
        (".ps1", "powershell"),
        (".hcl", "hcl"),
        (".toml", "toml"),
        (".yaml", "yaml"),
        (".erl", "erlang"),
        (".nix", "nix"),
        (".svelte", "svelte"),
        (".fs", "fsharp"),
        (".groovy", "groovy"),
        (".css", "css"),
        (".html", "html"),
        (".json", "json"),
        (".xml", "xml"),
        (".graphql", "graphql"),
        (".bas", "vb6"),
        (".cls", "vb6"),
        (".tsx", "tsx"),
    ];

    let mut failures = Vec::new();
    for (ext, expected_name) in &cases {
        match registry.for_extension(ext) {
            Some(pack) => {
                if pack.name != *expected_name {
                    failures.push(format!(
                        "{ext}: expected '{expected_name}', got '{}'",
                        pack.name
                    ));
                }
            }
            None => failures.push(format!("{ext}: not found in registry")),
        }
    }
    if !failures.is_empty() {
        panic!("Extension lookup failures:\n{}", failures.join("\n"));
    }
}

#[test]
fn test_registry_file_path_lookup() {
    let registry = bundled_registry().unwrap();

    assert_eq!(registry.for_file("src/main.py").unwrap().name, "python");
    assert_eq!(registry.for_file("lib/foo.rs").unwrap().name, "rust");
    assert_eq!(registry.for_file("app/index.tsx").unwrap().name, "tsx");
    assert_eq!(registry.for_file("Makefile.mk").unwrap().name, "makefile");
    assert_eq!(registry.for_file("no_extension").map(|p| &p.name), None);
}

#[test]
fn test_registry_content_probe_fallback() {
    let registry = bundled_registry().unwrap();

    // for_file_with_content should fall back to extension when no probe matches
    let py_content = b"def hello(): pass";
    let pack = registry.for_file_with_content("test.py", py_content);
    assert_eq!(pack.unwrap().name, "python");

    // Unknown extension should return None
    let pack = registry.for_file_with_content("file.xyz", b"some content");
    assert!(pack.is_none());
}

#[test]
fn test_extraction_smoke_python() {
    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".py").unwrap();

    let source = b"def greet(name):\n    return f'Hello {name}'\n\nclass Foo:\n    def bar(self):\n        greet('world')\n";
    let extraction = infigraph_core::extract::extract_file("test.py", source, pack)
        .expect("extraction should succeed");

    let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"greet"), "should extract greet: {names:?}");
    assert!(names.contains(&"Foo"), "should extract Foo: {names:?}");
    assert!(names.contains(&"bar"), "should extract bar: {names:?}");

    assert!(
        !extraction.relations.is_empty(),
        "should have call relations"
    );
    assert!(
        extraction
            .relations
            .iter()
            .any(|r| r.target_id.contains("greet")),
        "should have call to greet"
    );
}

#[test]
fn test_extraction_smoke_rust() {
    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".rs").unwrap();

    let source = b"pub fn add(a: i32, b: i32) -> i32 { a + b }\nfn main() { let x = add(1, 2); }\n";
    let extraction = infigraph_core::extract::extract_file("test.rs", source, pack)
        .expect("extraction should succeed");

    let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"add"), "should extract add: {names:?}");
    assert!(names.contains(&"main"), "should extract main: {names:?}");
}

#[test]
fn test_extraction_smoke_typescript() {
    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();

    let source = b"export function fetchData(url: string): Promise<any> { return fetch(url); }\nexport class ApiClient { get() { return fetchData('/api'); } }\n";
    let extraction = infigraph_core::extract::extract_file("api.ts", source, pack)
        .expect("extraction should succeed");

    let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"fetchData"),
        "should extract fetchData: {names:?}"
    );
    assert!(
        names.contains(&"ApiClient"),
        "should extract ApiClient: {names:?}"
    );
}

#[test]
fn test_extraction_smoke_go() {
    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".go").unwrap();

    let source =
        b"package main\nfunc Add(a, b int) int { return a + b }\nfunc main() { Add(1, 2) }\n";
    let extraction = infigraph_core::extract::extract_file("main.go", source, pack)
        .expect("extraction should succeed");

    let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Add"), "should extract Add: {names:?}");
    assert!(names.contains(&"main"), "should extract main: {names:?}");
}

#[test]
fn test_extraction_smoke_java() {
    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".java").unwrap();

    let source = b"public class Calculator {\n    public int add(int a, int b) { return a + b; }\n    public static void main(String[] args) { new Calculator().add(1, 2); }\n}\n";
    let extraction = infigraph_core::extract::extract_file("Calculator.java", source, pack)
        .expect("extraction should succeed");

    let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"Calculator"),
        "should extract Calculator: {names:?}"
    );
    assert!(names.contains(&"add"), "should extract add: {names:?}");
}
