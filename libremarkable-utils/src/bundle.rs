//! `.rmdoc` bundle creation and parsing.
//!
//! An `.rmdoc` file — the export/import format of the official
//! reMarkable apps — is a zip archive of the raw xochitl file set for
//! one document (`<uuid>.metadata`, `<uuid>.content`, `<uuid>/*.rm`,
//! thumbnails, and the payload when there is one), with paths relative
//! to the data directory. See `docs/notebook-data.md`.
//!
//! The device ships busybox only (no `zip`), so both directions go
//! through tar: downloads stream a `tar -c` from the device and repack
//! it into a zip locally; uploads unpack the zip locally, re-target it
//! to a fresh UUID, and stream a tar into `tar -x` on the device.

use std::io::{self, Cursor, Read};

use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{Error, Result};

/// Repack a tar stream (as produced by busybox `tar -cf -` on the
/// device) into `.rmdoc` zip bytes. Only regular files and directories
/// are carried over; entry paths are preserved relative.
pub fn tar_to_rmdoc(tar_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    archive.entries()?.try_for_each(|entry| -> Result<()> {
        let mut entry = entry?;
        let path = entry
            .path()?
            .to_string_lossy()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        if path.is_empty() || is_appledouble(&path) {
            return Ok(());
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                zip.add_directory(&path, options).map_err(zip_err)?;
            }
            tar::EntryType::Regular => {
                zip.start_file(&path, options).map_err(zip_err)?;
                io::copy(&mut entry, &mut zip)?;
            }
            // Symlinks etc. do not occur in xochitl data; skip rather
            // than produce a bundle the official importer might reject.
            _ => {}
        }
        Ok(())
    })?;

    Ok(zip.finish().map_err(zip_err)?.into_inner())
}

/// A parsed `.rmdoc` bundle for one document.
pub struct Rmdoc {
    /// The document UUID the bundle was exported under.
    pub uuid: String,
    /// Parsed root `.metadata` JSON. Callers may mutate this (e.g.
    /// `parent`, `visibleName`) before re-targeting with
    /// [`rmdoc_to_tar`]; unknown fields are preserved.
    pub metadata: Value,
    /// Parsed `.content` JSON, when present.
    pub content: Option<Value>,
    /// Every other entry, verbatim (`None` data = directory).
    files: Vec<RmdocEntry>,
}

struct RmdocEntry {
    path: String,
    data: Option<Vec<u8>>,
}

impl Rmdoc {
    /// Visible name recorded in the bundle's metadata.
    pub fn visible_name(&self) -> Option<&str> {
        self.metadata.get("visibleName").and_then(Value::as_str)
    }

    /// Document type, with the same normalization as live listings:
    /// empty/missing `fileType` with a `.content` present means a
    /// native notebook.
    pub fn file_type(&self) -> Option<&str> {
        let content = self.content.as_ref()?;
        Some(
            content
                .get("fileType")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("notebook"),
        )
    }
}

/// Parse `.rmdoc` bytes. Expects exactly one document (one top-level
/// `.metadata` entry); every entry must belong to that document's UUID.
pub fn parse_rmdoc(bytes: &[u8]) -> Result<Rmdoc> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(zip_err)?;

    let uuid = (0..zip.len())
        .try_fold(None::<String>, |found, index| {
            let name = zip.by_index(index).map_err(zip_err)?.name().to_string();
            let stem = (!name.contains('/') && !is_appledouble(&name))
                .then(|| name.strip_suffix(".metadata"))
                .flatten();
            match (found, stem) {
                (Some(_), Some(_)) => Err(Error::Bundle(
                    "multiple .metadata entries; expected exactly one document".to_string(),
                )),
                (found, stem) => Ok(stem.map(str::to_string).or(found)),
            }
        })?
        .ok_or_else(|| Error::Bundle("no .metadata entry found".to_string()))?;

    type Parsed = (Option<Value>, Option<Value>, Vec<RmdocEntry>);
    let (metadata, content, files) = (0..zip.len()).try_fold(
        (None, None, Vec::new()) as Parsed,
        |(metadata, mut content, mut files), index| {
            let mut entry = zip.by_index(index).map_err(zip_err)?;
            let is_dir = entry.is_dir();
            let path = entry.name().trim_end_matches('/').to_string();
            if path.is_empty() || is_appledouble(&path) {
                return Ok((metadata, content, files));
            }
            if !belongs_to(&path, &uuid) {
                return Err(Error::Bundle(format!(
                    "entry '{path}' does not belong to document {uuid}"
                )));
            }
            if is_dir {
                files.push(RmdocEntry { path, data: None });
                return Ok((metadata, content, files));
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let parse = |data: &[u8]| {
                serde_json::from_slice::<Value>(data).map_err(|source| Error::Json {
                    path: path.clone(),
                    source,
                })
            };
            if path == format!("{uuid}.metadata") {
                // Regenerated from the parsed (possibly mutated) value
                // in rmdoc_to_tar; not carried as a verbatim file.
                return Ok((Some(parse(&data)?), content, files));
            }
            if path == format!("{uuid}.content") {
                content = Some(parse(&data)?);
            }
            files.push(RmdocEntry {
                path,
                data: Some(data),
            });
            Ok((metadata, content, files))
        },
    )?;

    Ok(Rmdoc {
        uuid,
        metadata: metadata.expect("metadata entry located above"),
        content,
        files,
    })
}

