//! `log`: the library's history (`main`'s first-parent chain: one entry
//! per sync, `restore` and `add`) and one document's. Read-only: origin
//! is fetched into the local cache, nothing is pushed, a tablet is only
//! asked for its hub state. Changes come from the history index; the
//! pure parts (commit kinds, author lines, what changed between two
//! versions) are tested here.

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value;

use super::history::{DocStep, HistoryIndex};
use super::library::{metadata_of, name_of, parent_of, place_with, resolve_doc};
use super::message::{self, ADDED, CONSENT, DEVICE, DEVICE_ID, FIRMWARE, MODEL, RESTORED};
use super::plan::JsonBlobs;
use super::store::Store;
use super::{
    CommitDevice, CommitKind, DocEvent, DocLogEntry, DocLogReport, ForkRecord, Hub, LogEntry,
    LogReport, Place, RestoreRecord,
};
use crate::error::Result;
use crate::git::{self, Oid};
use crate::hub::device::Tablet;
use crate::hub::model::{DocFile, DocId, DocVersion, JsonFile, MirrorRules};

/// What a commit is, from its subject, trailers and parents.
pub(super) fn commit_kind(subject: &str, trailers: &[(String, String)]) -> CommitKind {
    let has = |key: &str| message::values(trailers, key).next().is_some();
    if has(RESTORED) {
        CommitKind::Restore
    } else if has(ADDED) {
        CommitKind::Add
    } else if subject == "rmu hub: init" {
        CommitKind::Init
    } else if subject.starts_with("rmu hub: sync ") {
        CommitKind::Sync
    } else if subject.starts_with("rmu hub: snapshot of ") {
        CommitKind::Snapshot
    } else {
        CommitKind::Other
    }
}

/// The author's name and time (seconds since the epoch) from a raw
/// commit's `author Name <email> time zone` header.
pub(super) fn author_of(raw: &[u8]) -> Option<(String, i64)> {
    let header = raw
        .split(|&b| b == b'\n')
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix(b"author "))?;
    let header = String::from_utf8_lossy(header);
    let (identity, when) = header.rsplit_once('>')?;
    let name = identity.rsplit_once(" <")?.0.to_string();
    let time = when.split_whitespace().next()?.parse().ok()?;
    Some((name, time))
}

/// The device trailers, if a commit has any.
fn device(trailers: &[(String, String)]) -> Option<CommitDevice> {
    let first = |key: &str| message::values(trailers, key).next().map(str::to_string);
    let device = CommitDevice {
        name: first(DEVICE),
        id: first(DEVICE_ID),
        model: first(MODEL),
        firmware: first(FIRMWARE),
    };
    (device != CommitDevice::default()).then_some(device)
}

/// A commit as `log` shows it.
struct CommitInfo {
    kind: CommitKind,
    author: String,
    time: i64,
    subject: String,
    message: String,
    trailers: Vec<(String, String)>,
}

fn commit_info(store: &Store, commit: &Oid) -> Result<CommitInfo> {
    let raw = store.raw_commit(commit)?;
    let parsed = git::parse_commit(&raw)?;
    let (author, time) = author_of(&raw).unwrap_or_default();
    let trailers = message::trailers(&parsed.message);
    let subject = message::subject(&parsed.message).to_string();
    Ok(CommitInfo {
        kind: commit_kind(&subject, &trailers),
        author,
        time,
        subject,
        trailers,
        message: parsed.message,
    })
}

/// A present version's parsed `.metadata` (`None` if absent or not a
/// present version).
fn present(
    store: &Store,
    rules: &MirrorRules,
    doc: &DocId,
    subtree: Option<&Oid>,
) -> Result<Option<(DocVersion, Rc<Value>)>> {
    let Some(subtree) = subtree else {
        return Ok(None);
    };
    let version = store.doc_version(rules, doc, subtree)?;
    Ok(metadata_of(store, &version)?.map(|metadata| (version, metadata)))
}

