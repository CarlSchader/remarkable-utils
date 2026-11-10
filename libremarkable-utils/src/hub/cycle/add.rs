//! `add`: a PDF or EPUB committed to origin as a new document, without a
//! tablet (see "History, restore and add" in `docs/hub-design.md`). A PDF
//! whose page count [`crate::pdf::page_count`] reads gets a materialised
//! page list with deterministic page ids, so tablets opening it never
//! fork; anything else gets rmu's unmaterialised skeleton. The `.content`
//! rules are pure and tested here.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use super::library::{Decided, NewDoc, items_of, place, usable_folder};
use super::message::{self, Author};
use super::{AddReport, CycleError, Hub, rmu_signature, unix_now};
use crate::error::{Error, Result};
use crate::git::{CommitMeta, FileMode};
use crate::hub::device::Tablet;
use crate::hub::model::{DocFile, DocId, ListingEntry};
use crate::xochitl;

/// The id of page `index` of a document added by rmu: UUIDv5 with the
/// document's UUID as namespace and the index in decimal as name.
pub(super) fn page_id(doc: &DocId, index: usize) -> String {
    let namespace = uuid::Uuid::parse_str(doc.as_str()).expect("a document id is a UUID");
    uuid::Uuid::new_v5(&namespace, index.to_string().as_bytes()).to_string()
}

/// The `idx` of page `index` among `count`: `b` and the index in base 26
/// (`a` to `z`), all of one width, so they sort as the pages do (`ba`,
/// `bb`, ... for up to 26 pages, as xochitl starts).
pub(super) fn page_idx(index: usize, count: usize) -> String {
    let width = std::iter::successors(Some(count.saturating_sub(1)), |n| {
        (*n >= 26).then_some(n / 26)
    })
    .count();
    let digits: Vec<u8> = (0..width)
        .scan(index, |rest, _| {
            let digit = b'a' + (*rest % 26) as u8;
            *rest /= 26;
            Some(digit)
        })
        .collect();
    std::iter::once('b')
        .chain(digits.into_iter().rev().map(char::from))
        .collect()
}

/// The `.content` of an added document: rmu's skeleton, with a
/// materialised page list of `pages` pages if given (registers stamped
/// as xochitl stamps a first open), `cPages.original` and `pageCount` the
/// page count, `sizeInBytes` the payload's size.
pub(super) fn added_content(
    doc: &DocId,
    file_type: &str,
    pages: Option<usize>,
    size: u64,
) -> Value {
    let mut content: Value = serde_json::from_str(&xochitl::document_content_json(file_type))
        .expect("the skeleton is JSON");
    if let Some(count) = pages {
        let entries: Vec<Value> = (0..count)
            .map(|index| {
                json!({
                    "id": page_id(doc, index),
                    "idx": { "timestamp": "1:2", "value": page_idx(index, count) },
                    "redir": { "timestamp": "1:2", "value": index },
                })
            })
            .collect();
        content["cPages"]["pages"] = Value::from(entries);
        content["cPages"]["original"] = json!({ "timestamp": "1:1", "value": count });
        content["pageCount"] = Value::from(count);
        content["sizeInBytes"] = Value::from(size.to_string());
        content["tags"] = json!([]);
    }
    content
}

/// A visible name for a new document: trimmed, not empty, without `/`.
fn checked_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::InvalidName("name cannot be empty".to_string()));
    }
    if name.contains('/') {
        return Err(Error::InvalidName("name cannot contain '/'".to_string()));
    }
    Ok(name.to_string())
}