/// Serialize a bundle as a tar stream targeting `new_uuid`, suitable
/// for `tar -xf -` in the xochitl data dir. All entry paths are
/// renamed from the bundle's original UUID; the `.metadata` entry is
/// regenerated from [`Rmdoc::metadata`].
pub fn rmdoc_to_tar(rmdoc: &Rmdoc, new_uuid: &str) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());

    rmdoc.files.iter().try_for_each(|entry| {
        let renamed = format!("{new_uuid}{}", &entry.path[rmdoc.uuid.len()..]);
        match &entry.data {
            Some(data) => append_file(&mut builder, &renamed, data),
            None => append_dir(&mut builder, &renamed),
        }
    })?;

    // The .metadata entry goes LAST: tar extracts in order, and a
    // document only becomes visible to xochitl once its .metadata
    // exists. An interrupted extraction therefore leaves invisible
    // orphan files instead of a half-restored document.
    let metadata_text =
        serde_json::to_string_pretty(&rmdoc.metadata).map_err(|source| Error::Json {
            path: format!("{new_uuid}.metadata"),
            source,
        })?;
    append_file(
        &mut builder,
        &format!("{new_uuid}.metadata"),
        metadata_text.as_bytes(),
    )?;
    Ok(builder.into_inner()?)
}

fn append_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append_data(&mut header, path, data)?;
    Ok(())
}

fn append_dir(builder: &mut tar::Builder<Vec<u8>>, path: &str) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append_data(&mut header, path, &[][..])?;
    Ok(())
}

/// `path` belongs to `uuid` iff it is `uuid` itself or continues with
/// `.` or `/` (rejects sibling documents sharing a prefix).
fn belongs_to(path: &str, uuid: &str) -> bool {
    path.strip_prefix(uuid)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.') || rest.starts_with('/'))
}

fn zip_err(err: zip::result::ZipError) -> Error {
    Error::Bundle(err.to_string())
}

