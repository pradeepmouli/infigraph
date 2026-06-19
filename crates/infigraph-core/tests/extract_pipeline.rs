use infigraph_core::extract::extract_file;
use infigraph_core::lang::LanguagePack;
use infigraph_core::model::{RelationKind, SymbolKind};

const PYTHON_ENTITIES: &str = r#"
(module
  (function_definition
    name: (identifier) @func.name
    body: (block
      (expression_statement
        (string) @func.docstring)?)) @func.def)

(module
  (decorated_definition
    (decorator) @func.decorator
    definition: (function_definition
      name: (identifier) @func.name
      body: (block
        (expression_statement
          (string) @func.docstring)?)) @func.def))

(class_definition
  name: (identifier) @class.name
  body: (block
    (expression_statement
      (string) @class.docstring)?)) @class.def

(class_definition
  body: (block
    (function_definition
      name: (identifier) @method.name
      body: (block
        (expression_statement
          (string) @method.docstring)?)) @method.def))

(class_definition
  body: (block
    (decorated_definition
      (decorator) @method.decorator
      definition: (function_definition
        name: (identifier) @method.name
        body: (block
          (expression_statement
            (string) @method.docstring)?)) @method.def)))

(module
  (function_definition
    name: (identifier) @test.name
    (#match? @test.name "^test_")
    body: (block
      (expression_statement
        (string) @test.docstring)?)) @test.def)

(module
  (expression_statement
    (assignment
      left: (identifier) @var.name)) @var.def)
"#;

const PYTHON_RELATIONS: &str = r#"
(call
  function: (identifier) @call.func) @call.site

(call
  function: (attribute
    object: (_) @call.receiver
    attribute: (identifier) @call.func)) @call.site

(import_statement
  name: (dotted_name) @import.module)

(import_from_statement
  module_name: (dotted_name) @import.module)

(class_definition
  name: (identifier) @inherit.child
  superclasses: (argument_list
    (identifier) @inherit.parent))
"#;

fn python_pack() -> LanguagePack {
    let grammar = tree_sitter_python::LANGUAGE.into();
    LanguagePack::new(
        "python",
        vec![".py"],
        grammar,
        PYTHON_ENTITIES,
        PYTHON_RELATIONS,
    )
    .unwrap()
}

// ---------- extract_file end-to-end ----------

#[test]
fn test_extract_simple_function() {
    let src = b"def hello(name: str) -> str:\n    \"\"\"Greet someone.\"\"\"\n    return f'hello {name}'\n";
    let pack = python_pack();
    let ext = extract_file("hello.py", src, &pack).unwrap();

    assert_eq!(ext.file, "hello.py");
    assert_eq!(ext.language, "python");
    assert!(!ext.content_hash.is_empty());
    assert_eq!(ext.symbols.len(), 1);
    assert_eq!(ext.symbols[0].name, "hello");
    assert_eq!(ext.symbols[0].kind, SymbolKind::Function);
    assert!(ext.symbols[0].parameters.is_some());
    assert!(ext.symbols[0]
        .parameters
        .as_deref()
        .unwrap()
        .contains("name"));
    assert!(ext.symbols[0].return_type.is_some());
}

#[test]
fn test_extract_class_with_methods() {
    let src = b"class Animal:\n    \"\"\"Base animal.\"\"\"\n    def speak(self):\n        pass\n    def eat(self, food):\n        pass\n";
    let pack = python_pack();
    let ext = extract_file("animal.py", src, &pack).unwrap();

    let class = ext.symbols.iter().find(|s| s.kind == SymbolKind::Class);
    assert!(class.is_some());
    assert_eq!(class.unwrap().name, "Animal");

    let methods: Vec<&str> = ext
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Method)
        .map(|s| s.name.as_str())
        .collect();
    assert!(methods.contains(&"speak"));
    assert!(methods.contains(&"eat"));
}

#[test]
fn test_extract_test_functions() {
    let src = b"def test_addition():\n    assert 1 + 1 == 2\n\ndef test_subtraction():\n    assert 2 - 1 == 1\n\ndef helper():\n    return 42\n";
    let pack = python_pack();
    let ext = extract_file("test_math.py", src, &pack).unwrap();

    let tests: Vec<&str> = ext
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Test)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(tests.len(), 2);
    assert!(tests.contains(&"test_addition"));
    assert!(tests.contains(&"test_subtraction"));

    let funcs: Vec<&str> = ext
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Function)
        .map(|s| s.name.as_str())
        .collect();
    assert!(funcs.contains(&"helper"));
}

