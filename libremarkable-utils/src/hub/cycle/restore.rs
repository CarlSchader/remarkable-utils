//! `restore`: an old version of a document, committed to origin as a new
//! document with a fresh UUID (see "History, restore and add" in
//! `docs/hub-design.md`). The original is never touched; every tablet's
//! next sync adds the copy. The naming, placement and `.metadata` rules
//! are pure and tested here.

use serde_json::Value;

use super::history::HistoryIndex;
use super::library::{
    Decided, NewDoc, items_of, metadata_of, name_of, place, resolve_doc, usable_folder,
};
use super::message::{self, Author};
use super::origin::OriginCtx;
use super::store::{LOCAL_MAIN, Store};
use super::{CycleError, Hub, RestoreReport, unix_now};
use crate::error::{Error, Result};
use crate::git::Oid;
use crate::hub::device::Tablet;
use crate::hub::model::{DocId, DocVersion, MirrorRules};
use crate::xochitl::{self, Item, ItemKind};

/// The commit `rev` names in the cache: a commit id (abbreviated or not)
/// or `main`, `main~N`, `main^N` (origin's `main` as fetched).
pub(super) fn resolve_rev(origin: &OriginCtx, rev: &str) -> Result<Oid> {
    let unknown = || Error::from(CycleError::UnknownRevision(rev.to_string()));
    let trimmed = rev.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return Err(unknown());
    }
    let mapped = match trimmed.strip_prefix("main") {
        Some(rest) if rest.is_empty() || rest.starts_with(['~', '^']) => {
            format!("{LOCAL_MAIN}{rest}")
        }
        _ => trimmed.to_string(),
    };
    origin
        .cache
        .repo
        .rev_parse(&format!("{mapped}^{{commit}}"))?
        .ok_or_else(unknown)
}

/// The version of `doc` to restore from commit `at`, with the commit
/// holding it: `at`'s own if `doc` is present there, else (deleted by
/// then) the version just before its newest deletion on `at`'s
/// first-parent chain.
fn version_to_restore(
    store: &Store,
    rules: &MirrorRules,
    index: &HistoryIndex,
    at: &Oid,
    doc: &DocId,
) -> Result<(Oid, DocVersion)> {
    if let Some(subtree) = index.subtree_at(at, doc).flatten() {
        let version = store.doc_version(rules, doc, subtree)?;
        if version.is_present() {
            return Ok((at.clone(), version));
        }
    }
    for (commit, (_, before, _)) in index.doc_chain(at, doc).unwrap_or_default() {
        let Some(before) = before else {
            continue;
        };
        let version = store.doc_version(rules, doc, before)?;
        if version.is_present()
            && let Some(parent) = store.commit(commit)?.parents.first()
        {
            return Ok((parent.clone(), version));
        }
    }
    Err(CycleError::NoVersionAt {
        doc: doc.clone(),
        at: at.clone(),
    }
    .into())
}

/// Where the copy goes: its original folder if that is present, a
/// folder and outside the trash on the `main` being built on; the root
/// otherwise.
pub(super) fn restore_parent(original: &str, items: &[Item]) -> String {
    match usable_folder(items, original) {
        true => original.to_string(),
        false => String::new(),
    }
}

/// `<name> (restored <date>)`, or with the copy's first 8 hex digits
/// appended if `taken` (as conflict copies are named).
pub(super) fn restored_name(
    name: &str,
    date: &str,
    copy: &DocId,
    taken: &dyn Fn(&str) -> bool,
) -> String {
    let plain = format!("{name} (restored {date})");
    match taken(&plain) {
        false => plain,
        true => format!("{name} (restored {date} {})", &copy.as_str()[..8]),
    }
}

/// Whether `parent` already holds an item named `name`.
pub(super) fn name_taken(items: &[Item], parent: &str, name: &str) -> bool {
    items
        .iter()
        .any(|item| item.parent == parent && item.visible_name == name)
}

/// The copy's `.metadata`: the original's with `visibleName`, `parent`
/// and `lastModified` (now, as a string or a number like the original's)
/// rewritten; every other field kept.
pub(super) fn restored_metadata(old: &Value, name: &str, parent: &str, now_ms: i64) -> Vec<u8> {
    let mut metadata = old.clone();
    let modified = match old.get("lastModified") {
        Some(Value::Number(_)) => Value::from(now_ms),
        _ => Value::from(now_ms.to_string()),
    };
    if let Some(map) = metadata.as_object_mut() {
        map.insert("visibleName".to_string(), Value::from(name));
        map.insert("parent".to_string(), Value::from(parent));
        map.insert("lastModified".to_string(), modified);
    }
    serde_json::to_vec_pretty(&metadata).expect("JSON serialises")
}