/// AppleDouble sidecar entries (`._foo`) appear when a tar was created
/// by macOS bsdtar. The device's busybox tar never produces them, and
/// xochitl never creates such filenames, but they must not leak into a
/// bundle the official importer might read.
fn is_appledouble(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("._"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tar_with(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in entries {
            match contents {
                Some(data) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(data.len() as u64);
                    header.set_mode(0o644);
                    header.set_cksum();
                    builder.append_data(&mut header, path, *data).unwrap();
                }
                None => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_cksum();
                    builder.append_data(&mut header, path, &[][..]).unwrap();
                }
            }
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn tar_round_trips_to_zip() {
        let tar_bytes = tar_with(&[
            ("u1.metadata", Some(br#"{"type":"DocumentType"}"#)),
            ("u1.content", Some(br#"{"fileType":"notebook"}"#)),
            ("u1", None),
            ("u1/page1.rm", Some(b"stroke data")),
        ]);

        let zip_bytes = tar_to_rmdoc(&tar_bytes).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();

        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["u1.content", "u1.metadata", "u1/", "u1/page1.rm"]);

        let mut page = String::new();
        zip.by_name("u1/page1.rm")
            .unwrap()
            .read_to_string(&mut page)
            .unwrap();
        assert_eq!(page, "stroke data");
    }

    #[test]
    fn appledouble_entries_are_dropped() {
        let tar_bytes = tar_with(&[
            ("._u1.metadata", Some(b"apple noise".as_slice())),
            ("u1/._page1.rm", Some(b"apple noise".as_slice())),
            ("u1.metadata", Some(br#"{}"#.as_slice())),
        ]);
        let zip_bytes = tar_to_rmdoc(&tar_bytes).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, ["u1.metadata"]);
    }

    #[test]
    fn empty_tar_yields_empty_zip() {
        let zip_bytes = tar_to_rmdoc(&tar_with(&[])).unwrap();
        let zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        assert_eq!(zip.len(), 0);
    }

    fn sample_rmdoc() -> Vec<u8> {
        tar_to_rmdoc(&tar_with(&[
            (
                "aaa.metadata",
                Some(
                    br#"{"type":"DocumentType","visibleName":"My Notes","parent":"old"}"#
                        .as_slice(),
                ),
            ),
            (
                "aaa.content",
                Some(br#"{"fileType":"notebook"}"#.as_slice()),
            ),
            ("aaa.pagedata", Some(b"Blank\n".as_slice())),
            ("aaa", None),
            ("aaa/p1.rm", Some(b"strokes".as_slice())),
        ]))
        .unwrap()
    }

    #[test]
    fn parse_and_retarget_rmdoc() {
        let mut rmdoc = parse_rmdoc(&sample_rmdoc()).unwrap();
        assert_eq!(rmdoc.uuid, "aaa");
        assert_eq!(rmdoc.visible_name(), Some("My Notes"));
        assert_eq!(rmdoc.file_type(), Some("notebook"));

        // Mutate like an upload would.
        let object = rmdoc.metadata.as_object_mut().unwrap();
        object.insert("parent".to_string(), Value::String("newparent".to_string()));

        let tar_bytes = rmdoc_to_tar(&rmdoc, "bbb").unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut paths = Vec::new();
        let mut metadata_text = String::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            if path == "bbb.metadata" {
                entry.read_to_string(&mut metadata_text).unwrap();
            }
            paths.push(path);
        }
        // Crash consistency: .metadata must be the LAST entry so an
        // interrupted extraction never leaves a visible document.
        assert_eq!(paths.last().map(String::as_str), Some("bbb.metadata"));
        paths.sort();
        assert_eq!(
            paths,
            [
                "bbb",
                "bbb.content",
                "bbb.metadata",
                "bbb.pagedata",
                "bbb/p1.rm"
            ]
        );

        let metadata: Value = serde_json::from_str(&metadata_text).unwrap();
        assert_eq!(metadata["parent"], "newparent");
        assert_eq!(metadata["visibleName"], "My Notes");
        // Unknown/other fields preserved.
        assert_eq!(metadata["type"], "DocumentType");
    }

    #[test]
    fn rmdoc_without_metadata_is_rejected() {
        let bytes = tar_to_rmdoc(&tar_with(&[("aaa.content", Some(b"{}".as_slice()))])).unwrap();
        assert!(matches!(parse_rmdoc(&bytes), Err(Error::Bundle(_))));
    }

    #[test]
    fn rmdoc_with_multiple_documents_is_rejected() {
        let bytes = tar_to_rmdoc(&tar_with(&[
            ("aaa.metadata", Some(b"{}".as_slice())),
            ("bbb.metadata", Some(b"{}".as_slice())),
        ]))
        .unwrap();
        assert!(matches!(parse_rmdoc(&bytes), Err(Error::Bundle(_))));
    }

    #[test]
    fn rmdoc_with_foreign_entries_is_rejected() {
        // "aaabbb.pdf" shares a prefix with "aaa" but is not the same doc.
        let bytes = tar_to_rmdoc(&tar_with(&[
            ("aaa.metadata", Some(b"{}".as_slice())),
            ("aaabbb.pdf", Some(b"x".as_slice())),
        ]))
        .unwrap();
        assert!(matches!(parse_rmdoc(&bytes), Err(Error::Bundle(_))));
    }
}