#[test]
fn test_extract_call_relations() {
    let src = b"def main():\n    helper()\n    obj.method()\n\ndef helper():\n    pass\n";
    let pack = python_pack();
    let ext = extract_file("calls.py", src, &pack).unwrap();

    let calls: Vec<&str> = ext
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls)
        .map(|r| r.target_id.as_str())
        .collect();
    assert!(
        calls.iter().any(|t| t.contains("helper")),
        "should detect helper() call"
    );
    assert!(
        calls.iter().any(|t| t.contains("method")),
        "should detect obj.method() call"
    );
}

#[test]
fn test_extract_import_relations() {
    let src = b"import os\nfrom pathlib import Path\n\ndef work():\n    pass\n";
    let pack = python_pack();
    let ext = extract_file("imports.py", src, &pack).unwrap();

    let imports: Vec<&str> = ext
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Imports)
        .map(|r| r.target_id.as_str())
        .collect();
    assert!(
        imports.iter().any(|t| t.contains("os")),
        "should detect import os"
    );
    assert!(
        imports.iter().any(|t| t.contains("pathlib")),
        "should detect from pathlib import"
    );
}

#[test]
fn test_extract_inheritance() {
    let src = b"class Base:\n    pass\n\nclass Child(Base):\n    pass\n";
    let pack = python_pack();
    let ext = extract_file("inherit.py", src, &pack).unwrap();

    let inherits: Vec<_> = ext
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Inherits)
        .collect();
    assert_eq!(inherits.len(), 1);
    assert!(inherits[0].source_id.contains("Child"));
    assert!(inherits[0].target_id.contains("Base"));
}

#[test]
fn test_extract_statements() {
    let src = b"def process(x):\n    if x > 0:\n        for i in range(x):\n            print(i)\n    else:\n        pass\n";
    let pack = python_pack();
    let ext = extract_file("stmts.py", src, &pack).unwrap();

    assert!(!ext.statements.is_empty(), "should extract statements");
    let kinds: Vec<&str> = ext.statements.iter().map(|s| s.kind.as_str()).collect();
    assert!(kinds.contains(&"If"), "expected If statement");
    assert!(kinds.contains(&"For"), "expected For statement");
    assert!(kinds.contains(&"Else"), "expected Else statement");
}

#[test]
fn test_extract_complexity() {
    let src = b"def complex_func(x, y):\n    if x > 0:\n        if y > 0:\n            return x + y\n        else:\n            return x\n    elif x < 0:\n        return y\n    else:\n        return 0\n";
    let pack = python_pack();
    let ext = extract_file("complex.py", src, &pack).unwrap();

    let func = ext
        .symbols
        .iter()
        .find(|s| s.name == "complex_func")
        .unwrap();
    assert!(
        func.complexity > 1,
        "complex function should have complexity > 1, got {}",
        func.complexity
    );
}

#[test]
fn test_extract_module_level_variables() {
    let src = b"MAX_SIZE = 100\nDEBUG = True\n\ndef work():\n    pass\n";
    let pack = python_pack();
    let ext = extract_file("config.py", src, &pack).unwrap();

    let vars: Vec<&str> = ext
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Variable)
        .map(|s| s.name.as_str())
        .collect();
    assert!(vars.contains(&"MAX_SIZE"));
    assert!(vars.contains(&"DEBUG"));
}

#[test]
fn test_extract_content_hash_deterministic() {
    let src = b"def foo():\n    pass\n";
    let pack = python_pack();
    let ext1 = extract_file("foo.py", src, &pack).unwrap();
    let ext2 = extract_file("foo.py", src, &pack).unwrap();
    assert_eq!(ext1.content_hash, ext2.content_hash);
}

#[test]
fn test_extract_content_hash_changes() {
    let pack = python_pack();
    let ext1 = extract_file("f.py", b"def a(): pass", &pack).unwrap();
    let ext2 = extract_file("f.py", b"def b(): pass", &pack).unwrap();
    assert_ne!(ext1.content_hash, ext2.content_hash);
}

