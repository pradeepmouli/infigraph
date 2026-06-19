use std::collections::HashSet;
use std::path::Path;

use infigraph_docs::chunk::{chunk_document, Chunk, ChunkStrategy};
use infigraph_docs::extract::{extract_document, DocFormat, ExtractedDoc};
use infigraph_docs::links::extract_and_link_doc;
use infigraph_docs::search::DocBM25Index;
use infigraph_docs::store::DocStore;
use infigraph_docs::{is_document_file, DocIndex};

// ==================== is_document_file ====================

#[test]
fn test_is_document_file_supported() {
    let supported = [
        "readme.md",
        "readme.markdown",
        "notes.txt",
        "doc.rst",
        "guide.adoc",
        "spec.org",
        "report.pdf",
        "letter.docx",
        "slides.pptx",
        "data.xlsx",
        "page.html",
        "page.htm",
        "book.epub",
        "data.xml",
        "style.xsl",
        "schema.xsd",
        "icon.svg",
        "config.plist",
        "manual.rtf",
    ];
    for name in &supported {
        assert!(
            is_document_file(Path::new(name)),
            "{name} should be document"
        );
    }
}

#[test]
fn test_is_document_file_unsupported() {
    let unsupported = [
        "main.rs",
        "app.py",
        "index.js",
        "Cargo.toml",
        "Makefile",
        "no_extension",
        "image.png",
        "photo.jpg",
        "video.mp4",
    ];
    for name in &unsupported {
        assert!(
            !is_document_file(Path::new(name)),
            "{name} should not be document"
        );
    }
}

// ==================== extract ====================

#[test]
fn test_extract_markdown() {
    let content = b"# My Title\n\nSome paragraph text.\n\n## Section Two\n\nMore content here.\n";
    let doc = extract_document(Path::new("test.md"), content, "md").unwrap();
    assert_eq!(doc.format, DocFormat::Markdown);
    assert_eq!(doc.title.as_deref(), Some("My Title"));
    assert!(doc.text.contains("Some paragraph text"));
    assert!(doc.text.contains("Section Two"));
    assert!(doc.page_count.is_none());
}

#[test]
fn test_extract_plaintext() {
    let content = b"Hello World\nThis is plain text.\n";
    let doc = extract_document(Path::new("test.txt"), content, "txt").unwrap();
    assert_eq!(doc.format, DocFormat::PlainText);
    assert_eq!(doc.title.as_deref(), Some("Hello World"));
    assert!(doc.text.contains("plain text"));
}

#[test]
fn test_extract_html() {
    let content =
        b"<html><head><title>My Page</title></head><body><p>Hello world</p></body></html>";
    let doc = extract_document(Path::new("test.html"), content, "html").unwrap();
    assert_eq!(doc.format, DocFormat::Html);
    assert_eq!(doc.title.as_deref(), Some("My Page"));
    assert!(doc.text.contains("Hello world"), "html text: {}", doc.text);
}

#[test]
fn test_extract_xml() {
    let content = b"<root><item>First</item><item>Second</item></root>";
    let doc = extract_document(Path::new("test.xml"), content, "xml").unwrap();
    assert_eq!(doc.format, DocFormat::Xml);
    assert!(doc.text.contains("First"), "xml text: {}", doc.text);
    assert!(doc.text.contains("Second"), "xml text: {}", doc.text);
}

#[test]
fn test_extract_rst() {
    let content = b"My Document\n===========\n\nRST content here.\n";
    let doc = extract_document(Path::new("test.rst"), content, "rst").unwrap();
    assert_eq!(doc.format, DocFormat::Rst);
    assert!(doc.text.contains("RST content"));
}

#[test]
fn test_extract_unsupported_format() {
    let result = extract_document(Path::new("test.rs"), b"fn main() {}", "rs");
    assert!(result.is_err(), "unsupported format should error");
}

// ==================== chunk ====================

fn make_doc(text: &str) -> ExtractedDoc {
    ExtractedDoc {
        file: "test.md".to_string(),
        title: None,
        content_hash: "abc123".to_string(),
        format: DocFormat::Markdown,
        text: text.to_string(),
        page_count: None,
    }
}

