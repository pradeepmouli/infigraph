use std::path::Path;

use anyhow::{Context, Result};
use calamine::Reader;

#[derive(Debug, Clone)]
pub struct ExtractedDoc {
    pub file: String,
    pub title: Option<String>,
    pub content_hash: String,
    pub format: DocFormat,
    pub text: String,
    pub page_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFormat {
    Markdown,
    PlainText,
    Rst,
    Asciidoc,
    Org,
    Pdf,
    Docx,
    Pptx,
    Xlsx,
    Html,
    Rtf,
    Xml,
}

impl DocFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::PlainText => "text",
            Self::Rst => "rst",
            Self::Asciidoc => "asciidoc",
            Self::Org => "org",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
            Self::Html => "html",
            Self::Rtf => "rtf",
            Self::Xml => "xml",
        }
    }
}

pub fn extract_document(path: &Path, bytes: &[u8], ext: &str) -> Result<ExtractedDoc> {
    let format = match ext {
        "md" | "markdown" => DocFormat::Markdown,
        "txt" => DocFormat::PlainText,
        "rst" => DocFormat::Rst,
        "adoc" => DocFormat::Asciidoc,
        "org" => DocFormat::Org,
        "pdf" => DocFormat::Pdf,
        "docx" => DocFormat::Docx,
        "pptx" => DocFormat::Pptx,
        "xlsx" => DocFormat::Xlsx,
        "html" | "htm" => DocFormat::Html,
        "rtf" => DocFormat::Rtf,
        "xml" | "xsl" | "xsd" | "svg" | "plist" => DocFormat::Xml,
        _ => anyhow::bail!("unsupported document format: {ext}"),
    };

    let (text, title, page_count) = match format {
        DocFormat::Markdown
        | DocFormat::PlainText
        | DocFormat::Rst
        | DocFormat::Asciidoc
        | DocFormat::Org => extract_text(bytes)?,
        DocFormat::Pdf => extract_pdf(path, bytes)?,
        DocFormat::Docx => extract_docx(bytes)?,
        DocFormat::Pptx => extract_pptx(bytes)?,
        DocFormat::Xlsx => extract_xlsx(bytes)?,
        DocFormat::Html => extract_html(bytes)?,
        DocFormat::Rtf => extract_rtf(bytes)?,
        DocFormat::Xml => extract_xml(bytes)?,
    };

    Ok(ExtractedDoc {
        file: String::new(),
        title,
        content_hash: String::new(),
        format,
        text,
        page_count,
    })
}

fn extract_text(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let title = text
        .lines()
        .next()
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|t| !t.is_empty());
    Ok((text, title, None))
}

fn extract_pdf(path: &Path, bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    match pdf_oxide::PdfDocument::from_bytes(bytes.to_vec()) {
        Ok(doc) => {
            let page_count = doc.page_count().unwrap_or(0);
            let mut pages_text = Vec::new();
            for i in 0..page_count {
                match doc.extract_text(i) {
                    Ok(text) => pages_text.push(text),
                    Err(e) => {
                        eprintln!(
                            "warning: PDF page {} extraction failed in {}: {e}",
                            i + 1,
                            path.display()
                        );
                    }
                }
            }
            let text = pages_text.join("\n");
            let title = text
                .lines()
                .next()
                .map(|l| l.trim().to_string())
                .filter(|t| !t.is_empty());
            let count = if page_count > 0 {
                Some(page_count)
            } else {
                None
            };
            Ok((text, title, count))
        }
        Err(_) => {
            let text = String::from_utf8_lossy(bytes);
            if text.is_ascii() && text.len() > 10 {
                let title = text
                    .lines()
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|t| !t.is_empty());
                Ok((text.into_owned(), title, None))
            } else {
                anyhow::bail!("PDF extraction failed: {}", path.display())
            }
        }
    }
}

fn extract_docx(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("DOCX is not a valid ZIP archive")?;

    let mut text = String::new();
    let mut title = None;

    if let Ok(mut file) = archive.by_name("word/document.xml") {
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut file, &mut xml)?;
        text = extract_text_from_ooxml(&xml);
        title = text
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|t| !t.is_empty());
    }

    Ok((text, title, None))
}