#[test]
fn test_extract_symbol_ids_contain_file() {
    let src = b"def my_func():\n    pass\n\nclass MyClass:\n    def method(self):\n        pass\n";
    let pack = python_pack();
    let ext = extract_file("src/module.py", src, &pack).unwrap();

    for sym in &ext.symbols {
        assert!(
            sym.id.contains("src/module.py"),
            "symbol id should contain file path: {}",
            sym.id
        );
    }
}

#[test]
fn test_extract_span_line_numbers() {
    let src = b"def first():\n    pass\n\ndef second():\n    pass\n";
    let pack = python_pack();
    let ext = extract_file("lines.py", src, &pack).unwrap();

    let first = ext.symbols.iter().find(|s| s.name == "first").unwrap();
    assert_eq!(first.span.start_line, 1);

    let second = ext.symbols.iter().find(|s| s.name == "second").unwrap();
    assert!(second.span.start_line > first.span.start_line);
}

#[test]
fn test_extract_empty_file() {
    let pack = python_pack();
    let ext = extract_file("empty.py", b"", &pack).unwrap();
    assert!(ext.symbols.is_empty());
    assert!(ext.relations.is_empty());
    assert!(ext.statements.is_empty());
}

#[test]
fn test_extract_docstrings() {
    let src = b"class Greeter:\n    \"\"\"A friendly greeter.\"\"\"\n    def greet(self):\n        \"\"\"Say hello.\"\"\"\n        pass\n";
    let pack = python_pack();
    let ext = extract_file("doc.py", src, &pack).unwrap();

    let class = ext
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Class)
        .unwrap();
    assert!(class
        .docstring
        .as_deref()
        .unwrap_or("")
        .contains("friendly greeter"));

    let method = ext
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Method)
        .unwrap();
    assert!(method
        .docstring
        .as_deref()
        .unwrap_or("")
        .contains("Say hello"));
}

#[test]
fn test_extract_nested_class_method_not_top_level() {
    let src = b"class Outer:\n    class Inner:\n        def inner_method(self):\n            pass\n    def outer_method(self):\n        pass\n";
    let pack = python_pack();
    let ext = extract_file("nested.py", src, &pack).unwrap();

    let methods: Vec<&str> = ext
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Method)
        .map(|s| s.name.as_str())
        .collect();
    assert!(methods.contains(&"outer_method"));
    assert!(methods.contains(&"inner_method"));
}

#[test]
fn test_extract_multiple_inheritance() {
    let src = b"class A:\n    pass\n\nclass B:\n    pass\n\nclass C(A, B):\n    pass\n";
    let pack = python_pack();
    let ext = extract_file("multi.py", src, &pack).unwrap();

    let inherits: Vec<_> = ext
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Inherits)
        .collect();
    assert_eq!(inherits.len(), 2, "C inherits from both A and B");
}

#[test]
fn test_extract_receiver_on_method_call() {
    let src = b"def work():\n    self.save()\n    db.query('SELECT 1')\n";
    let pack = python_pack();
    let ext = extract_file("recv.py", src, &pack).unwrap();

    let calls_with_receiver: Vec<_> = ext
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls && r.receiver.is_some())
        .collect();
    assert!(
        !calls_with_receiver.is_empty(),
        "method calls should have receiver"
    );
}

// ---------- Full pipeline: extract → graph → query ----------

#[test]
fn test_extract_to_graph_roundtrip() {
    use infigraph_core::graph::{GraphQuery, GraphStore};

    let src = b"class Service:\n    def handle(self, request):\n        result = self.validate(request)\n        return result\n\n    def validate(self, data):\n        if not data:\n            raise ValueError('empty')\n        return True\n\ndef test_handle():\n    svc = Service()\n    svc.handle({})\n";
    let pack = python_pack();
    let ext = extract_file("service.py", src, &pack).unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    let store = GraphStore::open(&dir.path().join("graph")).unwrap();
    store.upsert_file(&ext).unwrap();

    let conn = store.connection().unwrap();
    let q = GraphQuery::new(&conn);

    let syms = q.symbols_in_file("service.py").unwrap();
    assert!(
        syms.len() >= 3,
        "expected Service, handle, validate, test_handle; got {}",
        syms.len()
    );

    let branches = q
        .branches_of(&syms.iter().find(|s| s.name == "validate").unwrap().id)
        .unwrap();
    assert!(
        !branches.is_empty(),
        "validate should have branches (if statement)"
    );
}