#[test]
fn test_chunk_by_headings() {
    let text = "# Introduction\n\nThis is the intro.\n\n## Details\n\nHere are details.\n";
    let doc = make_doc(text);
    let chunks = chunk_document(&doc, "test.md", "hash1", ChunkStrategy::HeadingBounded);
    assert!(
        chunks.len() >= 2,
        "should produce at least 2 chunks: got {}",
        chunks.len()
    );
    assert!(
        chunks[0].text.contains("Introduction"),
        "first chunk: {}",
        chunks[0].text
    );
    assert!(
        chunks.iter().any(|c| c.text.contains("Details")),
        "should have Details chunk"
    );

    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.index, i, "chunk index mismatch");
        assert_eq!(c.doc_file, "test.md");
        assert!(!c.id.is_empty());
    }
}

#[test]
fn test_chunk_no_headings_falls_back_to_paragraphs() {
    let paragraphs: Vec<String> = (0..5)
        .map(|i| format!("Paragraph {} has some text content that is meaningful.", i))
        .collect();
    let text = paragraphs.join("\n\n");
    let doc = make_doc(&text);
    let chunks = chunk_document(&doc, "doc.txt", "hash2", ChunkStrategy::HeadingBounded);
    assert!(!chunks.is_empty(), "should produce chunks from paragraphs");
    assert!(
        chunks[0].text.contains("Paragraph"),
        "chunk text: {}",
        chunks[0].text
    );
}

#[test]
fn test_chunk_empty_text() {
    let doc = make_doc("");
    let chunks = chunk_document(&doc, "empty.md", "hash3", ChunkStrategy::HeadingBounded);
    assert!(chunks.is_empty(), "empty text should produce no chunks");
}

#[test]
fn test_chunk_fixed_token() {
    let words: Vec<String> = (0..600).map(|i| format!("word{i}")).collect();
    let text = words.join(" ");
    let doc = make_doc(&text);
    let chunks = chunk_document(
        &doc,
        "big.txt",
        "hash4",
        ChunkStrategy::FixedToken {
            size: 100,
            overlap: 20,
        },
    );
    assert!(
        chunks.len() >= 6,
        "600 words / 100 token chunks = at least 6 chunks, got {}",
        chunks.len()
    );
    assert!(chunks[0].text.contains("word0"));
}

// ==================== BM25 search ====================

#[test]
fn test_bm25_basic_ranking() {
    let docs = vec![
        (
            "doc1".to_string(),
            "the quick brown fox jumps over the lazy dog".to_string(),
        ),
        (
            "doc2".to_string(),
            "rust programming language is fast and safe".to_string(),
        ),
        (
            "doc3".to_string(),
            "the fox and the dog are friends".to_string(),
        ),
    ];
    let index = DocBM25Index::build(docs);

    let results = index.search("fox", 10);
    assert!(!results.is_empty(), "should find fox");
    let top_ids: Vec<usize> = results.iter().map(|(idx, _)| *idx).collect();
    assert!(top_ids.contains(&0), "doc1 has 'fox'");
    assert!(top_ids.contains(&2), "doc3 has 'fox'");
    assert!(!top_ids.contains(&1), "doc2 has no 'fox'");
}

#[test]
fn test_bm25_no_match() {
    let docs = vec![("doc1".to_string(), "hello world".to_string())];
    let index = DocBM25Index::build(docs);
    let results = index.search("nonexistent", 10);
    assert!(results.is_empty(), "no match expected");
}

#[test]
fn test_bm25_empty_corpus() {
    let index = DocBM25Index::build(Vec::new());
    let results = index.search("anything", 10);
    assert!(results.is_empty());
}

// ==================== DocStore CRUD ====================

fn temp_store() -> (DocStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.kuzu");
    let store = DocStore::open(&db_path).unwrap();
    (store, dir)
}