impl Hub<'_> {
    /// `rmu hub restore <doc> --at <rev>`: `doc`'s version at `rev` (or
    /// just before its deletion) as a new document with a fresh UUID,
    /// committed onto origin's `main` and pushed. `doc` is a UUID or a
    /// logical path, resolved at `rev`; `rev` must be `main` or one of
    /// its ancestors. With a tablet (which supplies the origin URL unless
    /// [`super::HubOptions::origin`] is set), it is the commit's author.
    pub fn restore(
        &self,
        tablet: Option<&dyn Tablet>,
        reference: &str,
        rev: &str,
    ) -> Result<RestoreReport> {
        let result = self.restore_inner(tablet, reference, rev);
        self.progress.finished();
        result
    }

    fn restore_inner(
        &self,
        tablet: Option<&dyn Tablet>,
        reference: &str,
        rev: &str,
    ) -> Result<RestoreReport> {
        let mut library = self.open_library(tablet, true)?;
        let (at, doc, from, version, metadata) = {
            let origin = &library.origin;
            let (store, rules) = (&origin.store, origin.rules);
            let at = resolve_rev(origin, rev)?;
            self.progress.step("Reading history");
            let index = store.history(&origin.main)?;
            if !index.contains(&at) {
                return Err(CycleError::NotOnMain(at).into());
            }
            let doc = resolve_doc(store, rules, &index, &at, reference)?;
            let (from, version) = version_to_restore(store, rules, &index, &at, &doc)?;
            let metadata =
                metadata_of(store, &version)?.ok_or_else(|| CycleError::NoVersionAt {
                    doc: doc.clone(),
                    at: at.clone(),
                })?;
            if xochitl::item_kind(&metadata) != Some(ItemKind::Document) {
                return Err(Error::NotADocument(reference.to_string()));
            }
            (at, doc, from, version, metadata)
        };
        let copy = DocId::new(uuid::Uuid::new_v4().to_string())?;
        let original_name = name_of(&metadata).unwrap_or_else(|| doc.to_string());
        let original_parent = metadata
            .get("parent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let date = self.date();
        let now_ms = unix_now().saturating_mul(1000);
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
        let new = NewDoc {
            doc: copy.clone(),
            files: version.files,
            other: version.other,
        };
        let (commit, attempts, (name, place)) =
            self.push_new_doc(&mut library, &new, &mut |origin, theirs| {
                let items = items_of(theirs, &origin.store)?;
                let parent = restore_parent(&original_parent, &items);
                let name = restored_name(&original_name, &date, &copy, &|candidate| {
                    name_taken(&items, &parent, candidate)
                });
                Ok(Decided {
                    metadata: restored_metadata(&metadata, &name, &parent, now_ms),
                    message: message::restore(author.as_ref(), &name, &copy, &doc, &from),
                    outcome: (name, place(&items, &parent)),
                })
            })?;
        Ok(RestoreReport {
            original: doc,
            copy,
            name,
            place,
            at,
            from,
            commit,
            push_attempts: attempts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(uuid: &str, name: &str, parent: &str, kind: ItemKind) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind,
            file_type: None,
            created_time: 0,
            last_modified: 0,
            size_bytes: None,
        }
    }

    #[test]
    fn the_copy_goes_to_its_folder_if_it_still_holds_documents() {
        let items = vec![
            item("books", "Books", "", ItemKind::Folder),
            item("old", "Old", "trash", ItemKind::Folder),
            item("paper", "Paper", "books", ItemKind::Document),
        ];
        assert_eq!(restore_parent("books", &items), "books");
        assert_eq!(restore_parent("", &items), "");
        assert_eq!(restore_parent("old", &items), "", "in the trash");
        assert_eq!(restore_parent("trash", &items), "");
        assert_eq!(restore_parent("gone", &items), "", "deleted");
        assert_eq!(restore_parent("paper", &items), "", "not a folder");
    }

    #[test]
    fn copies_are_named_with_the_date_and_an_id_if_taken() {
        let copy = DocId::new("1a2b3c4d-0000-4000-8000-000000000001").unwrap();
        let items = vec![item(
            "x",
            "Notes (restored 2026-11-09)",
            "books",
            ItemKind::Document,
        )];
        let named = |parent: &str| {
            restored_name("Notes", "2026-11-09", &copy, &|name| {
                name_taken(&items, parent, name)
            })
        };
        assert_eq!(named(""), "Notes (restored 2026-11-09)");
        assert_eq!(named("books"), "Notes (restored 2026-11-09 1a2b3c4d)");
    }

    #[test]
    fn restored_metadata_keeps_every_other_field() {
        let old = json!({
            "visibleName": "Notes", "parent": "trash", "lastModified": "1",
            "type": "DocumentType", "pinned": true, "unknownField": [1, 2],
        });
        let new: Value = serde_json::from_slice(&restored_metadata(
            &old,
            "N (restored d)",
            "",
            1_794_182_400_000,
        ))
        .unwrap();
        assert_eq!(
            new,
            json!({
                "visibleName": "N (restored d)", "parent": "", "lastModified": "1794182400000",
                "type": "DocumentType", "pinned": true, "unknownField": [1, 2],
            })
        );
        // A numeric timestamp stays numeric.
        let new: Value = serde_json::from_slice(&restored_metadata(
            &json!({ "lastModified": 5 }),
            "n",
            "p",
            7,
        ))
        .unwrap();
        assert_eq!(new["lastModified"], json!(7));
    }
}
