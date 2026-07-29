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

/// Regression test: rust/relations.scm had a comment describing intent to
/// capture `impl Trait for Type` as an INHERITS relationship, but no actual
/// query pattern was ever written -- every trait impl in every Rust codebase
/// silently produced zero INHERITS edges (confirmed against infigraph's own
/// `impl GraphBackend for KuzuBackend`, which had no corresponding edge).
#[test]
fn test_extraction_rust_impl_trait_produces_inherits_edge() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".rs").unwrap();

    let source = b"trait Greet {\n    fn hello(&self);\n}\nstruct Person;\nimpl Greet for Person {\n    fn hello(&self) {}\n}\n";
    let extraction = infigraph_core::extract::extract_file("test.rs", source, pack)
        .expect("extraction should succeed");

    assert!(
        extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Person")
                && r.target_id.contains("Greet")),
        "expected an INHERITS edge from Person to Greet, got: {:?}",
        extraction.relations
    );
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

/// Regression test: typescript/relations.scm had no inheritance capture at
/// all (only calls + imports), unlike python/relations.scm and
/// javascript/relations.scm which both have working @inherit.child/
/// @inherit.parent patterns -- every `class X extends Y`, `interface X
/// extends Y`, and `class X implements Y` in every TypeScript codebase
/// silently produced zero INHERITS edges (confirmed against a real repo's
/// `InputProps extends React.ComponentProps<'input'>`, which had no
/// corresponding edge).
#[test]
fn test_extraction_typescript_inheritance_produces_edges() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();

    let source = b"class Animal {}\nclass Dog extends Animal {}\n\ninterface Shape {}\ninterface Circle extends Shape {}\n\ninterface Drawable {}\nclass Square implements Drawable {}\n";
    let extraction = infigraph_core::extract::extract_file("test.ts", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.contains(parent)
        })
    };

    assert!(
        has_edge("Dog", "Animal"),
        "class extends: {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Circle", "Shape"),
        "interface extends: {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Square", "Drawable"),
        "class implements: {:?}",
        extraction.relations
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

/// Regression test: TypeScript inheritance clauses whose base type is a generic
/// (`Shape<T>`) or qualified/dotted name (`ns.Bar`) or member expression
/// (`React.Component`) previously resolved to the WRONG identifier once the
/// query was wildcard-ified without a decomposition step (e.g. literal text
/// "Shape<T>" instead of "Shape") -- confirmed this produces the actual base
/// name, not the whole compound expression.
#[test]
fn test_extraction_typescript_inheritance_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();

    let source = b"class Shape<T> {}\ninterface Circle extends Shape<number> {}\n\nnamespace ns { export class Bar {} }\ninterface Foo extends ns.Bar {}\n\nclass ReactComponentLike { Component() {} }\nclass MyComponent extends ReactComponentLike.Component {}\n";
    let extraction = infigraph_core::extract::extract_file("test.ts", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.ends_with(&format!("::{parent}"))
        })
    };

    assert!(
        has_edge("Circle", "Shape"),
        "generic interface extends should resolve to base name \"Shape\", not \"Shape<number>\": {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Foo", "Bar"),
        "qualified interface extends should resolve to base name \"Bar\", not \"ns.Bar\": {:?}",
        extraction.relations
    );
}

/// Regression test: Rust `impl Trait for Type` where Trait is a generic
/// (`Iterator<Item = T>`) or fully-qualified path (`std::fmt::Display`)
/// previously resolved to the wrong identifier once wildcard-ified without a
/// decomposition step. Confirms both resolve to their real base trait name.
#[test]
fn test_extraction_rust_impl_trait_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".rs").unwrap();

    let source = b"struct MyIter;\nimpl Iterator for MyIter {\n    type Item = u32;\n    fn next(&mut self) -> Option<u32> { None }\n}\n\nstruct MyType;\nimpl std::fmt::Display for MyType {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}\n";
    let extraction = infigraph_core::extract::extract_file("test.rs", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.ends_with(&format!("::{parent}"))
        })
    };

    assert!(
        has_edge("MyIter", "Iterator"),
        "impl for a plain trait should resolve to \"Iterator\": {:?}",
        extraction.relations
    );
    assert!(
        has_edge("MyType", "Display"),
        "impl for a fully-qualified trait path should resolve to \"Display\", not \"std::fmt::Display\": {:?}",
        extraction.relations
    );
}

