//! Text extraction for document ingestion.
//!
//! Plain-text formats (`.md`/`.txt`) are always supported. Richer formats
//! (PDF + Office) require the `docs-extract` build feature, which pulls in
//! `pdf-extract` and `office_oxide`. When the feature is off, those formats are
//! still recognised so ingest can report them as skipped with a clear hint
//! rather than silently ignoring the files.

use anyhow::Result;
use std::path::Path;

/// Formats handled without any optional feature.
const PLAIN_EXTS: &[&str] = &["md", "markdown", "txt", "text"];
/// Richer formats handled only when built with `docs-extract`.
const RICH_EXTS: &[&str] = &["pdf", "docx", "xlsx", "pptx", "doc", "xls", "ppt"];

/// Max bytes for Office documents (10 MiB; matches the office WASM plugin cap).
#[cfg(feature = "docs-extract")]
const MAX_OFFICE_BYTES: u64 = 10 * 1024 * 1024;
/// Max bytes for PDFs (50 MB; matches the `pdf_read` tool cap).
#[cfg(feature = "docs-extract")]
const MAX_PDF_BYTES: u64 = 50 * 1024 * 1024;

/// Outcome of attempting extraction on a path.
pub enum Extracted {
    /// Successfully extracted text.
    Text(String),
    /// Recognised document type, but this build lacks `docs-extract`.
    NeedsFeature,
    /// Not a recognised document type.
    Unsupported,
}

/// Lowercased file extension, if any.
fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
}

/// True if the extension is a document type we could extract given the right
/// build. Used to decide which files to collect during a directory walk.
pub fn is_known_doc(path: &Path) -> bool {
    match ext_of(path) {
        Some(ext) => PLAIN_EXTS.contains(&ext.as_str()) || RICH_EXTS.contains(&ext.as_str()),
        None => false,
    }
}

/// Human-readable list of formats this build can extract.
pub fn supported_summary() -> String {
    #[cfg(feature = "docs-extract")]
    {
        let mut all: Vec<&str> = PLAIN_EXTS.to_vec();
        all.extend_from_slice(RICH_EXTS);
        all.join(", ")
    }
    #[cfg(not(feature = "docs-extract"))]
    {
        format!(
            "{} (rebuild with --features docs-extract for: {})",
            PLAIN_EXTS.join(", "),
            RICH_EXTS.join(", ")
        )
    }
}

/// Extract text from a single document.
pub fn extract(path: &Path) -> Result<Extracted> {
    let Some(ext) = ext_of(path) else {
        return Ok(Extracted::Unsupported);
    };
    if PLAIN_EXTS.contains(&ext.as_str()) {
        let text = std::fs::read_to_string(path)?;
        return Ok(Extracted::Text(text));
    }
    match ext.as_str() {
        "pdf" => extract_pdf(path),
        "docx" | "xlsx" | "pptx" | "doc" | "xls" | "ppt" => extract_office(path, &ext),
        _ => Ok(Extracted::Unsupported),
    }
}

#[cfg(feature = "docs-extract")]
fn extract_pdf(path: &Path) -> Result<Extracted> {
    use anyhow::{Context, bail};
    let len = std::fs::metadata(path)?.len();
    if len > MAX_PDF_BYTES {
        bail!(
            "PDF too large: {} bytes (limit {} MiB)",
            len,
            MAX_PDF_BYTES / (1024 * 1024)
        );
    }
    let bytes = std::fs::read(path)?;
    // Image-only or encrypted PDFs yield empty text rather than erroring.
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .with_context(|| format!("extracting PDF {}", path.display()))?;
    Ok(Extracted::Text(text))
}

#[cfg(not(feature = "docs-extract"))]
fn extract_pdf(_path: &Path) -> Result<Extracted> {
    Ok(Extracted::NeedsFeature)
}

#[cfg(feature = "docs-extract")]
fn extract_office(path: &Path, ext: &str) -> Result<Extracted> {
    use anyhow::{anyhow, bail};
    use office_oxide::{Document, DocumentFormat};
    use std::io::Cursor;

    let len = std::fs::metadata(path)?.len();
    if len > MAX_OFFICE_BYTES {
        bail!(
            "document too large: {} bytes (limit {} MiB)",
            len,
            MAX_OFFICE_BYTES / (1024 * 1024)
        );
    }
    let format = match ext {
        "docx" => DocumentFormat::Docx,
        "xlsx" => DocumentFormat::Xlsx,
        "pptx" => DocumentFormat::Pptx,
        "doc" => DocumentFormat::Doc,
        "xls" => DocumentFormat::Xls,
        "ppt" => DocumentFormat::Ppt,
        _ => return Ok(Extracted::Unsupported),
    };
    let bytes = std::fs::read(path)?;
    let doc = Document::from_reader(Cursor::new(bytes), format)
        .map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
    // Markdown output preserves headings/tables so the chunker can split on them.
    Ok(Extracted::Text(doc.to_markdown()))
}

#[cfg(not(feature = "docs-extract"))]
fn extract_office(_path: &Path, _ext: &str) -> Result<Extracted> {
    Ok(Extracted::NeedsFeature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_docs_cover_plain_and_rich() {
        assert!(is_known_doc(Path::new("a/notes.md")));
        assert!(is_known_doc(Path::new("a/sheet.XLSX")));
        assert!(is_known_doc(Path::new("a/report.pdf")));
        assert!(!is_known_doc(Path::new("a/archive.zip")));
        assert!(!is_known_doc(Path::new("a/noext")));
    }

    #[test]
    fn plain_text_is_always_extractable() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("note.txt");
        std::fs::write(&f, "hello world").unwrap();
        match extract(&f).unwrap() {
            Extracted::Text(t) => assert_eq!(t, "hello world"),
            _ => panic!("expected text"),
        }
    }
}