fn extract_pptx(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("PPTX is not a valid ZIP archive")?;

    let mut all_text = Vec::new();
    let mut slide_names: Vec<String> = Vec::new();

    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file.name().to_string();
        if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
            slide_names.push(name);
        }
    }
    slide_names.sort();

    let page_count = Some(slide_names.len());

    for name in &slide_names {
        if let Ok(mut file) = archive.by_name(name) {
            let mut xml = String::new();
            std::io::Read::read_to_string(&mut file, &mut xml)?;
            let slide_text = extract_text_from_ooxml(&xml);
            if !slide_text.is_empty() {
                all_text.push(slide_text);
            }
        }
    }

    let text = all_text.join("\n\n");
    let title = text
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|t| !t.is_empty());
    Ok((text, title, page_count))
}

fn extract_xlsx(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let cursor = std::io::Cursor::new(bytes);
    let mut workbook =
        calamine::open_workbook_auto_from_rs(cursor).context("Failed to open spreadsheet")?;

    let mut all_text = Vec::new();
    let sheet_names: Vec<String> = workbook.sheet_names().to_vec();
    let page_count = Some(sheet_names.len());

    for name in &sheet_names {
        if let Ok(range) = workbook.worksheet_range(name) {
            let mut sheet_text = format!("Sheet: {}\n", name);
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(|cell| format!("{}", cell)).collect();
                let line = cells.join("\t");
                if !line.trim().is_empty() {
                    sheet_text.push_str(&line);
                    sheet_text.push('\n');
                }
            }
            all_text.push(sheet_text);
        }
    }

    let text = all_text.join("\n");
    let title = sheet_names.first().cloned();
    Ok((text, title, page_count))
}

fn extract_html(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let html = String::from_utf8_lossy(bytes);
    let mut text = String::new();
    let mut in_tag = false;
    let mut title = None;

    // Extract title from <title> tag
    if let Some(start) = html.find("<title>") {
        if let Some(end) = html[start..].find("</title>") {
            title = Some(html[start + 7..start + end].trim().to_string());
        }
    }

    // Strip HTML tags — simple but effective for text extraction
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !text.ends_with('\n') && !text.ends_with(' ') {
                    text.push(' ');
                }
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }

    // Collapse whitespace
    let text = regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(text.trim(), " ")
        .into_owned();

    Ok((text, title, None))
}

fn extract_rtf(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let rtf = String::from_utf8_lossy(bytes);
    let mut text = String::new();
    let mut in_control = false;
    let mut brace_depth = 0i32;

    for ch in rtf.chars() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            '\\' => in_control = true,
            ' ' | '\n' if in_control => {
                in_control = false;
                if brace_depth <= 2 {
                    text.push(' ');
                }
            }
            _ if in_control => {}
            _ if brace_depth <= 2 => text.push(ch),
            _ => {}
        }
    }

    let text = text.trim().to_string();
    let title = text
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|t| !t.is_empty());
    Ok((text, title, None))
}

fn extract_xml(bytes: &[u8]) -> Result<(String, Option<String>, Option<usize>)> {
    let xml_str = String::from_utf8_lossy(bytes);
    let mut text = String::new();
    let mut title = None;
    let mut reader = quick_xml::Reader::from_str(&xml_str);
    let mut buf = Vec::new();
    let mut depth = 0u32;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                depth += 1;
                if depth > 1 && !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                // Use root element name as title if not set
                if depth == 1 && title.is_none() {
                    let local = e.local_name();
                    let name = std::str::from_utf8(local.as_ref()).unwrap_or("");
                    if !name.is_empty() {
                        title = Some(name.to_string());
                    }
                }
            }
            Ok(quick_xml::events::Event::Text(ref e)) => {
                if let Ok(t) = e.unescape() {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        if !text.is_empty() && !text.ends_with('\n') && !text.ends_with(' ') {
                            text.push(' ');
                        }
                        text.push_str(trimmed);
                    }
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                depth = depth.saturating_sub(1);
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    let text = text.trim().to_string();
    Ok((text, title, None))
}

fn extract_text_from_ooxml(xml: &str) -> String {
    let mut text = String::new();
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut in_text = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e))
            | Ok(quick_xml::events::Event::Empty(ref e)) => {
                let local = e.local_name();
                let name = std::str::from_utf8(local.as_ref()).unwrap_or("");
                if name == "t" {
                    in_text = true;
                }
                // Paragraph boundary
                if name == "p" && !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
            }
            Ok(quick_xml::events::Event::Text(ref e)) if in_text => {
                if let Ok(t) = e.unescape() {
                    text.push_str(&t);
                }
            }
            Ok(quick_xml::events::Event::End(ref e)) => {
                let local = e.local_name();
                let name = std::str::from_utf8(local.as_ref()).unwrap_or("");
                if name == "t" {
                    in_text = false;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    text
}