/// What changed between two present versions of a document, besides
/// view state and derived fields: name, folder, anything else. Pure.
pub(super) fn version_changes(
    rules: &MirrorRules,
    before: (&DocVersion, &Value),
    after: (&DocVersion, &Value),
    json: &dyn JsonBlobs,
) -> Result<VersionChanges> {
    let field = |metadata: &Value, key: &str| metadata.get(key).cloned();
    let renamed = field(before.1, "visibleName") != field(after.1, "visibleName");
    let moved = field(before.1, "parent") != field(after.1, "parent");
    let rest = |metadata: &Value| {
        let mut normalised = rules.normalise(JsonFile::Metadata, metadata);
        if let Some(map) = normalised.as_object_mut() {
            map.remove("visibleName");
            map.remove("parent");
        }
        normalised
    };
    let content = |version: &DocVersion| -> Result<Option<Value>> {
        match version.files.get(&DocFile::Content) {
            Some(entry) => Ok(json
                .json(&entry.id)?
                .map(|value| rules.normalise(JsonFile::Content, &value))),
            None => Ok(None),
        }
    };
    let files = |version: &DocVersion| {
        version
            .files
            .iter()
            .filter(|(file, _)| !matches!(file, DocFile::Metadata | DocFile::Content))
            .map(|(file, entry)| (file.repo_name(), entry.clone()))
            .chain(version.other.clone())
            .collect::<BTreeMap<_, _>>()
    };
    let changed = rest(before.1) != rest(after.1)
        || files(before.0) != files(after.0)
        || (before.0.files.get(&DocFile::Content) != after.0.files.get(&DocFile::Content)
            && content(before.0)? != content(after.0)?);
    Ok(VersionChanges {
        renamed,
        moved,
        changed,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VersionChanges {
    pub renamed: bool,
    pub moved: bool,
    pub changed: bool,
}

/// Where a document with parent `parent` was in `commit`.
fn place_at(store: &Store, rules: &MirrorRules, commit: &Oid, parent: &str) -> Result<Place> {
    place_with(parent, 1024, &mut |uuid| {
        let Ok(folder) = DocId::new(uuid) else {
            return Ok(None);
        };
        let Some(version) = store.version_at(rules, commit, &folder)? else {
            return Ok(None);
        };
        Ok(metadata_of(store, &version)?.map(|metadata| {
            let name = name_of(&metadata).unwrap_or_else(|| uuid.to_string());
            (name, parent_of(&metadata))
        }))
    })
}

impl Hub<'_> {
    /// `rmu hub log`: the library's history, newest first, at most
    /// `limit` entries. With a tablet (which supplies the origin URL
    /// unless [`super::HubOptions::origin`] is set), its base is reported.
    pub fn log(&self, tablet: Option<&dyn Tablet>, limit: Option<usize>) -> Result<LogReport> {
        let result = self.log_inner(tablet, limit);
        self.progress.finished();
        result
    }

    fn log_inner(&self, tablet: Option<&dyn Tablet>, limit: Option<usize>) -> Result<LogReport> {
        let library = self.open_library(tablet, false)?;
        let origin = &library.origin;
        let (store, rules) = (&origin.store, origin.rules);
        self.progress.step("Reading history");
        let index = store.history(&origin.main)?;
        let chain = index.chain(&origin.main).unwrap_or_default();
        let shown = limit.map_or(chain.len(), |limit| limit.min(chain.len()));
        let entries = chain[..shown]
            .iter()
            .map(|step| log_entry(store, rules, step.commit, &step.changes))
            .collect::<Result<Vec<_>>>()?;
        Ok(LogReport {
            repo_id: origin.config.repo_id.clone(),
            origin: origin.url.clone(),
            main: origin.main.clone(),
            tablet: library.tablet_base(None),
            entries,
            more: chain.len() - shown,
        })
    }

    /// `rmu hub log <doc>`: one document's history, newest first. `doc`
    /// is a UUID or a logical path (resolved on `main`, else on older
    /// commits; see [`resolve_doc`]).
    pub fn doc_log(&self, tablet: Option<&dyn Tablet>, reference: &str) -> Result<DocLogReport> {
        let result = self.doc_log_inner(tablet, reference);
        self.progress.finished();
        result
    }

    fn doc_log_inner(&self, tablet: Option<&dyn Tablet>, reference: &str) -> Result<DocLogReport> {
        let library = self.open_library(tablet, false)?;
        let origin = &library.origin;
        let (store, rules, main) = (&origin.store, origin.rules, &origin.main);
        self.progress.step("Reading history");
        let index = store.history(main)?;
        let doc = resolve_doc(store, rules, &index, main, reference)?;
        let mut events = doc_events(store, rules, &index, main, &doc)?;
        let entries = index
            .chain(main)
            .unwrap_or_default()
            .into_iter()
            .map(|step| -> Result<Option<DocLogEntry>> {
                let info = commit_info(store, step.commit)?;
                let mut found = events.remove(step.commit).unwrap_or_default();
                found.extend(trailer_events(&info.trailers, &doc));
                Ok((!found.is_empty()).then(|| DocLogEntry {
                    commit: step.commit.clone(),
                    kind: info.kind,
                    author: info.author,
                    time: info.time,
                    subject: info.subject,
                    events: found,
                    version: index.subtree_at(step.commit, &doc).flatten().cloned(),
                }))
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>>>()?;
        let name = entries.iter().find_map(|entry| newest_name(&entry.events));
        let at_main = present(store, rules, &doc, index.subtree_at(main, &doc).flatten())?;
        let place = at_main
            .as_ref()
            .map(|(_, metadata)| place_at(store, rules, main, &parent_of(metadata)))
            .transpose()?;
        let name = at_main
            .as_ref()
            .and_then(|(_, metadata)| name_of(metadata))
            .or(name)
            .or_else(|| {
                last_known_name(store, rules, &index, main, &doc)
                    .ok()
                    .flatten()
            });
        Ok(DocLogReport {
            repo_id: origin.config.repo_id.clone(),
            origin: origin.url.clone(),
            main: main.clone(),
            doc: doc.clone(),
            name,
            place,
            tablet: library.tablet_base(Some(&doc)),
            entries,
        })
    }
}

/// One entry of the library's history.
fn log_entry(
    store: &Store,
    rules: &MirrorRules,
    commit: &Oid,
    changes: &[DocStep<'_>],
) -> Result<LogEntry> {
    let info = commit_info(store, commit)?;
    let entry = LogEntry {
        commit: commit.clone(),
        kind: info.kind,
        author: info.author,
        time: info.time,
        device: device(&info.trailers),
        notes: match info.kind {
            CommitKind::Sync => message::body(&info.message),
            _ => Vec::new(),
        },
        forks: message::forks(&info.trailers)
            .into_iter()
            .map(|(original, copy)| ForkRecord { original, copy })
            .collect(),
        consented: message::doc_ids(message::values(&info.trailers, CONSENT))
            .into_iter()
            .collect(),
        restored: message::restored(&info.trailers)
            .into_iter()
            .map(|(copy, original, from)| RestoreRecord {
                copy,
                original,
                from,
            })
            .collect(),
        subject: info.subject,
        added: Vec::new(),
        changed: Vec::new(),
        deleted: Vec::new(),
        names: BTreeMap::new(),
    };
    let entry = changes.iter().try_fold(
        entry,
        |mut entry, (doc, before, after)| -> Result<LogEntry> {
            let before = present(store, rules, doc, *before)?;
            let after = present(store, rules, doc, *after)?;
            let (list, named) = match (&before, &after) {
                (None, Some((_, metadata))) => (&mut entry.added, metadata),
                (Some((_, metadata)), None) => (&mut entry.deleted, metadata),
                (Some(_), Some((_, metadata))) => (&mut entry.changed, metadata),
                (None, None) => return Ok(entry),
            };
            list.push((*doc).clone());
            if let Some(name) = name_of(named) {
                entry.names.insert((*doc).clone(), name);
            }
            Ok(entry)
        },
    )?;
    // Each restore's original, by the name of the version restored.
    entry
        .restored
        .clone()
        .iter()
        .try_fold(entry, |mut entry, restored| -> Result<LogEntry> {
            let version = store.version_at(rules, &restored.from, &restored.original)?;
            let name = version
                .map(|version| metadata_of(store, &version))
                .transpose()?
                .flatten()
                .and_then(|metadata| name_of(&metadata));
            if let Some(name) = name {
                entry.names.entry(restored.original.clone()).or_insert(name);
            }
            Ok(entry)
        })
}

/// The events of `doc`'s own touches on `main`'s first-parent chain, by
/// commit (view-state-only changes have none).
fn doc_events(
    store: &Store,
    rules: &MirrorRules,
    index: &HistoryIndex,
    main: &Oid,
    doc: &DocId,
) -> Result<BTreeMap<Oid, Vec<DocEvent>>> {
    index
        .doc_chain(main, doc)
        .unwrap_or_default()
        .into_iter()
        .map(|(commit, (_, before, after))| {
            let before = present(store, rules, doc, before)?;
            let after = present(store, rules, doc, after)?;
            let events = match (before, after) {
                (None, None) => Vec::new(),
                (None, Some((_, metadata))) => vec![DocEvent::Added {
                    name: name_of(&metadata),
                    place: place_at(store, rules, commit, &parent_of(&metadata))?,
                }],
                (Some(_), None) => vec![DocEvent::Deleted],
                (Some((old, old_metadata)), Some((new, new_metadata))) => {
                    let changes = version_changes(
                        rules,
                        (&old, &old_metadata),
                        (&new, &new_metadata),
                        store,
                    )?;
                    let mut events = Vec::new();
                    if changes.renamed {
                        events.push(DocEvent::Renamed {
                            from: name_of(&old_metadata),
                            to: name_of(&new_metadata),
                        });
                    }
                    if changes.moved {
                        let previous = store.commit(commit)?.parents.first().cloned();
                        let from = match &previous {
                            Some(previous) => {
                                place_at(store, rules, previous, &parent_of(&old_metadata))?
                            }
                            None => place_at(store, rules, commit, &parent_of(&old_metadata))?,
                        };
                        events.push(DocEvent::Moved {
                            from,
                            to: place_at(store, rules, commit, &parent_of(&new_metadata))?,
                        });
                    }
                    if changes.changed {
                        events.push(DocEvent::Changed);
                    }
                    events
                }
            };
            Ok((commit.clone(), events))
        })
        .filter(|result| !matches!(result, Ok((_, events)) if events.is_empty()))
        .collect()
}

/// The events a commit's trailers record about `doc`.
fn trailer_events(trailers: &[(String, String)], doc: &DocId) -> Vec<DocEvent> {
    let forks = message::forks(trailers)
        .into_iter()
        .filter_map(|(original, copy)| match (&original == doc, &copy == doc) {
            (true, _) => Some(DocEvent::ForkedInto { copy }),
            (_, true) => Some(DocEvent::ForkOf { original }),
            _ => None,
        });
    let restores = message::restored(trailers)
        .into_iter()
        .filter_map(
            |(copy, original, from)| match (&original == doc, &copy == doc) {
                (true, _) => Some(DocEvent::RestoredAs { copy, from }),
                (_, true) => Some(DocEvent::RestoredFrom { original, from }),
                _ => None,
            },
        );
    forks.chain(restores).collect()
}

/// The newest name an entry's events give.
fn newest_name(events: &[DocEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        DocEvent::Added { name, .. } => name.clone(),
        DocEvent::Renamed { to, .. } => to.clone(),
        _ => None,
    })
}

/// The name of `doc`'s newest present version on `main`'s chain.
fn last_known_name(
    store: &Store,
    rules: &MirrorRules,
    index: &HistoryIndex,
    main: &Oid,
    doc: &DocId,
) -> Result<Option<String>> {
    index
        .doc_chain(main, doc)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|(_, (_, before, after))| [after, before])
        .map(|subtree| present(store, rules, doc, subtree))
        .find_map(Result::transpose)
        .transpose()
        .map(|found| found.and_then(|(_, metadata)| name_of(&metadata)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::model::ListingEntry;
    use serde_json::json;
    use std::collections::HashMap;

    fn oid(n: u32) -> Oid {
        format!("{n:040x}").parse().unwrap()
    }

    #[test]
    fn commit_kinds_come_from_subjects_and_trailers() {
        let trailer = |key: &str| vec![(key.to_string(), "x".to_string())];
        assert_eq!(commit_kind("rmu hub: init", &[]), CommitKind::Init);
        assert_eq!(commit_kind("rmu hub: sync Desk", &[]), CommitKind::Sync);
        assert_eq!(
            commit_kind("rmu hub: snapshot of Desk", &[]),
            CommitKind::Snapshot
        );
        assert_eq!(
            commit_kind("rmu hub: restore \"x\"", &trailer(RESTORED)),
            CommitKind::Restore
        );
        assert_eq!(
            commit_kind("rmu hub: add \"x\"", &trailer(ADDED)),
            CommitKind::Add
        );
        // A subject alone does not make a restore; a hand-made commit.
        assert_eq!(
            commit_kind("rmu hub: restore \"x\"", &[]),
            CommitKind::Other
        );
        assert_eq!(commit_kind("Add a README", &[]), CommitKind::Other);
    }

    #[test]
    fn author_lines_give_name_and_time() {
        let raw = b"tree 1\nparent 2\nauthor Desk tablet <id@rmu.invalid> 1794182400 +0000\n\
                    committer rmu <rmu@rmu.invalid> 1794182401 +0000\n\nmessage\nauthor x <y> 5 +0\n";
        assert_eq!(
            author_of(raw),
            Some(("Desk tablet".to_string(), 1_794_182_400))
        );
        assert_eq!(author_of(b"tree 1\n\nauthor a <b> 1 +0\n"), None);
        assert_eq!(author_of(b"author broken\n"), None);
    }

    #[test]
    fn version_changes_ignore_view_state() {
        let rules = MirrorRules::latest();
        let json: HashMap<Oid, Value> = [
            (oid(1), json!({ "fileType": "pdf", "zoomMode": "fitWidth" })),
            (
                oid(2),
                json!({ "fileType": "pdf", "zoomMode": "fitHeight" }),
            ),
            (
                oid(3),
                json!({ "fileType": "pdf", "orientation": "landscape" }),
            ),
        ]
        .into();
        let version = |content: u32, page: Option<u32>| DocVersion {
            files: [(DocFile::Content, ListingEntry::regular(oid(content)))]
                .into_iter()
                .chain(page.map(|blob| {
                    (
                        DocFile::Page(crate::hub::model::PageId::new("p1").unwrap()),
                        ListingEntry::regular(oid(blob)),
                    )
                }))
                .collect(),
            other: BTreeMap::new(),
        };
        let metadata = |name: &str, parent: &str, opened: &str| {
            json!({ "visibleName": name, "parent": parent, "lastOpened": opened,
                    "lastModified": opened, "type": "DocumentType" })
        };
        let base = metadata("A", "", "1");
        let changes = |before: (&DocVersion, &Value), after: (&DocVersion, &Value)| {
            version_changes(rules, before, after, &json).unwrap()
        };
        let none = VersionChanges {
            renamed: false,
            moved: false,
            changed: false,
        };
        // Reading (view state, lastModified) is no change.
        assert_eq!(
            changes(
                (&version(1, None), &base),
                (&version(2, None), &metadata("A", "", "9"))
            ),
            none
        );
        assert_eq!(
            changes(
                (&version(1, None), &base),
                (&version(1, None), &metadata("B", "f", "1"))
            ),
            VersionChanges {
                renamed: true,
                moved: true,
                ..none
            }
        );
        assert_eq!(
            changes((&version(1, None), &base), (&version(3, None), &base)),
            VersionChanges {
                changed: true,
                ..none
            }
        );
        assert_eq!(
            changes((&version(1, None), &base), (&version(1, Some(7)), &base)),
            VersionChanges {
                changed: true,
                ..none
            }
        );
        let mut pinned = base.clone();
        pinned["pinned"] = json!(true);
        assert!(changes((&version(1, None), &base), (&version(1, None), &pinned)).changed);
    }

    #[test]
    fn trailers_name_forks_and_restores_of_a_document() {
        let doc = |n: u32| DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap();
        let from = oid(9);
        let trailers = message::trailers(&format!(
            "s\n\nRmu-Fork: {} {}\nRmu-Restored: {} {} {from}\nRmu-Fork: {} {}\n",
            doc(1),
            doc(2),
            doc(3),
            doc(1),
            doc(4),
            doc(5)
        ));
        assert_eq!(
            trailer_events(&trailers, &doc(1)),
            vec![
                DocEvent::ForkedInto { copy: doc(2) },
                DocEvent::RestoredAs {
                    copy: doc(3),
                    from: from.clone()
                }
            ]
        );
        assert_eq!(
            trailer_events(&trailers, &doc(3)),
            vec![DocEvent::RestoredFrom {
                original: doc(1),
                from
            }]
        );
        assert_eq!(
            trailer_events(&trailers, &doc(5)),
            vec![DocEvent::ForkOf { original: doc(4) }]
        );
        assert!(trailer_events(&trailers, &doc(6)).is_empty());
    }
}
