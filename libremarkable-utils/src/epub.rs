//! Minimal EPUB 3 generation for text imports.
//!
//! The device renders only notebooks, PDF, and EPUB — `.txt`/`.md`
//! files are made to work by converting them to EPUB on the host at
//! upload time. See `docs/text-import.md` for the nuances (raw-HTML
//! escaping, reflow trade-offs, one-way conversion).
//!
//! The generated archive is deliberately minimal but spec-valid:
//! `mimetype` (stored uncompressed, first entry — required by the EPUB
//! OCF spec and checked by strict readers), `META-INF/container.xml`,
//! a nav document, one XHTML content file, and the OPF manifest.

use std::io::{Cursor, Write};

use pulldown_cmark::{Event, Options, Parser, html};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{Error, Result};

/// Source flavor of a text import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// CommonMark + tables, strikethrough, task lists, footnotes.
    Markdown,
    /// Plain text: blank-line paragraphs, single newlines become `<br/>`.
    Plain,
}

/// Convert Markdown or plain text into EPUB bytes. `title` becomes both
/// the EPUB title and (by the caller) the device's visible name.
pub fn text_to_epub(title: &str, kind: TextKind, source: &str) -> Result<Vec<u8>> {
    let body = match kind {
        TextKind::Markdown => markdown_to_xhtml(source),
        TextKind::Plain => plain_text_to_xhtml(source),
    };
    build_epub(title, &body)
}

/// Markdown → XHTML via pulldown-cmark.
///
/// Raw HTML events are escaped to literal text: xochitl's EPUB reader
/// wants well-formed XHTML, and arbitrary inline HTML could produce a
/// document the device refuses to render.
fn markdown_to_xhtml(source: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let events = Parser::new_ext(source, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// Plain text → XHTML: blank-line-separated paragraphs, single
/// newlines rendered as line breaks. Not `<pre>` — that would defeat
/// reflow on the device.
fn plain_text_to_xhtml(source: &str) -> String {
    source
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| {
            let lines: Vec<String> = paragraph.lines().map(escape_xml).collect();
            format!("<p>{}</p>\n", lines.join("<br/>\n"))
        })
        .collect()
}

fn escape_xml(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn build_epub(title: &str, body_xhtml: &str) -> Result<Vec<u8>> {
    let title = escape_xml(title);
    let identifier = uuid::Uuid::new_v4();

    let container_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

    let content_opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="pub-id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="pub-id">urn:uuid:{identifier}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2000-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="content" href="content.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="content"/>
  </spine>
</package>
"#
    );

    let nav_xhtml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>{title}</title></head>
<body>
<nav epub:type="toc"><ol><li><a href="content.xhtml">{title}</a></li></ol></nav>
</body>
</html>
"#
    );

    let content_xhtml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>{title}</title></head>
<body>
{body_xhtml}</body>
</html>
"#
    );

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // The OCF spec requires `mimetype` first and uncompressed so the
    // magic bytes sit at a fixed offset.
    zip.start_file("mimetype", stored).map_err(zip_err)?;
    zip.write_all(b"application/epub+zip")?;

    [
        ("META-INF/container.xml", container_xml.to_string()),
        ("OEBPS/content.opf", content_opf),
        ("OEBPS/nav.xhtml", nav_xhtml),
        ("OEBPS/content.xhtml", content_xhtml),
    ]
    .into_iter()
    .try_for_each(|(path, contents)| -> Result<()> {
        zip.start_file(path, deflated).map_err(zip_err)?;
        zip.write_all(contents.as_bytes())?;
        Ok(())
    })?;

    Ok(zip.finish().map_err(zip_err)?.into_inner())
}

fn zip_err(err: zip::result::ZipError) -> Error {
    Error::Epub(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_entry(zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> String {
        let mut out = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn epub_structure_is_valid_ocf() {
        let bytes = text_to_epub("My Notes", TextKind::Plain, "hello").unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();

        // mimetype must be the first entry and stored uncompressed.
        let first = zip.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), CompressionMethod::Stored);
        drop(first);
        assert_eq!(read_entry(&mut zip, "mimetype"), "application/epub+zip");

        let container = read_entry(&mut zip, "META-INF/container.xml");
        assert!(container.contains("OEBPS/content.opf"));

        let opf = read_entry(&mut zip, "OEBPS/content.opf");
        assert!(opf.contains("<dc:title>My Notes</dc:title>"));
        assert!(opf.contains(r#"properties="nav""#));

        let content = read_entry(&mut zip, "OEBPS/content.xhtml");
        assert!(content.contains("<p>hello</p>"));
    }

    #[test]
    fn markdown_renders_and_escapes_raw_html() {
        let source = "# Title\n\nSome *emphasis*.\n\n<script>alert(1)</script>\n\n- [x] done";
        let xhtml = markdown_to_xhtml(source);
        assert!(xhtml.contains("<h1>Title</h1>"));
        assert!(xhtml.contains("<em>emphasis</em>"));
        // Raw HTML must be escaped, never passed through.
        assert!(!xhtml.contains("<script>"));
        assert!(xhtml.contains("&lt;script&gt;"));
        assert!(xhtml.contains("checkbox"));
    }

    #[test]
    fn plain_text_paragraphs_and_breaks() {
        let xhtml = plain_text_to_xhtml("first line\nsecond line\n\nnext & <para>\r\n\r\nlast");
        assert_eq!(
            xhtml,
            "<p>first line<br/>\nsecond line</p>\n<p>next &amp; &lt;para&gt;</p>\n<p>last</p>\n"
        );
    }

    #[test]
    fn title_is_escaped_everywhere() {
        let bytes = text_to_epub("A & B <notes>", TextKind::Plain, "x").unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let opf = read_entry(&mut zip, "OEBPS/content.opf");
        assert!(opf.contains("A &amp; B &lt;notes&gt;"));
        let nav = read_entry(&mut zip, "OEBPS/nav.xhtml");
        assert!(nav.contains("A &amp; B &lt;notes&gt;"));
    }
}