impl Hub<'_> {
    /// `rmu hub add <file> [folder]`: a `.pdf` or `.epub` as a new
    /// document (fresh UUID) in `folder` (a logical path or UUID on
    /// `main`; the root if `None`), named `name` or the file's stem,
    /// committed onto origin's `main` and pushed. A name already taken in
    /// the folder is refused. With a tablet (which supplies the origin URL
    /// unless [`super::HubOptions::origin`] is set), it is the commit's
    /// author.
    pub fn add(
        &self,
        tablet: Option<&dyn Tablet>,
        file: &Path,
        folder: Option<&str>,
        name: Option<&str>,
    ) -> Result<AddReport> {
        let result = self.add_inner(tablet, file, folder, name);
        self.progress.finished();
        result
    }

    fn add_inner(
        &self,
        tablet: Option<&dyn Tablet>,
        file: &Path,
        folder: Option<&str>,
        name: Option<&str>,
    ) -> Result<AddReport> {
        let file_type = file
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let payload_file = match file_type.as_str() {
            "pdf" => DocFile::PayloadPdf,
            "epub" => DocFile::PayloadEpub,
            _ => return Err(CycleError::NotAddable(file_type).into()),
        };
        if !file.is_file() {
            return Err(Error::FileNotFound(file.to_path_buf()));
        }
        let name = checked_name(&match name {
            Some(name) => name.to_string(),
            None => file.file_stem().map_or_else(
                || "untitled".to_string(),
                |stem| stem.to_string_lossy().into_owned(),
            ),
        })?;
        self.progress.step("Reading the file");
        let bytes = std::fs::read(file)?;
        let pages = match payload_file {
            DocFile::PayloadPdf => crate::pdf::page_count(&bytes),
            _ => None,
        };
        let mut library = self.open_library(tablet, true)?;
        let doc = DocId::new(uuid::Uuid::new_v4().to_string())?;
        let folder_ref = folder.unwrap_or("");
        let parent = {
            let origin = &library.origin;
            let theirs = origin.store.tree_of(origin.rules, &origin.main)?;
            let items = items_of(&theirs, &origin.store)?;
            let parent = xochitl::resolve_folder_ref(&items, folder_ref)?;
            if !usable_folder(&items, &parent) {
                return Err(CycleError::FolderInTrash(folder_ref.to_string()).into());
            }
            parent
        };
        // The payload and `.content` go into the cache once, in a commit
        // no ref points to; every attempt reuses their blobs.
        self.progress.step("Storing the document");
        let content =
            serde_json::to_vec_pretty(&added_content(&doc, &file_type, pages, bytes.len() as u64))
                .expect("JSON serialises");
        let mut writer = library
            .origin
            .cache
            .repo
            .commit_writer(self.progress, None)?;
        let payload = writer.add_bytes(payload_file.repo_name(), FileMode::Regular, &bytes)?;
        let content = writer.add_bytes("content", FileMode::Regular, &content)?;
        let signature = rmu_signature(unix_now());
        writer.commit(&CommitMeta {
            parents: Vec::new(),
            author: signature.clone(),
            committer: signature,
            message: "rmu hub: add (stored, not pushed)\n".to_string(),
        })?;
        let new = NewDoc {
            doc: doc.clone(),
            files: BTreeMap::from([
                (payload_file, ListingEntry::regular(payload)),
                (DocFile::Content, ListingEntry::regular(content)),
            ]),
            other: BTreeMap::new(),
        };
        let device = library.tablet.as_ref().map(|attached| {
            (
                attached.state.device_name.clone(),
                attached.state.device_id.clone(),
                attached.info.clone(),
            )
        });
        let author = device.as_ref().map(|(name, id, info)| Author {
            name,
            id,
            model: info.model.as_deref(),
            firmware: info.firmware.as_deref(),
        });
        let now_ms = unix_now().saturating_mul(1000);
        let (commit, attempts, place) =
            self.push_new_doc(&mut library, &new, &mut |origin, theirs| {
                let items = items_of(theirs, &origin.store)?;
                if !usable_folder(&items, &parent) {
                    return Err(CycleError::FolderGone(folder_ref.to_string()).into());
                }
                xochitl::ensure_no_conflict(&items, &parent, &name, None)?;
                Ok(Decided {
                    metadata: xochitl::document_metadata_json(&name, &parent, now_ms).into_bytes(),
                    message: message::add(author.as_ref(), &name, &doc),
                    outcome: place(&items, &parent),
                })
            })?;
        Ok(AddReport {
            doc,
            name,
            place,
            file_type,
            bytes: bytes.len() as u64,
            pages,
            commit,
            push_attempts: attempts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Oid;
    use crate::hub::merge::unchanged;
    use crate::hub::model::{DocVersion, MirrorRules};

    fn doc() -> DocId {
        DocId::new("8c1f0b9e-0000-4000-8000-000000000001").unwrap()
    }

    #[test]
    fn page_ids_are_deterministic_per_document_and_index() {
        let other = DocId::new("8c1f0b9e-0000-4000-8000-000000000002").unwrap();
        assert_eq!(page_id(&doc(), 0), page_id(&doc(), 0));
        assert_ne!(page_id(&doc(), 0), page_id(&doc(), 1));
        assert_ne!(page_id(&doc(), 0), page_id(&other, 0));
        let id = uuid::Uuid::parse_str(&page_id(&doc(), 3)).unwrap();
        assert_eq!(id.get_version_num(), 5);
        // Pinned: a change here would fork every document added before it.
        assert_eq!(
            page_id(&doc(), 0),
            uuid::Uuid::new_v5(&uuid::Uuid::parse_str(doc().as_str()).unwrap(), b"0").to_string()
        );
    }

    #[test]
    fn page_indices_sort_as_the_pages() {
        assert_eq!(page_idx(0, 1), "ba");
        assert_eq!(page_idx(1, 3), "bb");
        assert_eq!(page_idx(25, 26), "bz");
        assert_eq!(page_idx(0, 27), "baa");
        assert_eq!(page_idx(26, 27), "bba");
        assert_eq!(page_idx(702, 703), "bbba");
        let all: Vec<String> = (0..800).map(|index| page_idx(index, 800)).collect();
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted);
        assert!(all.iter().all(|idx| idx.len() == all[0].len()));
    }

    /// One version per `.content` (blob ids from their position).
    fn versions(contents: &[Value]) -> (Vec<DocVersion>, BTreeMap<Oid, Vec<u8>>) {
        let metadata = xochitl::document_metadata_json("Paper", "", 1);
        let metadata_id: Oid = "f".repeat(40).parse().unwrap();
        let mut blobs = BTreeMap::from([(metadata_id.clone(), metadata.into_bytes())]);
        let versions = contents
            .iter()
            .enumerate()
            .map(|(n, content)| {
                let id: Oid = format!("{n:040x}").parse().unwrap();
                blobs.insert(id.clone(), serde_json::to_vec(content).unwrap());
                DocVersion {
                    files: [
                        (
                            DocFile::Metadata,
                            ListingEntry::regular(metadata_id.clone()),
                        ),
                        (DocFile::Content, ListingEntry::regular(id)),
                    ]
                    .into(),
                    other: BTreeMap::new(),
                }
            })
            .collect();
        (versions, blobs)
    }

    #[test]
    fn the_materialised_list_reads_as_unopened_and_survives_a_rematerialisation() {
        let rules = MirrorRules::latest();
        let added = added_content(&doc(), "pdf", Some(3), 1234);
        assert_eq!(added["pageCount"], 3);
        assert_eq!(added["cPages"]["original"]["value"], 3);
        assert_eq!(added["sizeInBytes"], "1234");
        assert_eq!(added["cPages"]["pages"][1]["id"], page_id(&doc(), 1));
        assert_eq!(added["cPages"]["pages"][2]["redir"]["value"], 2);
        let skeleton = added_content(&doc(), "pdf", None, 1234);
        assert_eq!(skeleton["cPages"]["pages"], json!([]));
        // A tablet that materialised the list again, with ids of its own.
        let mut again = added.clone();
        (0..3).for_each(|n| {
            again["cPages"]["pages"][n]["id"] = Value::from(format!("other-{n}"));
        });
        // One page short: a real change.
        let mut short = added.clone();
        short["cPages"]["pages"].as_array_mut().unwrap().pop();
        short["cPages"]["original"]["value"] = Value::from(2);
        let (v, blobs) = versions(&[skeleton, added, again, short]);
        let same = |base: &DocVersion, side: &DocVersion| {
            unchanged(rules, &doc(), base, side, &blobs).unwrap()
        };
        assert!(
            same(&v[0], &v[1]),
            "a first-open materialisation of the skeleton"
        );
        assert!(same(&v[1], &v[2]), "materialised again over rmu's list");
        assert!(!same(&v[1], &v[3]), "another page count is a change");
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(checked_name("  Paper ").unwrap(), "Paper");
        assert!(matches!(checked_name(" "), Err(Error::InvalidName(_))));
        assert!(matches!(checked_name("a/b"), Err(Error::InvalidName(_))));
    }
}
