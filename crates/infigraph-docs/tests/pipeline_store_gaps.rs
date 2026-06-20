use infigraph_docs::chunk::ChunkStrategy;
use infigraph_docs::extract::DocFormat;
use infigraph_docs::DocIndex;

// ==================== ChunkStrategy::for_extension ====================

#[test]
fn test_chunk_strategy_for_markdown() {
    match ChunkStrategy::for_extension("md") {
        ChunkStrategy::HeadingBounded => {}
        other => panic!("expected HeadingBounded for .md, got {:?}", other),
    }
}

#[test]
fn test_chunk_strategy_for_html() {
    match ChunkStrategy::for_extension("html") {
        ChunkStrategy::HeadingBounded => {}
        other => panic!("expected HeadingBounded for .html, got {:?}", other),
    }
}

#[test]
fn test_chunk_strategy_for_unknown() {
    match ChunkStrategy::for_extension("xyz") {
        ChunkStrategy::HeadingBounded => {}
        other => panic!("expected HeadingBounded for unknown ext, got {:?}", other),
    }
}

#[test]
fn test_chunk_strategy_for_xml_variants() {
    for ext in &["xml", "xsl", "xsd", "svg", "plist"] {
        match ChunkStrategy::for_extension(ext) {
            ChunkStrategy::HeadingBounded => {}
            other => panic!("expected HeadingBounded for .{ext}, got {:?}", other),
        }
    }
}

// ==================== DocFormat::as_str ====================

#[test]
fn test_doc_format_as_str_all() {
    assert_eq!(DocFormat::Markdown.as_str(), "markdown");
    assert_eq!(DocFormat::PlainText.as_str(), "text");
    assert_eq!(DocFormat::Rst.as_str(), "rst");
    assert_eq!(DocFormat::Asciidoc.as_str(), "asciidoc");
    assert_eq!(DocFormat::Org.as_str(), "org");
    assert_eq!(DocFormat::Pdf.as_str(), "pdf");
    assert_eq!(DocFormat::Docx.as_str(), "docx");
    assert_eq!(DocFormat::Pptx.as_str(), "pptx");
    assert_eq!(DocFormat::Xlsx.as_str(), "xlsx");
    assert_eq!(DocFormat::Html.as_str(), "html");
    assert_eq!(DocFormat::Rtf.as_str(), "rtf");
    assert_eq!(DocFormat::Xml.as_str(), "xml");
}

// ==================== DocIndex::root ====================

#[test]
fn test_doc_index_root() {
    let tmp = tempfile::tempdir().unwrap();
    let idx = DocIndex::open(tmp.path()).unwrap();
    let root = idx
        .root()
        .canonicalize()
        .unwrap_or_else(|_| idx.root().to_path_buf());
    let expected = tmp.path().canonicalize().unwrap();
    assert_eq!(
        root, expected,
        "root {:?} should match {:?}",
        root, expected
    );
}
