//! `.rmdoc` bundle creation.
//!
//! An `.rmdoc` file — the export/import format of the official
//! reMarkable apps — is a zip archive of the raw xochitl file set for
//! one document (`<uuid>.metadata`, `<uuid>.content`, `<uuid>/*.rm`,
//! thumbnails, and the payload when there is one), with paths relative
//! to the data directory. See `docs/notebook-data.md`.
//!
//! The device ships busybox only (no `zip`), so we stream a `tar` from
//! the device and repack it into a zip locally.

use std::io::{self, Cursor};

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

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry
            .path()?
            .to_string_lossy()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        if path.is_empty() || is_appledouble(&path) {
            continue;
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
    }

    Ok(zip.finish().map_err(zip_err)?.into_inner())
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
}