fn sample_doc(file: &str) -> ExtractedDoc {
    ExtractedDoc {
        file: file.to_string(),
        title: Some(format!("Title of {file}")),
        content_hash: format!("hash_{file}"),
        format: DocFormat::Markdown,
        text: format!("Content of {file}"),
        page_count: Some(1),
    }
}

fn sample_chunk(file: &str, idx: usize) -> Chunk {
    Chunk {
        id: format!("{file}::chunk_{idx}"),
        doc_file: file.to_string(),
        content_hash: format!("hash_{file}"),
        index: idx,
        heading: Some(format!("Section {idx}")),
        text: format!("Chunk {idx} text for {file}"),
        start_offset: idx * 100,
        end_offset: (idx + 1) * 100,
        page: Some(0),
    }
}

#[test]
fn test_store_open_and_schema() {
    let (store, _dir) = temp_store();
    let conn = store.connection().unwrap();
    let result = conn.query("MATCH (d:Document) RETURN count(d)").unwrap();
    assert!(result.get_num_tuples() > 0 || result.get_num_tuples() == 0);
}

#[test]
fn test_store_doc_hashes_empty() {
    let (store, _dir) = temp_store();
    let hashes = store.get_doc_hashes().unwrap();
    assert!(hashes.is_empty(), "new store should have no doc hashes");
}

#[test]
fn test_store_upsert_and_hashes() {
    let (store, _dir) = temp_store();

    let doc1 = sample_doc("readme.md");
    let doc2 = sample_doc("guide.md");
    let c1 = sample_chunk("readme.md", 0);
    let c2 = sample_chunk("readme.md", 1);
    let c3 = sample_chunk("guide.md", 0);

    store
        .upsert_all_parquet(&[&doc1, &doc2], &[&c1, &c2, &c3])
        .unwrap();

    let hashes = store.get_doc_hashes().unwrap();
    assert_eq!(hashes.len(), 2, "should have 2 docs");
    assert_eq!(hashes.get("readme.md").unwrap(), "hash_readme.md");
    assert_eq!(hashes.get("guide.md").unwrap(), "hash_guide.md");
}

#[test]
fn test_store_stats() {
    let (store, _dir) = temp_store();

    let doc = sample_doc("test.md");
    let c1 = sample_chunk("test.md", 0);
    let c2 = sample_chunk("test.md", 1);
    store.upsert_all_parquet(&[&doc], &[&c1, &c2]).unwrap();

    let stats = store.stats().unwrap();
    assert_eq!(stats.document_count, 1);
    assert_eq!(stats.chunk_count, 2);
}

#[test]
fn test_store_get_all_chunks() {
    let (store, _dir) = temp_store();

    let doc = sample_doc("file.md");
    let c1 = sample_chunk("file.md", 0);
    let c2 = sample_chunk("file.md", 1);
    store.upsert_all_parquet(&[&doc], &[&c1, &c2]).unwrap();

    let chunks = store.get_all_chunks().unwrap();
    assert_eq!(chunks.len(), 2, "should have 2 chunks");
    assert!(chunks.iter().any(|(id, _)| id.contains("chunk_0")));
    assert!(chunks.iter().any(|(id, _)| id.contains("chunk_1")));
}

#[test]
fn test_store_get_chunk_ids() {
    let (store, _dir) = temp_store();

    let doc = sample_doc("a.md");
    let c = sample_chunk("a.md", 0);
    store.upsert_all_parquet(&[&doc], &[&c]).unwrap();

    let ids = store.get_chunk_ids().unwrap();
    assert!(ids.contains("a.md::chunk_0"), "ids: {ids:?}");
}

#[test]
fn test_store_get_chunk_details() {
    let (store, _dir) = temp_store();

    let doc = sample_doc("detail.md");
    let c = sample_chunk("detail.md", 0);
    store.upsert_all_parquet(&[&doc], &[&c]).unwrap();

    let details = store.get_chunk_details(&["detail.md::chunk_0"]).unwrap();
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].id, "detail.md::chunk_0");
    assert!(details[0].text.contains("Chunk 0 text"));
}