/// Regression test: Python superclasses that are dotted/qualified names
/// (`pkg.Animal`) or subscripted generics (`Generic[T]`) previously produced
/// zero INHERITS edges under the narrow `(identifier)` pattern, and a spurious
/// wrong edge would result from a naive unconstrained wildcard (matching
/// `metaclass=Meta` as if it were a base class). Confirms both real cases
/// resolve correctly and the keyword argument is correctly excluded.
#[test]
fn test_extraction_python_inheritance_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".py").unwrap();

    let source = b"import pkg\nfrom typing import Generic, TypeVar\nT = TypeVar('T')\n\nclass Dog(pkg.Animal):\n    pass\n\nclass Container(Generic[T]):\n    pass\n\nclass Cat(pkg.Animal, metaclass=type):\n    pass\n";
    let extraction = infigraph_core::extract::extract_file("test.py", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.ends_with(&format!("::{parent}"))
        })
    };

    assert!(
        has_edge("Dog", "Animal"),
        "dotted superclass should resolve to \"Animal\", not \"pkg.Animal\": {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Container", "Generic"),
        "subscripted generic superclass should resolve to \"Generic\": {:?}",
        extraction.relations
    );
    assert!(
        !extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Cat")
                && r.target_id.contains("Meta")),
        "metaclass=Meta must NOT produce a spurious INHERITS edge: {:?}",
        extraction.relations
    );
}

/// Regression test: Java superclasses/interfaces that are generic (`Bar<T>`),
/// qualified (`pkg.Bar`), or both combined (`pkg.Bar<T>`) previously produced
/// zero INHERITS edges under the narrow `(type_identifier)` pattern. Confirms
/// all three resolve to their real base name, including the doubly-compound
/// case which requires the decomposition query to recurse.
#[test]
fn test_extraction_java_inheritance_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".java").unwrap();

    let source = b"package test;\nclass Bar<T> {}\nclass Foo extends Bar<String> {}\n\nclass Baz {}\n\nclass Qux extends pkg2.Baz {}\n";
    let extraction = infigraph_core::extract::extract_file("Test.java", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.ends_with(&format!("::{parent}"))
        })
    };

    assert!(
        has_edge("Foo", "Bar"),
        "generic superclass should resolve to \"Bar\", not \"Bar<String>\": {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Qux", "Baz"),
        "qualified superclass should resolve to \"Baz\", not \"pkg2.Baz\": {:?}",
        extraction.relations
    );
}

/// Regression test: Dart extends/implements clauses whose base type is generic
/// (`Animal<T>`) or qualified (`pkg.Animal`) previously resolved to the wrong
/// identifier under the narrow `(type_identifier)`-only pattern. Confirms both
/// resolve correctly via the single fully-anchored pattern (no decomposition
/// query needed for Dart), and that multiple `implements` interfaces each
/// produce their own correct edge.
#[test]
fn test_extraction_dart_inheritance_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".dart").unwrap();

    let source = b"class Animal<T> {}\nclass Dog extends Animal<int> {}\n\nclass Walker {}\nclass Runner {}\nclass Athlete implements Walker, Runner {}\n";
    let extraction = infigraph_core::extract::extract_file("test.dart", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.ends_with(&format!("::{parent}"))
        })
    };

    assert!(
        has_edge("Dog", "Animal"),
        "generic extends should resolve to \"Animal\", not \"Animal<int>\": {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Athlete", "Walker") && has_edge("Athlete", "Runner"),
        "multiple implements interfaces should each produce their own edge: {:?}",
        extraction.relations
    );
}

/// Regression test: dart/relations.scm had no inheritance capture at all.
#[test]
fn test_extraction_dart_inheritance_produces_edges() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".dart").unwrap();

    let source = b"class Animal {}\nclass Dog extends Animal {}\n\nclass Drawable {}\nclass Square implements Drawable {}\n";
    let extraction = infigraph_core::extract::extract_file("test.dart", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.contains(parent)
        })
    };

    assert!(
        has_edge("Dog", "Animal"),
        "class extends: {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Square", "Drawable"),
        "class implements: {:?}",
        extraction.relations
    );
}

/// Regression test: Go embedded fields that are generic (`Base[T]`) or
/// package-qualified (`pkg.Animal`) previously resolved to the wrong identifier
/// once the query was wildcard-ified without a decomposition step. Confirms
/// both resolve to their real base type name, and that a named (non-embedded)
/// field is correctly excluded (not treated as inheritance).
#[test]
fn test_extraction_go_struct_embedding_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".go").unwrap();

    let source = b"package main\n\ntype Base[T any] struct {\n\tValue T\n}\ntype Container struct {\n\tBase[int]\n}\n\ntype Animal struct{}\ntype Dog struct {\n\tAnimal\n\tName string\n}\n";
    let extraction = infigraph_core::extract::extract_file("test.go", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.ends_with(&format!("::{parent}"))
        })
    };

    assert!(
        has_edge("Container", "Base"),
        "generic embedded field should resolve to \"Base\", not \"Base[int]\": {:?}",
        extraction.relations
    );
    assert!(
        !extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Dog")
                && r.target_id.contains("Name")),
        "named field \"Name\" must NOT produce a spurious INHERITS edge: {:?}",
        extraction.relations
    );
}