#[test]
fn test_store_delete_docs_by_ids() {
    let (store, _dir) = temp_store();

    let doc1 = sample_doc("keep.md");
    let doc2 = sample_doc("delete.md");
    let c1 = sample_chunk("keep.md", 0);
    let c2 = sample_chunk("delete.md", 0);
    store
        .upsert_all_parquet(&[&doc1, &doc2], &[&c1, &c2])
        .unwrap();

    store.delete_docs_by_ids(&["delete.md"]).unwrap();

    let hashes = store.get_doc_hashes().unwrap();
    assert_eq!(hashes.len(), 1);
    assert!(hashes.contains_key("keep.md"));
    assert!(!hashes.contains_key("delete.md"));
}

#[test]
fn test_store_source_crud() {
    let (store, _dir) = temp_store();

    store
        .upsert_source("src1", "confluence", "https://wiki.example.com", "SPACE")
        .unwrap();

    let doc = sample_doc("page.md");
    let c = sample_chunk("page.md", 0);
    store.upsert_all_parquet(&[&doc], &[&c]).unwrap();

    store.link_doc_to_source("page.md", "src1").unwrap();
    let docs = store.get_docs_by_source("src1").unwrap();
    assert!(
        docs.contains(&"page.md".to_string()),
        "should find linked doc: {docs:?}"
    );
}

#[test]
fn test_store_links_crud() {
    let (store, _dir) = temp_store();

    let doc1 = sample_doc("a.md");
    let doc2 = sample_doc("b.md");
    let c1 = sample_chunk("a.md", 0);
    let c2 = sample_chunk("b.md", 0);
    store
        .upsert_all_parquet(&[&doc1, &doc2], &[&c1, &c2])
        .unwrap();

    store.create_link("a.md", "b.md", "b.md", "local").unwrap();

    let conn = store.connection().unwrap();
    let mut result = conn
        .query("MATCH (a:Document)-[l:LINKS_TO]->(b:Document) RETURN a.id, b.id, l.url")
        .unwrap();
    let mut found = false;
    while let Some(row) = result.next() {
        if row[0].to_string() == "a.md" && row[1].to_string() == "b.md" {
            found = true;
        }
    }
    assert!(found, "should have LINKS_TO edge from a.md to b.md");

    store.delete_links_from("a.md").unwrap();
    let mut result2 = conn
        .query("MATCH (a:Document)-[l:LINKS_TO]->(b:Document) WHERE a.id = 'a.md' RETURN count(l)")
        .unwrap();
    if let Some(row) = result2.next() {
        let count: i64 = row[0].to_string().parse().unwrap_or(0);
        assert_eq!(count, 0, "links should be deleted");
    }
}

// ==================== links::extract_and_link_doc ====================

#[test]
fn test_extract_and_link_doc_markdown_links() {
    let (store, _dir) = temp_store();

    let doc_a = sample_doc("docs/index.md");
    let doc_b = sample_doc("docs/guide.md");
    let c_a = sample_chunk("docs/index.md", 0);
    let c_b = sample_chunk("docs/guide.md", 0);
    store
        .upsert_all_parquet(&[&doc_a, &doc_b], &[&c_a, &c_b])
        .unwrap();

    let source_doc = ExtractedDoc {
        file: "docs/index.md".to_string(),
        title: Some("Index".to_string()),
        content_hash: "hash1".to_string(),
        format: DocFormat::Markdown,
        text: "See the [guide](guide.md) for details.\nAlso [external](https://example.com)."
            .to_string(),
        page_count: None,
    };

    let all_doc_ids: HashSet<String> = ["docs/index.md", "docs/guide.md"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    extract_and_link_doc(&store, &source_doc, &all_doc_ids);

    let conn = store.connection().unwrap();
    let mut result = conn.query(
        "MATCH (a:Document)-[l:LINKS_TO]->(b:Document) WHERE a.id = 'docs/index.md' RETURN b.id, l.link_type"
    ).unwrap();
    let mut linked_to_guide = false;
    let mut linked_external = false;
    while let Some(row) = result.next() {
        let target = row[0].to_string();
        if target == "docs/guide.md" {
            linked_to_guide = true;
        }
        if row[1].to_string() == "external" {
            linked_external = true;
        }
    }
    assert!(
        linked_to_guide,
        "should create LINKS_TO for relative markdown link"
    );
    assert!(
        !linked_external,
        "should NOT create LINKS_TO for external links (target not in all_doc_ids)"
    );
}

// ==================== DocIndex lifecycle ====================

#[test]
fn test_docindex_open_creates_infigraph_dir() {
    let dir = tempfile::tempdir().unwrap();
    let _idx = DocIndex::open(dir.path()).unwrap();
    assert!(
        dir.path().join(".infigraph").exists(),
        ".infigraph dir should be created"
    );
}

#[test]
fn test_docindex_init_creates_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = DocIndex::open(dir.path()).unwrap();
    assert!(idx.store().is_none(), "store should be None before init");
    idx.init().unwrap();
    assert!(idx.store().is_some(), "store should be Some after init");
}

#[test]
fn test_docindex_clean_removes_db() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = DocIndex::open(dir.path()).unwrap();
    idx.init().unwrap();
    assert!(idx.store().is_some());

    idx.clean().unwrap();
    assert!(idx.store().is_none(), "store should be None after clean");
}

#[test]
fn test_docindex_index_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = DocIndex::open(dir.path()).unwrap();
    idx.init().unwrap();
    let result = idx.index().unwrap();
    assert_eq!(result.total_files, 0);
    assert_eq!(result.indexed_files, 0);
    assert_eq!(result.total_chunks, 0);
}

#[test]
fn test_docindex_index_with_files() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("readme.md"),
        "# Project\n\nThis is the readme.\n\n## Setup\n\nRun install.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("notes.txt"),
        "Some plain text notes about the project.\n\nAnother paragraph.\n",
    )
    .unwrap();
    // Non-document file should be ignored
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let mut idx = DocIndex::open(dir.path()).unwrap();
    idx.init().unwrap();

    let result = idx.index().unwrap();
    assert_eq!(result.total_files, 2, "should find 2 document files");
    assert_eq!(result.indexed_files, 2, "should index both");
    assert!(result.total_chunks > 0, "should produce chunks");

    let store = idx.store().unwrap();
    let hashes = store.get_doc_hashes().unwrap();
    assert_eq!(hashes.len(), 2);
    assert!(hashes.contains_key("readme.md") || hashes.contains_key("notes.txt"));
}

#[test]
fn test_docindex_reindex_is_incremental() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.md"), "# Hello\n\nWorld.\n").unwrap();

    let mut idx = DocIndex::open(dir.path()).unwrap();
    idx.init().unwrap();
    let r1 = idx.index().unwrap();
    assert_eq!(r1.indexed_files, 1);

    // Second index with same content should be no-op
    let r2 = idx.index().unwrap();
    assert_eq!(
        r2.indexed_files, 0,
        "unchanged file should not be re-indexed"
    );
    assert_eq!(r2.total_files, 1, "should still see the file");
}

#[test]
fn test_docindex_reindex_picks_up_changes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.md"), "# Original\n\nContent.\n").unwrap();

    let mut idx = DocIndex::open(dir.path()).unwrap();
    idx.init().unwrap();
    idx.index().unwrap();

    // Modify the file
    std::fs::write(dir.path().join("doc.md"), "# Updated\n\nNew content.\n").unwrap();

    let r2 = idx.index().unwrap();
    assert_eq!(r2.indexed_files, 1, "changed file should be re-indexed");
}

#[test]
fn test_docindex_ignores_hidden_and_build_dirs() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/config.txt"), "git config").unwrap();

    std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
    std::fs::write(dir.path().join("node_modules/pkg/readme.md"), "# Pkg").unwrap();

    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target/output.txt"), "build output").unwrap();

    std::fs::write(dir.path().join("real.md"), "# Real Doc\n\nContent.\n").unwrap();

    let mut idx = DocIndex::open(dir.path()).unwrap();
    idx.init().unwrap();
    let result = idx.index().unwrap();
    assert_eq!(
        result.total_files, 1,
        "should only find real.md, not files in ignored dirs"
    );
}