/// Regression test: Go has no `extends`/`implements` keywords, but struct
/// embedding (an anonymous field with no name, just a type) is its closest
/// analog to inheritance and wasn't captured at all. Interface satisfaction
/// in Go is implicit/structural and can't be determined from syntax alone,
/// so it's intentionally not covered here.
#[test]
fn test_extraction_go_struct_embedding_produces_inherits_edge() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".go").unwrap();

    let source = b"package main\ntype Animal struct {\n\tName string\n}\ntype Dog struct {\n\tAnimal\n\tBreed string\n}\n";
    let extraction = infigraph_core::extract::extract_file("test.go", source, pack)
        .expect("extraction should succeed");

    assert!(
        extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Dog")
                && r.target_id.contains("Animal")),
        "expected an INHERITS edge from Dog to Animal (embedded field), got: {:?}",
        extraction.relations
    );
}

/// Regression test: Kotlin superclasses/interfaces that are generic
/// (`Comparable<Dog>`) or qualified (`pkg.Animal`) previously resolved to the
/// wrong identifier once wildcard-ified without a decomposition step (Kotlin's
/// grammar declares no fields on `user_type` at all, so this needed a
/// kind+anchor-based query rather than field-based).
#[test]
fn test_extraction_kotlin_inheritance_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".kt").unwrap();

    let source = b"class Dog : Comparable<Dog> {}\n";
    let extraction = infigraph_core::extract::extract_file("Test.kt", source, pack)
        .expect("extraction should succeed");

    assert!(
        extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Dog")
                && r.target_id.ends_with("::Comparable")),
        "generic superclass should resolve to \"Comparable\", not \"Comparable<Dog>\": {:?}",
        extraction.relations
    );
}

/// Regression test: kotlin/relations.scm had no inheritance capture at all.
#[test]
fn test_extraction_kotlin_inheritance_produces_edges() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".kt").unwrap();

    let source =
        b"open class Animal\nclass Dog : Animal()\n\ninterface Shape\nclass Circle : Shape\n";
    let extraction = infigraph_core::extract::extract_file("Test.kt", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.contains(parent)
        })
    };

    assert!(
        has_edge("Dog", "Animal"),
        "class inheritance: {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Circle", "Shape"),
        "interface implementation: {:?}",
        extraction.relations
    );
}

/// Regression test: objc/relations.scm had no inheritance capture, AND
/// objc/entities.scm's class_interface/class_implementation/
/// protocol_declaration patterns used a `name:` field that doesn't exist on
/// those grammar nodes (verified against tree-sitter-objc's node-types.json
/// -- the class name is an unlabeled positional child, not a field), so
/// every Objective-C class/protocol produced zero symbols at all, not just
/// zero inheritance edges.
#[test]
fn test_extraction_objc_produces_symbols_and_inherits_edge() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".m").unwrap();

    let source = b"@interface Animal\n@end\n@interface Dog : Animal\n@end\n";
    let extraction = infigraph_core::extract::extract_file("Test.m", source, pack)
        .expect("extraction should succeed");

    let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"Animal"),
        "should extract Animal: {names:?}"
    );
    assert!(names.contains(&"Dog"), "should extract Dog: {names:?}");

    assert!(
        extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Dog")
                && r.target_id.contains("Animal")),
        "expected an INHERITS edge from Dog to Animal, got: {:?}",
        extraction.relations
    );
}

/// Regression test: Swift superclasses/protocol conformances that are generic
/// (`Comparable<Dog>`) previously resolved to the wrong identifier once
/// wildcard-ified without a decomposition step (Swift's `user_type` is
/// structurally identical to Kotlin's -- no fields, kind+anchor-based).
#[test]
fn test_extraction_swift_inheritance_compound_bases_resolve_correctly() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".swift").unwrap();

    let source = b"class Dog: Comparable<Dog> {}\n";
    let extraction = infigraph_core::extract::extract_file("Test.swift", source, pack)
        .expect("extraction should succeed");

    assert!(
        extraction
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Inherits
                && r.source_id.contains("Dog")
                && r.target_id.ends_with("::Comparable")),
        "generic superclass should resolve to \"Comparable\", not \"Comparable<Dog>\": {:?}",
        extraction.relations
    );
}

/// Regression test: swift/relations.scm had no inheritance capture at all.
#[test]
fn test_extraction_swift_inheritance_produces_edges() {
    use infigraph_core::model::RelationKind;

    let registry = bundled_registry().unwrap();
    let pack = registry.for_extension(".swift").unwrap();

    let source =
        b"class Animal {}\nclass Dog: Animal {}\n\nprotocol Shape {}\nprotocol Circle: Shape {}\n";
    let extraction = infigraph_core::extract::extract_file("Test.swift", source, pack)
        .expect("extraction should succeed");

    let has_edge = |child: &str, parent: &str| {
        extraction.relations.iter().any(|r| {
            r.kind == RelationKind::Inherits
                && r.source_id.contains(child)
                && r.target_id.contains(parent)
        })
    };

    assert!(
        has_edge("Dog", "Animal"),
        "class inheritance: {:?}",
        extraction.relations
    );
    assert!(
        has_edge("Circle", "Shape"),
        "protocol inheritance: {:?}",
        extraction.relations
    );
}
