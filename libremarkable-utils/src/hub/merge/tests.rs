//! Merge-engine tests: every row of the per-document table, every tree
//! rule, the deletion guard, determinism, convergence, and a seeded
//! property test asserting that ink is never discarded. All fixtures are
//! fabricated; blobs live in an in-memory map.

use super::*;
use crate::git::FileMode;
use crate::hub::model::PageId;
use serde_json::json;
use sha2::{Digest, Sha256};

const DEVICE: &str = "Tablet B";
const DATE: &str = "2026-11-09";

fn u(n: u32) -> DocId {
    DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
}

fn pid(n: u32) -> String {
    format!("9d2e1c0f-0000-4000-8000-{n:012x}")
}

fn page(n: u32) -> DocFile {
    DocFile::Page(PageId::new(pid(n)).unwrap())
}

fn oid_of(bytes: &[u8]) -> Oid {
    format!("{:x}", Sha256::digest(bytes))[..40]
        .parse()
        .unwrap()
}

/// An in-memory object store.
#[derive(Default, Clone)]
struct Lib {
    blobs: BTreeMap<Oid, Vec<u8>>,
}

impl Lib {
    fn put(&mut self, bytes: &[u8]) -> ListingEntry {
        let id = oid_of(bytes);
        self.blobs.insert(id.clone(), bytes.to_vec());
        ListingEntry::regular(id)
    }

    fn bytes(&self, entry: &MergedEntry) -> Vec<u8> {
        match entry {
            MergedEntry::Existing(entry) => self.blobs[&entry.id].clone(),
            MergedEntry::New(bytes) => bytes.clone(),
        }
    }

    fn json(&self, version: &MergedVersion, file: &DocFile) -> Option<Value> {
        version
            .files
            .get(file)
            .map(|entry| serde_json::from_slice(&self.bytes(entry)).unwrap())
    }

    fn md(&self, outcome: &MergeOutcome, id: &DocId) -> Value {
        self.json(&outcome.docs[id], &DocFile::Metadata).unwrap()
    }

    fn content(&self, outcome: &MergeOutcome, id: &DocId) -> Value {
        self.json(&outcome.docs[id], &DocFile::Content).unwrap()
    }

    /// Write a merged version's new blobs and return it as a version.
    fn realise(&mut self, version: &MergedVersion) -> DocVersion {
        DocVersion {
            files: version
                .files
                .iter()
                .map(|(file, entry)| {
                    let entry = match entry {
                        MergedEntry::Existing(entry) => entry.clone(),
                        MergedEntry::New(bytes) => self.put(bytes),
                    };
                    (file.clone(), entry)
                })
                .collect(),
            other: version.other.clone(),
        }
    }

    fn realise_tree(&mut self, outcome: &MergeOutcome) -> RepoTree {
        RepoTree {
            docs: outcome
                .docs
                .iter()
                .map(|(id, version)| (id.clone(), self.realise(version)))
                .collect(),
            extra: outcome.extra.clone(),
        }
    }
}

/// A fabricated document, folder or template, before it becomes blobs.
#[derive(Debug, Clone, PartialEq)]
struct D {
    md: Value,
    content: Option<Value>,
    files: Vec<(DocFile, Vec<u8>)>,
}

fn page_entry(n: u32, idx: &str, redir: Option<u64>) -> Value {
    let mut entry = json!({
        "id": pid(n),
        "idx": { "timestamp": "1:2", "value": idx },
    });
    if let Some(redir) = redir {
        entry["redir"] = json!({ "timestamp": "1:2", "value": redir });
    }
    entry
}

/// `"ba"`, `"bb"`, ... fractional indices for page positions.
fn idx(position: usize) -> String {
    format!("b{}", (b'a' + position as u8) as char)
}

impl D {
    fn metadata(name: &str, parent: &str, kind: &str) -> Value {
        json!({
            "createdTime": "1767225600000",
            "lastModified": "1767312000000",
            "lastOpened": "0",
            "lastOpenedPage": 0,
            "new": false,
            "parent": parent,
            "pinned": false,
            "source": "",
            "type": kind,
            "visibleName": name,
        })
    }

    fn folder(name: &str, parent: &str) -> Self {
        Self {
            md: Self::metadata(name, parent, "CollectionType"),
            content: Some(json!([])),
            files: Vec::new(),
        }
    }

    /// A PDF whose page list xochitl already materialised (pages 1..=n).
    fn pdf(name: &str, parent: &str, pages: u32) -> Self {
        let entries: Vec<Value> = (0..pages)
            .map(|i| page_entry(i + 1, &idx(i as usize), Some(u64::from(i))))
            .collect();
        Self {
            md: Self::metadata(name, parent, "DocumentType"),
            content: Some(json!({
                "cPages": {
                    "lastOpened": { "timestamp": "1:3", "value": pid(1) },
                    "original": { "timestamp": "1:1", "value": pages },
                    "pages": entries,
                    "uuids": [{ "first": "0f0e0d0c-0000-4000-8000-00000000a001", "second": 1 }],
                },
                "coverPageNumber": 0,
                "customZoomCenterX": 0,
                "customZoomScale": 1,
                "documentMetadata": {},
                "extraMetadata": { "LastPen": "Ballpointv2" },
                "fileType": "pdf",
                "fontName": "",
                "formatVersion": 2,
                "lineHeight": -1,
                "margins": 100,
                "orientation": "portrait",
                "pageCount": pages,
                "pageTags": [],
                "sizeInBytes": "1000",
                "tags": [],
                "textAlignment": "left",
                "textScale": 1,
                "zoomMode": "bestFit",
            })),
            files: vec![(DocFile::PayloadPdf, format!("%PDF {name}").into_bytes())],
        }
    }

    /// A PDF as `rmu upload` writes it: the skeleton, never opened.
    fn skeleton(name: &str, parent: &str) -> Self {
        Self {
            md: Self::metadata(name, parent, "DocumentType"),
            content: Some(serde_json::from_str(&xochitl::document_content_json("pdf")).unwrap()),
            files: vec![(DocFile::PayloadPdf, format!("%PDF {name}").into_bytes())],
        }
    }

    /// A notebook with pages `1..=n`, each inked with `ink`.
    fn notebook(name: &str, parent: &str, pages: u32, ink: &str) -> Self {
        let entries: Vec<Value> = (0..pages)
            .map(|i| {
                let mut entry = page_entry(i + 1, &idx(i as usize), None);
                entry["template"] = json!({ "timestamp": "1:4", "value": "Blank" });
                entry["modifed"] = json!("1767312000000");
                entry
            })
            .collect();
        Self {
            md: Self::metadata(name, parent, "DocumentType"),
            content: Some(json!({
                "cPages": { "pages": entries, "original": { "timestamp": "0:0", "value": -1 } },
                "fileType": "notebook",
                "formatVersion": 2,
                "pageCount": pages,
                "tags": [],
            })),
            files: (0..pages)
                .map(|i| (page(i + 1), format!("ink {ink} {}", i + 1).into_bytes()))
                .collect(),
        }
    }

    fn set(mut self, key: &str, value: Value) -> Self {
        self.md[key] = value;
        self
    }

    fn rename(self, name: &str) -> Self {
        self.set("visibleName", json!(name))
    }

    fn move_to(self, parent: &str) -> Self {
        self.set("parent", json!(parent))
    }

    fn trash(self) -> Self {
        self.move_to(TRASH_PARENT)
    }

    /// Set a value inside `.content`; numeric path segments index arrays.
    fn content_set(mut self, path: &[&str], value: Value) -> Self {
        fn step<'v>(node: &'v mut Value, key: &str) -> &'v mut Value {
            match (key.parse::<usize>(), node) {
                (Ok(index), Value::Array(items)) => &mut items[index],
                (_, node) => &mut node[key],
            }
        }
        let content = self.content.as_mut().unwrap();
        let (last, parents) = path.split_last().unwrap();
        let target = parents.iter().fold(content, |node, key| step(node, key));
        *step(target, last) = value;
        self
    }

    /// Ink page `n` with `bytes`: the `.rm` file, and a page entry with
    /// `modifed` (added at the end if the page is new).
    fn ink(mut self, n: u32, bytes: &str) -> Self {
        let id = pid(n);
        let pages = self.content.as_mut().unwrap()["cPages"]["pages"]
            .as_array_mut()
            .unwrap();
        match pages.iter_mut().find(|entry| entry["id"] == json!(id)) {
            Some(entry) => entry["modifed"] = json!(format!("17673{n:08}")),
            None => {
                let mut entry = page_entry(n, &format!("z{n:04}"), None);
                entry["modifed"] = json!(format!("17673{n:08}"));
                pages.push(entry);
            }
        }
        self.files.retain(|(file, _)| *file != page(n));
        self.files.push((page(n), bytes.as_bytes().to_vec()));
        self
    }

    /// Remove page `n` (entry and ink).
    fn drop_page(mut self, n: u32) -> Self {
        let id = pid(n);
        self.content.as_mut().unwrap()["cPages"]["pages"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["id"] != json!(id));
        self.files.retain(|(file, _)| *file != page(n));
        self
    }

    /// Reading only: view state and derived fields move.
    fn scroll(mut self, tick: u32) -> Self {
        self.md["lastOpened"] = json!(format!("17674{tick:08}"));
        self.md["lastOpenedPage"] = json!(tick);
        self.md["lastModified"] = json!(format!("17675{tick:08}"));
        if let Some(Value::Object(content)) = self.content.as_mut() {
            if let Some(Value::Object(cpages)) = content.get_mut("cPages") {
                cpages.insert(
                    "lastOpened".into(),
                    json!({ "timestamp": format!("1:{tick}"), "value": pid(1) }),
                );
                if let Some(Value::Array(pages)) = cpages.get_mut("pages")
                    && let Some(first) = pages.first_mut()
                {
                    first["scrollTime"] = json!({ "timestamp": "1:9", "value": tick });
                    first["verticalScroll"] = json!({ "timestamp": "1:9", "value": tick });
                }
            }
            content.insert("zoomMode".into(), json!(format!("zoom{tick}")));
            content.insert("sizeInBytes".into(), json!(format!("{tick}")));
        }
        self
    }

    fn tags(self, tags: &[&str]) -> Self {
        self.content_set(&["tags"], json!(tags))
    }

    fn build(&self, lib: &mut Lib) -> DocVersion {
        let json_bytes = |value: &Value| serde_json::to_vec_pretty(value).unwrap();
        let files = std::iter::once((DocFile::Metadata, json_bytes(&self.md)))
            .chain(
                self.content
                    .iter()
                    .map(|content| (DocFile::Content, json_bytes(content))),
            )
            .chain(self.files.iter().cloned())
            .map(|(file, bytes)| (file, lib.put(&bytes)))
            .collect();
        DocVersion {
            files,
            other: BTreeMap::new(),
        }
    }
}

fn tree(lib: &mut Lib, docs: &[(u32, &D)]) -> RepoTree {
    RepoTree {
        docs: docs.iter().map(|(n, d)| (u(*n), d.build(lib))).collect(),
        extra: BTreeMap::new(),
    }
}

struct Run<'a> {
    history: &'a dyn History,
    consent: BTreeSet<DocId>,
    allow: bool,
    thresholds: GuardThresholds,
}

impl Default for Run<'_> {
    fn default() -> Self {
        Self {
            history: &NoHistory,
            consent: BTreeSet::new(),
            allow: false,
            thresholds: GuardThresholds::default(),
        }
    }
}

impl Run<'_> {
    fn go(
        &self,
        lib: &Lib,
        base: &RepoTree,
        ours: &RepoTree,
        theirs: &RepoTree,
    ) -> Result<MergeOutcome> {
        let device = DeviceName::new(DEVICE).unwrap();
        let params = MergeParams {
            rules: MirrorRules::latest(),
            device_name: &device,
            date: DATE,
            allow_deletions: self.allow,
            thresholds: self.thresholds,
            consented_deletions: &self.consent,
        };
        merge(
            &MergeInput { base, ours, theirs },
            &lib.blobs,
            self.history,
            &params,
        )
    }
}

fn run(lib: &Lib, base: &RepoTree, ours: &RepoTree, theirs: &RepoTree) -> MergeOutcome {
    Run::default().go(lib, base, ours, theirs).unwrap()
}

/// base, ours and theirs each as `(uuid, doc)` lists.
type Docs<'a> = &'a [(u32, &'a D)];

fn three(lib: &mut Lib, base: Docs, ours: Docs, theirs: Docs) -> (RepoTree, RepoTree, RepoTree) {
    (tree(lib, base), tree(lib, ours), tree(lib, theirs))
}

fn notes_of(outcome: &MergeOutcome, id: &DocId) -> Vec<NoteKind> {
    outcome
        .notes
        .iter()
        .filter(|note| note.doc == *id)
        .map(|note| note.kind.clone())
        .collect()
}

fn page_blobs(version: &MergedVersion) -> BTreeSet<Oid> {
    version
        .files
        .iter()
        .filter(|(file, _)| file.kind() == FileKind::Page)
        .filter_map(|(_, entry)| match entry {
            MergedEntry::Existing(entry) => Some(entry.id.clone()),
            MergedEntry::New(_) => None,
        })
        .collect()
}

fn version_of(lib: &mut Lib, d: &D) -> MergedVersion {
    MergedVersion::existing(&d.build(lib))
}

// ---- per-document table ----------------------------------------------------

#[test]
fn unchanged_everywhere_is_unchanged() {
    let mut lib = Lib::default();
    let doc = D::pdf("Paper", "", 2);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &doc)], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Unchanged);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &doc));
    assert!(outcome.notes.is_empty() && outcome.forks.is_empty());
    assert!(!outcome.guard.needs_consent);
}

#[test]
fn changed_on_one_side_takes_that_side() {
    let mut lib = Lib::default();
    let base = D::notebook("Notes", "", 2, "base");
    let renamed = base.clone().rename("Renamed");
    let inked = base.clone().ink(2, "theirs ink");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &renamed)], &[(1, &base)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Changed(Side::Ours));
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &renamed));

    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &base)], &[(1, &inked)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Changed(Side::Theirs)
    );
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &inked));

    // A change on one side beats view-state churn on the other.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &base)],
        &[(1, &base.clone().scroll(5))],
        &[(1, &inked)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Changed(Side::Theirs)
    );
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &inked));
    let (b, o, t) = three(
        &mut lib,
        &[(1, &base)],
        &[(1, &renamed)],
        &[(1, &base.clone().scroll(7))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Changed(Side::Ours));
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &renamed));
}

#[test]
fn view_state_alone_can_be_reported_as_unchanged() {
    let mut lib = Lib::default();
    let base = D::notebook("Notes", "", 2, "base");
    let other = D::notebook("Other", "", 1, "other");
    let read = base.clone().scroll(5);
    // Decided by blob ids, without reading: a change on one side, or the
    // same bytes on both. The caller judges them as view state only.
    [
        (&read, &base, Resolution::Changed(Side::Ours)),
        (&base, &read, Resolution::Changed(Side::Theirs)),
        (&read, &read, Resolution::Identical),
    ]
    .into_iter()
    .for_each(|(ours, theirs, decided)| {
        let renamed = other.clone().rename("Renamed");
        let (b, o, t) = three(
            &mut lib,
            &[(1, &base), (2, &other)],
            &[(1, ours), (2, &renamed)],
            &[(1, theirs), (2, &other)],
        );
        let outcome = run(&lib, &b, &o, &t);
        assert_eq!(outcome.resolutions[&u(1)], decided);
        assert_eq!(outcome.summary().unchanged, 0);
        let summary = outcome.summary_with(|doc| Ok(*doc == u(1))).unwrap();
        assert_eq!(
            summary,
            Summary {
                unchanged: 1,
                changed_ours: 1,
                ..Summary::default()
            }
        );
    });
    // Other resolutions are never asked about.
    let (b, o, t) = three(&mut lib, &[], &[(1, &base)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    let summary = outcome
        .summary_with(|_| panic!("asked about an added document"))
        .unwrap();
    assert_eq!(summary.added, 1);
}

#[test]
fn identical_change_is_taken_once() {
    let mut lib = Lib::default();
    let base = D::notebook("Notes", "", 1, "base");
    let both = base.clone().ink(2, "same ink").rename("Both");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &both)], &[(1, &both)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Identical);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &both));
    assert!(outcome.forks.is_empty());

    // Same change, different view state: still identical, origin's bytes.
    let ours = both.clone().scroll(3);
    let theirs = both.clone().scroll(4);
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Identical);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &theirs));
}

#[test]
fn new_on_one_side_is_added() {
    let mut lib = Lib::default();
    let doc = D::pdf("New", "", 1);
    let (b, o, t) = three(&mut lib, &[], &[(1, &doc)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Added(Side::Ours));
    assert!(outcome.docs[&u(1)].is_present());
    let (b, o, t) = three(&mut lib, &[], &[], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Added(Side::Theirs));
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &doc));
}

#[test]
fn new_on_both_identical_is_taken_once() {
    let mut lib = Lib::default();
    let doc = D::notebook("Shared", "", 2, "cloud");
    let (b, o, t) = three(&mut lib, &[], &[(1, &doc)], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::AddedBoth);
    // Semantically identical (view state differs): origin's version.
    let ours = doc.clone().scroll(2);
    let (b, o, t) = three(&mut lib, &[], &[(1, &ours)], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::AddedBoth);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &doc));
    assert!(outcome.forks.is_empty());
}

#[test]
fn new_on_both_with_different_metadata_merges_against_an_empty_base() {
    let mut lib = Lib::default();
    let theirs = D::notebook("Origin name", "", 1, "cloud");
    let ours = theirs
        .clone()
        .rename("Tablet name")
        .set("pinned", json!(true));
    let (b, o, t) = three(&mut lib, &[], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    let md = lib.md(&outcome, &u(1));
    // Against an empty base every difference is a conflict: origin wins.
    assert_eq!(md["visibleName"], json!("Origin name"));
    assert_eq!(md["pinned"], json!(false));
    assert!(
        notes_of(&outcome, &u(1)).contains(&NoteKind::MetadataConflict {
            field: "visibleName".into()
        })
    );
    assert!(outcome.forks.is_empty());
}

#[test]
fn new_on_both_with_different_ink_forks() {
    let mut lib = Lib::default();
    let theirs = D::notebook("Shared", "", 1, "origin");
    let ours = D::notebook("Shared", "", 1, "tablet");
    let (b, o, t) = three(&mut lib, &[], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.forks.len(), 1);
    let copy = &outcome.forks[0].copy;
    assert_eq!(
        page_blobs(&outcome.docs[&u(1)]),
        page_blobs(&version_of(&mut lib, &theirs))
    );
    assert_eq!(
        page_blobs(&outcome.docs[copy]),
        page_blobs(&version_of(&mut lib, &ours))
    );
}

#[test]
fn deleted_on_one_side_and_unchanged_on_the_other_is_deleted() {
    let mut lib = Lib::default();
    let doc = D::pdf("Gone", "", 1);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Ours))
    );
    assert!(!outcome.docs.contains_key(&u(1)));
    assert_eq!(outcome.guard.from_origin, BTreeSet::from([u(1)]));
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &doc)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Theirs))
    );
    assert!(!outcome.docs.contains_key(&u(1)));
    assert_eq!(outcome.guard.from_tablet, BTreeSet::from([u(1)]));
}

#[test]
fn view_state_churn_never_resurrects_a_deleted_document() {
    let mut lib = Lib::default();
    let doc = D::pdf("Gone", "", 2);
    let read = doc.clone().scroll(9);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[], &[(1, &read)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Ours))
    );
    assert!(!outcome.docs.contains_key(&u(1)));
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &read)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Theirs))
    );
    assert!(!outcome.docs.contains_key(&u(1)));
}

#[test]
fn deleted_on_one_side_and_changed_on_the_other_is_kept() {
    let mut lib = Lib::default();
    let doc = D::notebook("Keep", "", 1, "base");
    let inked = doc.clone().ink(2, "new ink");
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[], &[(1, &inked)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Kept(Side::Theirs));
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &inked));
    assert_eq!(
        notes_of(&outcome, &u(1)),
        [NoteKind::KeptOverDeletion {
            deleted_by: Side::Ours
        }]
    );
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &inked)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Kept(Side::Ours));
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &inked));
    // A rename is a change too.
    let renamed = doc.clone().rename("Still wanted");
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &renamed)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Kept(Side::Ours));
}

#[test]
fn trashed_on_one_side_and_emptied_on_the_other_is_deleted_quietly() {
    let mut lib = Lib::default();
    let doc = D::pdf("Old", "", 1);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &doc.clone().trash())], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Theirs))
    );
    assert!(outcome.notes.is_empty());
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[], &[(1, &doc.clone().trash())]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Ours))
    );
    // Trashed and renamed: the rename is a change, so it is kept.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc)],
        &[(1, &doc.clone().trash().rename("Renamed"))],
        &[],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Kept(Side::Ours));
}

#[test]
fn deleted_on_both_is_deleted() {
    let mut lib = Lib::default();
    let doc = D::pdf("Gone", "", 1);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Deleted(None));
    assert!(outcome.docs.is_empty());
    // Origin already lacks it: not a deletion from origin.
    assert!(outcome.guard.from_origin.is_empty() && outcome.guard.from_tablet.is_empty());
}

// ---- metadata merge --------------------------------------------------------

#[test]
fn metadata_merges_per_field() {
    let mut lib = Lib::default();
    let folder = D::folder("Folder", "");
    let doc = D::notebook("Notes", "", 1, "base");
    let ours = doc.clone().rename("Renamed on tablet");
    let theirs = doc
        .clone()
        .move_to(u(9).as_str())
        .set("pinned", json!(true));
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (9, &folder)],
        &[(1, &ours), (9, &folder)],
        &[(1, &theirs), (9, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    let md = lib.md(&outcome, &u(1));
    assert_eq!(md["visibleName"], json!("Renamed on tablet"));
    assert_eq!(md["parent"], json!(u(9).as_str()));
    assert_eq!(md["pinned"], json!(true));
    assert!(outcome.notes.is_empty());
    assert!(matches!(
        outcome.docs[&u(1)].files[&DocFile::Metadata],
        MergedEntry::New(_)
    ));
}

#[test]
fn a_rename_on_one_side_and_ink_on_the_other_merge_without_a_fork() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let renamed = doc.clone().rename("Renamed");
    let inked = doc.clone().ink(2, "ink");
    [(&renamed, &inked), (&inked, &renamed)]
        .into_iter()
        .for_each(|(ours, theirs)| {
            let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, ours)], &[(1, theirs)]);
            let outcome = run(&lib, &b, &o, &t);
            assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
            assert!(outcome.forks.is_empty());
            assert_eq!(lib.md(&outcome, &u(1))["visibleName"], json!("Renamed"));
            assert_eq!(
                page_blobs(&outcome.docs[&u(1)]),
                page_blobs(&version_of(&mut lib, &inked))
            );
        });
}

#[test]
fn concurrent_renames_take_origins_name_with_a_note() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc)],
        &[(1, &doc.clone().rename("Tablet"))],
        &[(1, &doc.clone().rename("Origin"))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["visibleName"], json!("Origin"));
    assert_eq!(
        notes_of(&outcome, &u(1)),
        [NoteKind::MetadataConflict {
            field: "visibleName".into()
        }]
    );
    // Origin's blob is reused when the merge equals it.
    assert_eq!(
        outcome.docs[&u(1)],
        version_of(&mut lib, &doc.clone().rename("Origin"))
    );
}

#[test]
fn view_state_conflicts_are_not_reported() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let ours = doc.clone().rename("Renamed").scroll(1);
    let theirs = doc.clone().set("pinned", json!(true)).scroll(2);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    assert!(notes_of(&outcome, &u(1)).is_empty());
    let md = lib.md(&outcome, &u(1));
    assert_eq!(md["lastOpened"], theirs.md["lastOpened"]);
}

#[test]
fn last_modified_is_the_later_of_both_sides_parsed_leniently() {
    let rules = MirrorRules::latest();
    let base = json!({ "lastModified": "100", "visibleName": "a" });
    let ours = json!({ "lastModified": 300, "visibleName": "b" });
    let theirs = json!({ "lastModified": "200", "visibleName": "a" });
    let (merged, conflicts) = merge_metadata(rules, Some(&base), &ours, &theirs);
    assert_eq!(merged["lastModified"], json!(300));
    assert_eq!(merged["visibleName"], json!("b"));
    assert!(conflicts.is_empty());
    let (merged, _) = merge_metadata(rules, Some(&base), &theirs, &ours);
    assert_eq!(merged["lastModified"], json!(300));
    // Missing on one side: the other's; unparsable counts as 0.
    let (merged, _) = merge_metadata(rules, None, &json!({}), &json!({ "lastModified": "5" }));
    assert_eq!(merged["lastModified"], json!("5"));
    let (merged, _) = merge_metadata(
        rules,
        None,
        &json!({ "lastModified": "x" }),
        &json!({ "lastModified": "1" }),
    );
    assert_eq!(merged["lastModified"], json!("1"));
}

#[test]
fn metadata_merge_preserves_unknown_fields_and_removals() {
    let rules = MirrorRules::latest();
    let base = json!({ "visibleName": "a", "gone": 1, "kept": 2 });
    let ours = json!({ "visibleName": "a", "kept": 2, "futureFlag": { "x": [1] } });
    let theirs = json!({ "visibleName": "b", "gone": 1, "kept": 2, "otherFuture": true });
    let (merged, conflicts) = merge_metadata(rules, Some(&base), &ours, &theirs);
    assert_eq!(
        merged,
        json!({ "visibleName": "b", "kept": 2, "futureFlag": { "x": [1] }, "otherFuture": true })
    );
    assert!(conflicts.is_empty());
    // Non-object metadata on a side: origin's, untouched.
    let (merged, _) = merge_metadata(rules, None, &json!([]), &theirs);
    assert_eq!(merged, theirs);
}

// ---- forks -----------------------------------------------------------------

#[test]
fn concurrent_ink_forks_and_origin_keeps_the_uuid() {
    let mut lib = Lib::default();
    let folder = D::folder("Folder", "");
    let doc = D::notebook("Notes", u(9).as_str(), 2, "base");
    let ours = doc.clone().ink(2, "tablet ink").set("pinned", json!(true));
    let theirs = doc.clone().ink(2, "origin ink").rename("Notes v2");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (9, &folder)],
        &[(1, &ours), (9, &folder)],
        &[(1, &theirs), (9, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    let [fork] = outcome.forks.as_slice() else {
        panic!("one fork expected: {:?}", outcome.forks);
    };
    assert_eq!(fork.original, u(1));
    assert_eq!(fork.name, format!("Notes v2 (conflict {DEVICE} {DATE})"));
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Forked {
            copy: fork.copy.clone()
        }
    );
    assert_eq!(
        outcome.resolutions[&fork.copy],
        Resolution::ForkCopy { original: u(1) }
    );
    // The original: origin's fork class, both sides' metadata changes.
    let original = &outcome.docs[&u(1)];
    assert_eq!(
        page_blobs(original),
        page_blobs(&version_of(&mut lib, &theirs))
    );
    assert_eq!(
        lib.content(&outcome, &u(1)),
        theirs.content.clone().unwrap()
    );
    let md = lib.md(&outcome, &u(1));
    assert_eq!(md["visibleName"], json!("Notes v2"));
    assert_eq!(md["pinned"], json!(true));
    // The copy: the tablet's fork class and metadata, renamed, same folder.
    let copy = &outcome.docs[&fork.copy];
    assert_eq!(page_blobs(copy), page_blobs(&version_of(&mut lib, &ours)));
    assert_eq!(
        lib.content(&outcome, &fork.copy),
        ours.content.clone().unwrap()
    );
    let copy_md = lib.md(&outcome, &fork.copy);
    assert_eq!(copy_md["visibleName"], json!(fork.name));
    assert_eq!(copy_md["parent"], json!(u(9).as_str()));
    assert_eq!(copy_md["pinned"], json!(true));
    assert_eq!(copy_md["type"], json!("DocumentType"));
    assert!(notes_of(&outcome, &u(1)).contains(&NoteKind::Forked {
        copy: fork.copy.clone()
    }));
    // A fork adds a document and deletes nothing.
    assert!(outcome.guard.from_origin.is_empty() && outcome.guard.from_tablet.is_empty());
}

#[test]
fn fork_ids_are_deterministic_and_ignore_view_state() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let theirs = doc.clone().ink(1, "origin");
    let ours = doc.clone().ink(1, "tablet");
    let fork_copy = |lib: &mut Lib, ours: &D| {
        let (b, o, t) = three(lib, &[(1, &doc)], &[(1, ours)], &[(1, &theirs)]);
        run(lib, &b, &o, &t).forks[0].copy.clone()
    };
    let first = fork_copy(&mut lib, &ours);
    assert_eq!(fork_copy(&mut lib, &ours), first);
    assert_eq!(fork_copy(&mut lib, &ours.clone().scroll(42)), first);
    assert_eq!(
        fork_copy(&mut lib, &ours.clone().rename("Other name")),
        first
    );
    assert_ne!(fork_copy(&mut lib, &doc.clone().ink(1, "different")), first);
    // Pinned: the UUIDv5 recipe must never change.
    let key = ForkKey {
        content: Some(json!({ "b": 1, "a": [2] })),
        files: BTreeMap::from([("pages/x.rm".to_string(), (0o100644, "f".repeat(40)))]),
    };
    assert_eq!(
        key.canonical(),
        format!(
            "content {{\"a\":[2],\"b\":1}}\npages/x.rm 100644 {}\n",
            "f".repeat(40)
        )
    );
    assert_eq!(
        fork_id(&u(1), &key),
        DocId::new(
            Uuid::new_v5(
                &FORK_NAMESPACE,
                format!("{}\n{}", u(1), key.canonical()).as_bytes()
            )
            .to_string()
        )
        .unwrap()
    );
}

#[test]
fn fork_names_get_a_short_id_suffix_when_taken() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let squatter = D::pdf(&format!("Notes (conflict {DEVICE} {DATE})"), "", 1);
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &squatter)],
        &[(1, &doc.clone().ink(1, "tablet")), (2, &squatter)],
        &[(1, &doc.clone().ink(1, "origin")), (2, &squatter)],
    );
    let outcome = run(&lib, &b, &o, &t);
    let fork = &outcome.forks[0];
    assert_eq!(
        fork.name,
        format!(
            "Notes (conflict {DEVICE} {DATE} {})",
            &fork.copy.as_str()[..8]
        )
    );
    // The same name in another folder is not taken.
    let elsewhere = squatter.clone().move_to(TRASH_PARENT);
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &elsewhere)],
        &[(1, &doc.clone().ink(1, "tablet")), (2, &elsewhere)],
        &[(1, &doc.clone().ink(1, "origin")), (2, &elsewhere)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.forks[0].name,
        format!("Notes (conflict {DEVICE} {DATE})")
    );
}

#[test]
fn two_forks_with_one_name_in_one_folder_are_told_apart() {
    let mut lib = Lib::default();
    let a = D::notebook("Same", "", 1, "a");
    let b_doc = D::notebook("Same", "", 1, "b");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &a), (2, &b_doc)],
        &[
            (1, &a.clone().ink(1, "ta")),
            (2, &b_doc.clone().ink(1, "tb")),
        ],
        &[
            (1, &a.clone().ink(1, "oa")),
            (2, &b_doc.clone().ink(1, "ob")),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    let mut forks = outcome.forks.clone();
    assert_eq!(forks.len(), 2);
    forks.sort_by(|x, y| x.copy.cmp(&y.copy));
    assert_eq!(forks, outcome.forks, "forks are named in UUID order");
    assert_eq!(forks[0].name, format!("Same (conflict {DEVICE} {DATE})"));
    assert_eq!(
        forks[1].name,
        format!(
            "Same (conflict {DEVICE} {DATE} {})",
            &forks[1].copy.as_str()[..8]
        )
    );
}

#[test]
fn a_fork_already_in_origin_is_not_duplicated() {
    // A merge with a fork was pushed, but this tablet never recorded the
    // new base (e.g. a lost push acknowledgement handled as "not landed").
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let ours = doc.clone().ink(1, "tablet");
    let theirs = doc.clone().ink(1, "origin");
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &ours)], &[(1, &theirs)]);
    let first = run(&lib, &b, &o, &t);
    let pushed = lib.realise_tree(&first);
    let again = run(&lib, &b, &o, &pushed);
    let copy = first.forks[0].copy.clone();
    assert!(again.forks.is_empty());
    assert_eq!(
        notes_of(&again, &u(1)),
        [NoteKind::ForkAlreadyPresent { copy: copy.clone() }]
    );
    assert_eq!(again.resolutions[&copy], Resolution::Added(Side::Theirs));
    assert_eq!(again.docs.len(), 2);
    assert_eq!(
        page_blobs(&again.docs[&copy]),
        page_blobs(&first.docs[&copy])
    );
}

// ---- units, derived fields, sets, opaque -----------------------------------

fn epub(name: &str) -> D {
    D {
        content: Some(json!({
            "cPages": { "pages": [page_entry(1, "ba", Some(0))] },
            "fileType": "epub",
            "formatVersion": 2,
            "fontName": "Serif",
            "lineHeight": 100,
            "margins": 50,
            "textAlignment": "justify",
            "textScale": 1,
            "orientation": "portrait",
            "pageCount": 1,
        })),
        files: vec![
            (DocFile::PayloadEpub, b"PK epub".to_vec()),
            (DocFile::PayloadPdf, b"%PDF rendition 1".to_vec()),
        ],
        ..D::pdf(name, "", 0)
    }
}

#[test]
fn an_epub_rendition_and_its_layout_move_together() {
    let mut lib = Lib::default();
    let base = epub("Book");
    let relaid = D {
        files: vec![
            (DocFile::PayloadEpub, b"PK epub".to_vec()),
            (DocFile::PayloadPdf, b"%PDF rendition 2".to_vec()),
        ],
        ..base.clone().content_set(&["textScale"], json!(1.5))
    };
    // Re-layout on the tablet, rename in origin: the tablet's layout and
    // rendition, origin's name.
    let renamed = base.clone().rename("Book (annotated)");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &relaid)], &[(1, &renamed)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    let merged = &outcome.docs[&u(1)];
    assert_eq!(
        lib.bytes(&merged.files[&DocFile::PayloadPdf]),
        b"%PDF rendition 2"
    );
    assert_eq!(lib.content(&outcome, &u(1))["textScale"], json!(1.5));
    assert_eq!(
        lib.md(&outcome, &u(1))["visibleName"],
        json!("Book (annotated)")
    );
    // Re-layout on the tablet, ink positioned on the old layout in
    // origin: never mixed, the document forks.
    let inked = base.clone().ink(1, "origin highlight");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &relaid)], &[(1, &inked)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.forks.len(), 1);
    let original = &outcome.docs[&u(1)];
    assert_eq!(
        lib.bytes(&original.files[&DocFile::PayloadPdf]),
        b"%PDF rendition 1"
    );
    assert_eq!(lib.content(&outcome, &u(1))["textScale"], json!(1));
    let copy = &outcome.docs[&outcome.forks[0].copy];
    assert_eq!(
        lib.bytes(&copy.files[&DocFile::PayloadPdf]),
        b"%PDF rendition 2"
    );
}

#[test]
fn derived_fields_come_from_the_side_whose_fork_class_wins() {
    let mut lib = Lib::default();
    let base = D::notebook("Notes", "", 1, "base");
    let ours = base
        .clone()
        .ink(2, "tablet")
        .content_set(&["pageCount"], json!(2))
        .content_set(&["sizeInBytes"], json!("2000"));
    let theirs = base
        .clone()
        .rename("Renamed")
        .content_set(&["sizeInBytes"], json!("1500"));
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    assert_eq!(lib.content(&outcome, &u(1)), ours.content.clone().unwrap());
    assert_eq!(
        outcome.docs[&u(1)].files[&DocFile::Content],
        MergedEntry::Existing(
            lib.put(&serde_json::to_vec_pretty(ours.content.as_ref().unwrap()).unwrap())
        )
    );
}

#[test]
fn tags_and_page_tags_merge_as_sets() {
    let mut lib = Lib::default();
    let base = D::notebook("Notes", "", 1, "base")
        .tags(&["keep", "drop"])
        .content_set(&["pageTags"], json!([{ "name": "p", "pageId": pid(1) }]));
    let ours = base.clone().tags(&["keep", "tablet"]).ink(2, "tablet ink");
    let theirs = base.clone().tags(&["keep", "drop", "origin"]).content_set(
        &["pageTags"],
        json!([{ "name": "p", "pageId": pid(1) }, { "name": "q", "pageId": pid(1) }]),
    );
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    assert!(outcome.forks.is_empty(), "tag changes never fork");
    let content = lib.content(&outcome, &u(1));
    assert_eq!(content["tags"], json!(["keep", "origin", "tablet"]));
    assert_eq!(
        content["pageTags"],
        json!([{ "name": "p", "pageId": pid(1) }, { "name": "q", "pageId": pid(1) }])
    );
    // The fork class is the tablet's (it inked).
    assert_eq!(page_blobs(&outcome.docs[&u(1)]).len(), 2);
}

#[test]
fn set_merge_rules() {
    let v = |xs: &[&str]| xs.iter().map(|x| json!(x)).collect::<Vec<_>>();
    assert_eq!(
        merge_set(&v(&["a"]), &v(&["a", "o"]), &v(&["a", "t"])),
        v(&["a", "t", "o"])
    );
    // Removal on one side wins unless the other side added it back.
    assert_eq!(
        merge_set(&v(&["a", "b"]), &v(&["b"]), &v(&["a", "b"])),
        v(&["b"])
    );
    assert_eq!(merge_set(&v(&["a"]), &v(&[]), &v(&[])), v(&[]));
    assert_eq!(merge_set(&v(&[]), &v(&["x"]), &v(&["x"])), v(&["x"]));
    // Missing tags stay missing; an empty result is written only where
    // the winner had the field.
    let skeleton = json!({ "fileType": "pdf" });
    assert_eq!(merge_sets(&skeleton, None, None, None), skeleton);
    let with = json!({ "tags": ["a"] });
    assert_eq!(
        merge_sets(
            &skeleton,
            Some(&with),
            Some(&json!({ "tags": [] })),
            Some(&skeleton)
        ),
        skeleton
    );
    assert_eq!(
        merge_sets(
            &json!({ "tags": ["a"] }),
            Some(&with),
            Some(&json!({})),
            Some(&with)
        ),
        json!({ "tags": [] })
    );
}

#[test]
fn tag_only_changes_on_both_sides_merge_without_a_fork() {
    let mut lib = Lib::default();
    let base = D::pdf("Paper", "", 2);
    let (b, o, t) = three(
        &mut lib,
        &[(1, &base)],
        &[(1, &base.clone().tags(&["a"]))],
        &[(1, &base.clone().tags(&["b"]))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Merged);
    assert!(outcome.forks.is_empty());
    assert_eq!(lib.content(&outcome, &u(1))["tags"], json!(["b", "a"]));
}

#[test]
fn unknown_content_formats_are_compared_whole_except_view_state() {
    let mut lib = Lib::default();
    let old = D::pdf("Old firmware", "", 1).content_set(&["formatVersion"], json!(1));
    // Reading never counts, even for an unknown format: the enumerated
    // view-state paths are stripped from it too.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &old.clone().content_set(&["zoomMode"], json!("a")))],
        &[(1, &old.clone().content_set(&["zoomMode"], json!("b")))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Unchanged);
    // Read on one tablet, deleted on the other: deleted.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &old.clone().content_set(&["zoomMode"], json!("a")))],
        &[],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Theirs))
    );
    // Any other field changed on both sides: opaque, so it forks.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &old.clone().content_set(&["fontName"], json!("a")))],
        &[(1, &old.clone().content_set(&["fontName"], json!("b")))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.forks.len(), 1);
    assert!(notes_of(&outcome, &u(1)).contains(&NoteKind::Opaque));
    // One side only: that side's version, no fork.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &old.clone().content_set(&["fontName"], json!("a")))],
        &[(1, &old)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Changed(Side::Ours));
    // Tags of an opaque document are not merged as sets.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &old.clone().tags(&["a"]))],
        &[(1, &old.clone().tags(&["b"]))],
    );
    assert_eq!(run(&lib, &b, &o, &t).forks.len(), 1);
    assert!(content_known(&json!([])));
    assert!(content_known(&json!({ "formatVersion": 2 })));
    assert!(!content_known(&json!({ "formatVersion": 3 })));
    assert!(!content_known(
        &json!({ "formatVersion": 2, "cPages": { "pages": [1] } })
    ));
    assert!(!content_known(&json!({ "pages": [] })));
}

#[test]
fn a_firmware_migration_on_both_tablets_does_not_fork() {
    // Base still at an old format; both tablets were updated (migrating
    // every .content to v2) and read the document differently.
    let mut lib = Lib::default();
    let migrated = D::pdf("Paper", "", 2);
    let old = migrated
        .clone()
        .content_set(&["formatVersion"], json!(1))
        .content_set(&["legacyField"], json!(true));
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &migrated.clone().scroll(3))],
        &[(1, &migrated.clone().scroll(4))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Identical);
    // Both sides known: their tags still merge as sets (the old base
    // format does not make the document opaque).
    let (b, o, t) = three(
        &mut lib,
        &[(1, &old)],
        &[(1, &migrated.clone().tags(&["a"]))],
        &[(1, &migrated.clone().tags(&["b"]))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(lib.content(&outcome, &u(1))["tags"], json!(["b", "a"]));
}

#[test]
fn one_unknown_side_makes_a_document_opaque() {
    // The tablet is still at v2, origin's copy was written by a newer
    // firmware: no set merge, the tag changes fork.
    let mut lib = Lib::default();
    let doc = D::pdf("Paper", "", 1);
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc)],
        &[(1, &doc.clone().tags(&["a"]))],
        &[(
            1,
            &doc.clone()
                .tags(&["b"])
                .content_set(&["formatVersion"], json!(3)),
        )],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.forks.len(), 1);
    assert!(notes_of(&outcome, &u(1)).contains(&NoteKind::Opaque));
}

// ---- first-open materialisation --------------------------------------------

/// `skeleton` after xochitl opened it: one page entry per PDF page with
/// fresh ids (`first_page` onwards), derived and view fields, `tags: []`.
fn opened(skeleton: &D, first_page: u32, pages: u32) -> D {
    let entries: Vec<Value> = (0..pages)
        .map(|i| page_entry(first_page + i, &idx(i as usize), Some(u64::from(i))))
        .collect();
    skeleton
        .clone()
        .set("new", json!(false))
        .set("lastOpened", json!("1767400000000"))
        .content_set(&["cPages", "pages"], json!(entries))
        .content_set(
            &["cPages", "original"],
            json!({ "timestamp": "1:1", "value": pages }),
        )
        .content_set(
            &["cPages", "uuids"],
            json!([{ "first": "0f0e0d0c-0000-4000-8000-00000000a001", "second": 1 }]),
        )
        .content_set(&["pageCount"], json!(pages))
        .content_set(&["sizeInBytes"], json!("4096"))
        .content_set(&["customZoomScale"], json!(1))
        .content_set(&["extraMetadata"], json!({ "LastTool": "primary" }))
        .tags(&[])
}

#[test]
fn first_open_materialisation_counts_as_unchanged() {
    let mut lib = Lib::default();
    let skeleton = D::skeleton("Imported", "");
    let ours = opened(&skeleton, 100, 3);
    let theirs = opened(&skeleton, 200, 3).ink(201, "origin ink");
    let (b, o, t) = three(&mut lib, &[(1, &skeleton)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Changed(Side::Theirs)
    );
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &theirs));
    // Merely opened, deleted elsewhere: the deletion stands.
    let (b, o, t) = three(&mut lib, &[(1, &skeleton)], &[(1, &ours)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Deleted(Some(Side::Theirs))
    );
}

#[test]
fn when_both_sides_materialise_origins_page_ids_win() {
    let mut lib = Lib::default();
    let skeleton = D::skeleton("Imported", "");
    let ours = opened(&skeleton, 100, 3);
    let theirs = opened(&skeleton, 200, 3);
    let (b, o, t) = three(&mut lib, &[(1, &skeleton)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &theirs));
    // And with a rename on the tablet: merged, still origin's page ids.
    let renamed = ours.clone().rename("Renamed");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &skeleton)],
        &[(1, &renamed)],
        &[(1, &theirs)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(
        lib.content(&outcome, &u(1)),
        theirs.content.clone().unwrap()
    );
    assert_eq!(lib.md(&outcome, &u(1))["visibleName"], json!("Renamed"));
}

#[test]
fn non_trivial_materialisations_are_changes() {
    let mut lib = Lib::default();
    let skeleton = D::skeleton("Imported", "");
    let theirs = opened(&skeleton, 200, 2).ink(200, "origin");
    let variants = [
        // An inserted note page (no redir).
        opened(&skeleton, 100, 2).ink(150, "inserted"),
        // Ink on a materialised page.
        opened(&skeleton, 100, 2).ink(100, "tablet"),
        // A templated page.
        opened(&skeleton, 100, 2).content_set(
            &["cPages", "pages", "0", "template"],
            json!({ "timestamp": "1:7", "value": "Lined" }),
        ),
        // A deleted page entry.
        opened(&skeleton, 100, 2).content_set(
            &["cPages", "pages", "1", "deleted"],
            json!({ "timestamp": "1:7", "value": 1 }),
        ),
        // redir not matching its position.
        opened(&skeleton, 100, 2).content_set(
            &["cPages", "pages", "1", "redir"],
            json!({ "timestamp": "1:2", "value": 5 }),
        ),
        // A page count disagreeing with the PDF's.
        opened(&skeleton, 100, 2).content_set(
            &["cPages", "original"],
            json!({ "timestamp": "1:1", "value": 3 }),
        ),
    ];
    variants.iter().enumerate().for_each(|(index, ours)| {
        let (b, o, t) = three(&mut lib, &[(1, &skeleton)], &[(1, ours)], &[(1, &theirs)]);
        let outcome = run(&lib, &b, &o, &t);
        assert_eq!(outcome.forks.len(), 1, "variant {index}");
    });
}

/// A base whose page list is already materialised (as `rmu hub add`
/// writes it), materialised again by tablets with ids of their own.
#[test]
fn a_materialised_base_materialised_again_is_no_change() {
    let mut lib = Lib::default();
    let skeleton = D::skeleton("Added", "");
    let base = opened(&skeleton, 300, 3).set("new", json!(true));
    let ours = opened(&skeleton, 100, 3);
    // Origin unchanged: the tablet's bytes are taken (as for any version
    // origin did not touch), and nothing forks.
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &ours)], &[(1, &base)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &ours));
    // Both materialised again: no change on either side, and origin's
    // ids win.
    let theirs = opened(&skeleton, 200, 3);
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Unchanged);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &theirs));
    // Origin inked a page of the base's list: its change wins whole.
    let inked = base.clone().ink(301, "origin ink");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &ours)], &[(1, &inked)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Changed(Side::Theirs)
    );
    // Materialised again with another page count: a real change, which
    // forks against origin's ink.
    let longer = opened(&skeleton, 100, 4);
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &longer)], &[(1, &inked)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.forks.len(), 1);
    // And the tablet's ink on its own ids is never hidden.
    let tablet_ink = ours.clone().ink(100, "tablet ink");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &tablet_ink)], &[(1, &base)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Changed(Side::Ours));
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &tablet_ink));
}

#[test]
fn new_on_both_with_both_sides_materialised_takes_origins_page_ids() {
    // Lost tablet state or a reattach: two tablets each opened the same
    // imported PDF, with different random page ids.
    let mut lib = Lib::default();
    let skeleton = D::skeleton("Shared upload", "");
    let ours = opened(&skeleton, 100, 2);
    let theirs = opened(&skeleton, 200, 2);
    let (b, o, t) = three(&mut lib, &[], &[(1, &ours)], &[(1, &theirs)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.resolutions[&u(1)], Resolution::AddedBoth);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &theirs));
}

#[test]
fn new_on_both_with_one_side_materialised_does_not_fork() {
    let mut lib = Lib::default();
    let skeleton = D::skeleton("Shared upload", "");
    let ours = opened(&skeleton, 100, 2);
    let (b, o, t) = three(&mut lib, &[], &[(1, &ours)], &[(1, &skeleton)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.resolutions[&u(1)], Resolution::AddedBoth);
    let (b, o, t) = three(&mut lib, &[], &[(1, &skeleton)], &[(1, &ours)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.forks.is_empty());
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &ours));
}

#[test]
fn page_array_order_carries_no_meaning() {
    let mut lib = Lib::default();
    let base = D::notebook("Notes", "", 3, "base");
    let pages = base.content.as_ref().unwrap()["cPages"]["pages"].clone();
    let reversed: Vec<Value> = pages.as_array().unwrap().iter().rev().cloned().collect();
    let shuffled = base
        .clone()
        .content_set(&["cPages", "pages"], json!(reversed));
    let renamed = base.clone().rename("Renamed");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &shuffled)], &[(1, &renamed)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Changed(Side::Theirs)
    );
    // A real reorder (idx changed) is a change.
    let reordered = base.clone().content_set(
        &["cPages", "pages", "0", "idx"],
        json!({ "timestamp": "1:8", "value": "bz" }),
    );
    let inked = base.clone().ink(2, "origin");
    let (b, o, t) = three(&mut lib, &[(1, &base)], &[(1, &reordered)], &[(1, &inked)]);
    assert_eq!(run(&lib, &b, &o, &t).forks.len(), 1);
}

#[test]
fn view_state_churn_on_both_sides_never_forks() {
    let mut lib = Lib::default();
    let docs = [D::pdf("Paper", "", 3), D::notebook("Notes", "", 2, "x")];
    docs.iter().for_each(|doc| {
        let (b, o, t) = three(
            &mut lib,
            &[(1, doc)],
            &[(1, &doc.clone().scroll(11))],
            &[(1, &doc.clone().scroll(12))],
        );
        let outcome = run(&lib, &b, &o, &t);
        assert_eq!(outcome.resolutions[&u(1)], Resolution::Unchanged);
        assert!(outcome.forks.is_empty() && outcome.notes.is_empty());
        assert_eq!(
            outcome.docs[&u(1)],
            version_of(&mut lib, &doc.clone().scroll(12))
        );
    });
}

// ---- tree rules -------------------------------------------------------------

#[test]
fn an_edit_restores_a_document_from_the_trash() {
    let mut lib = Lib::default();
    let folder = D::folder("Folder", "");
    let doc = D::notebook("Notes", u(9).as_str(), 1, "base");
    // Origin trashed it, the tablet inked it.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (9, &folder)],
        &[(1, &doc.clone().ink(2, "tablet")), (9, &folder)],
        &[(1, &doc.clone().trash()), (9, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(9).as_str()));
    assert!(
        notes_of(&outcome, &u(1)).contains(&NoteKind::RestoredFromTrash {
            parent: u(9).to_string()
        })
    );
    // The tablet trashed it, origin inked it.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (9, &folder)],
        &[(1, &doc.clone().trash()), (9, &folder)],
        &[(1, &doc.clone().ink(2, "origin")), (9, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(9).as_str()));
    // Restored to exactly origin's version: its blob is reused.
    assert_eq!(
        outcome.docs[&u(1)],
        version_of(&mut lib, &doc.clone().ink(2, "origin"))
    );
}

#[test]
fn an_edit_restores_a_trashed_ancestor_folder() {
    let mut lib = Lib::default();
    let outer = D::folder("Outer", "");
    let inner = D::folder("Inner", u(8).as_str());
    let doc = D::notebook("Notes", u(9).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (9, &inner), (8, &outer)],
        &[(1, &doc.clone().ink(2, "tablet")), (9, &inner), (8, &outer)],
        &[(1, &doc), (9, &inner), (8, &outer.clone().trash())],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(8))["parent"], json!(""));
    assert_eq!(
        notes_of(&outcome, &u(8)),
        [NoteKind::RestoredFromTrash {
            parent: String::new()
        }]
    );
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(9).as_str()));
}

#[test]
fn restoring_walks_up_through_several_trashed_folders() {
    // Origin trashed the inner folder, then the outer one (moved it to
    // the trash after the inner one was already there).
    let mut lib = Lib::default();
    let outer = D::folder("Outer", "");
    let inner = D::folder("Inner", u(8).as_str());
    let doc = D::notebook("Notes", u(9).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (9, &inner), (8, &outer)],
        &[(1, &doc.clone().ink(2, "tablet")), (9, &inner), (8, &outer)],
        &[
            (1, &doc),
            (9, &inner.clone().trash()),
            (8, &outer.clone().trash()),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(9))["parent"], json!(u(8).as_str()));
    assert_eq!(lib.md(&outcome, &u(8))["parent"], json!(""));
}

#[test]
fn trash_by_the_editing_side_and_metadata_edits_are_respected() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    // The tablet inked and trashed it itself.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc)],
        &[(1, &doc.clone().ink(2, "tablet").trash())],
        &[(1, &doc.clone().rename("Renamed"))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(TRASH_PARENT));
    // A rename is not a fork-class edit: the trash stands.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc)],
        &[(1, &doc.clone().rename("Renamed"))],
        &[(1, &doc.clone().trash())],
    );
    let outcome = run(&lib, &b, &o, &t);
    let md = lib.md(&outcome, &u(1));
    assert_eq!(md["parent"], json!(TRASH_PARENT));
    assert_eq!(md["visibleName"], json!("Renamed"));
}

#[test]
fn an_orphans_parent_is_resurrected_from_history() {
    let mut lib = Lib::default();
    let outer = D::folder("Outer", "");
    let inner = D::folder("Inner", u(8).as_str());
    let new_doc = D::notebook("New", u(9).as_str(), 1, "tablet");
    // Origin deleted both folders; the tablet added a document inside.
    let (b, o, t) = three(
        &mut lib,
        &[(9, &inner), (8, &outer)],
        &[(9, &inner), (8, &outer), (1, &new_doc)],
        &[],
    );
    let history: BTreeMap<DocId, Vec<DocVersion>> = BTreeMap::from([
        (u(9), vec![inner.build(&mut lib)]),
        (u(8), vec![outer.build(&mut lib)]),
    ]);
    let outcome = Run {
        history: &history,
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert_eq!(outcome.resolutions[&u(9)], Resolution::Resurrected);
    assert_eq!(outcome.resolutions[&u(8)], Resolution::Resurrected);
    assert_eq!(outcome.docs[&u(9)], version_of(&mut lib, &inner));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(9).as_str()));
    assert!(notes_of(&outcome, &u(9)).contains(&NoteKind::Resurrected));
    // Resurrected items are not deletions.
    assert!(outcome.guard.from_tablet.is_empty());
}

#[test]
fn a_resurrected_folder_from_the_trash_returns_to_its_last_location() {
    let mut lib = Lib::default();
    let home = D::folder("Home", "");
    let folder = D::folder("Folder", u(7).as_str());
    let doc = D::notebook("New", u(9).as_str(), 1, "tablet");
    let (b, o, t) = three(
        &mut lib,
        &[(9, &folder.clone().trash()), (7, &home)],
        &[(9, &folder.clone().trash()), (7, &home), (1, &doc)],
        &[(7, &home)],
    );
    // Newest first: last seen in the trash, before that in Home.
    let history: BTreeMap<DocId, Vec<DocVersion>> = BTreeMap::from([(
        u(9),
        vec![
            folder.clone().trash().build(&mut lib),
            folder.build(&mut lib),
        ],
    )]);
    let outcome = Run {
        history: &history,
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    // The document is new in the tablet's trashed folder... which origin
    // emptied. Its parent comes back, outside the trash.
    assert_eq!(lib.md(&outcome, &u(9))["parent"], json!(u(7).as_str()));
    assert_eq!(outcome.docs[&u(9)], {
        let mut lib2 = lib.clone();
        let mut expected = version_of(&mut lib2, &folder);
        expected.files.insert(
            DocFile::Metadata,
            outcome.docs[&u(9)].files[&DocFile::Metadata].clone(),
        );
        expected
    });
}

#[test]
fn an_orphan_without_history_moves_to_the_root() {
    let mut lib = Lib::default();
    let folder = D::folder("Folder", "");
    let doc = D::notebook("New", u(9).as_str(), 1, "tablet");
    // Origin deleted the folder the tablet added into: it comes back from
    // the base's own version, no history needed.
    let (b, o, t) = three(&mut lib, &[(9, &folder)], &[(9, &folder), (1, &doc)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(9)], Resolution::Resurrected);
    assert_eq!(outcome.docs[&u(9)], version_of(&mut lib, &folder));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(9).as_str()));
    // A parent nobody has, and no history: the root.
    let (b, o, t) = three(&mut lib, &[], &[(1, &doc)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(!outcome.docs.contains_key(&u(9)));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(""));
    assert_eq!(
        notes_of(&outcome, &u(1)),
        [NoteKind::OrphanToRoot {
            missing_parent: u(9).to_string()
        }]
    );
    // Invalid parent strings too.
    let odd = doc.clone().move_to("not-a-uuid");
    let (b, o, t) = three(&mut lib, &[], &[(1, &odd)], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(""));
}

#[test]
fn a_parent_that_is_not_a_folder_moves_the_child_to_the_root() {
    let mut lib = Lib::default();
    let host = D::pdf("A document", "", 1);
    let child = D::notebook("Child", u(2).as_str(), 1, "tablet");
    let (b, o, t) = three(
        &mut lib,
        &[(2, &host)],
        &[(2, &host), (1, &child)],
        &[(2, &host)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(""));
    assert_eq!(
        notes_of(&outcome, &u(1)),
        [NoteKind::ParentNotFolder {
            parent: u(2).to_string()
        }]
    );
}

#[test]
fn a_cycle_takes_origins_parent() {
    let mut lib = Lib::default();
    let a = D::folder("A", "");
    let b_folder = D::folder("B", "");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &a), (2, &b_folder)],
        &[(1, &a.clone().move_to(u(2).as_str())), (2, &b_folder)],
        &[(1, &a), (2, &b_folder.clone().move_to(u(1).as_str()))],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(""));
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(u(1).as_str()));
    assert_eq!(
        notes_of(&outcome, &u(1)),
        [NoteKind::CycleBroken {
            parent: String::new()
        }]
    );
    // A's metadata equals origin's again: its blob is reused.
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &a));
}

#[test]
fn a_three_folder_cycle_terminates_acyclic() {
    let mut lib = Lib::default();
    let f = |name: &str, parent: &str| D::folder(name, parent);
    let (x, y, z) = (u(1), u(2), u(3));
    // Base: X, Y, Z at the root. Tablet: Y in X, Z in Y. Origin: X in Z.
    let (b, o, t) = three(
        &mut lib,
        &[(1, &f("X", "")), (2, &f("Y", "")), (3, &f("Z", ""))],
        &[
            (1, &f("X", "")),
            (2, &f("Y", x.as_str())),
            (3, &f("Z", y.as_str())),
        ],
        &[(1, &f("X", z.as_str())), (2, &f("Y", "")), (3, &f("Z", ""))],
    );
    let outcome = run(&lib, &b, &o, &t);
    let items: BTreeMap<DocId, TreeItem> = [&x, &y, &z]
        .into_iter()
        .map(|id| (id.clone(), TreeItem::from_metadata(&lib.md(&outcome, id))))
        .collect();
    assert!(tree_violations(&items).is_empty());
    assert_eq!(
        outcome
            .notes
            .iter()
            .filter(|note| matches!(note.kind, NoteKind::CycleBroken { .. }))
            .count(),
        1
    );
}

#[test]
fn a_restore_that_would_close_a_cycle_moves_the_edit_out_instead() {
    let mut lib = Lib::default();
    let a = D::folder("A", "");
    let b_folder = D::folder("B", "");
    let doc = D::notebook("Notes", u(1).as_str(), 1, "base");
    // Origin: trash A, move B into A. Tablet: move A into B, ink the
    // document in A. A's pre-trash parent on the tablet is B, which is
    // now inside A: restoring A there would close a cycle, so the inked
    // document comes out of A instead (to the root: its tablet and base
    // parent is A itself).
    let (b, o, t) = three(
        &mut lib,
        &[(1, &a), (2, &b_folder), (3, &doc)],
        &[
            (1, &a.clone().move_to(u(2).as_str())),
            (2, &b_folder),
            (3, &doc.clone().ink(2, "tablet")),
        ],
        &[
            (1, &a.clone().trash()),
            (2, &b_folder.clone().move_to(u(1).as_str())),
            (3, &doc),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(tree_violations(&merged_items(&lib, &outcome)), vec![]);
    assert!(!in_trash(&lib, &outcome, &u(3)));
    assert_eq!(lib.md(&outcome, &u(3))["parent"], json!(""));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(TRASH_PARENT));
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(u(1).as_str()));
}

#[test]
fn a_resurrected_folder_on_a_cycle_goes_to_the_root() {
    // R was in the trash at the base. Origin restored R into S, then
    // deleted it; the tablet moved S into R. S needs R back; the base's
    // R is in the trash, so history gives its last location, inside S: a
    // cycle. R was resurrected, so R goes to the root (not S, although S
    // has the smaller UUID and differs from origin's parent).
    let mut lib = Lib::default();
    let s_folder = D::folder("S", "");
    let r = D::folder("R", "").trash();
    let (b, o, t) = three(
        &mut lib,
        &[(1, &s_folder), (2, &r)],
        &[(1, &s_folder.clone().move_to(u(2).as_str())), (2, &r)],
        &[(1, &s_folder)],
    );
    let history: BTreeMap<DocId, Vec<DocVersion>> =
        BTreeMap::from([(u(2), vec![r.clone().move_to(u(1).as_str()).build(&mut lib)])]);
    let outcome = Run {
        history: &history,
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert_eq!(outcome.resolutions[&u(2)], Resolution::Resurrected);
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(""));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(2).as_str()));
    assert!(notes_of(&outcome, &u(2)).contains(&NoteKind::CycleBroken {
        parent: String::new()
    }));
}

#[test]
fn a_trashed_folder_the_editing_side_lacks_returns_to_its_base_parent() {
    // Base: G, T in G, D in T. The tablet deleted T (keeping D, inked);
    // origin renamed and trashed T. T is kept (renamed) and must come
    // out of the trash: the tablet has no parent for it, base has G.
    let mut lib = Lib::default();
    let g = D::folder("G", "");
    let t_folder = D::folder("T", u(1).as_str());
    let doc = D::notebook("Notes", u(2).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &g), (2, &t_folder), (3, &doc)],
        &[(1, &g), (3, &doc.clone().ink(2, "tablet"))],
        &[
            (1, &g),
            (2, &t_folder.clone().rename("T (old)").trash()),
            (3, &doc),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(u(1).as_str()));
    assert_eq!(lib.md(&outcome, &u(3))["parent"], json!(u(2).as_str()));
    assert!(
        notes_of(&outcome, &u(2)).contains(&NoteKind::RestoredFromTrash {
            parent: u(1).to_string()
        })
    );
}

#[test]
fn tree_violations_report_orphans_non_folders_and_cycles() {
    let item = |parent: &str, kind: Option<ItemKind>| TreeItem {
        parent: parent.to_string(),
        kind,
    };
    let folder = Some(ItemKind::Folder);
    let document = Some(ItemKind::Document);
    let items = BTreeMap::from([
        (u(1), item("", folder)),
        (u(2), item(TRASH_PARENT, folder)),
        (u(3), item(u(1).as_str(), document)),
        (u(4), item(u(99).as_str(), document)),
        (u(5), item("garbage", document)),
        (u(6), item(u(3).as_str(), document)),
        // A self-loop, a 2-cycle and a 3-cycle (reported once each,
        // starting at the smallest member).
        (u(7), item(u(7).as_str(), folder)),
        (u(9), item(u(8).as_str(), folder)),
        (u(8), item(u(9).as_str(), folder)),
        (u(12), item(u(10).as_str(), folder)),
        (u(10), item(u(11).as_str(), folder)),
        (u(11), item(u(12).as_str(), folder)),
        // Hanging off a cycle: not part of it.
        (u(13), item(u(10).as_str(), document)),
    ]);
    assert_eq!(
        tree_violations(&items),
        vec![
            TreeViolation::MissingParent {
                item: u(4),
                parent: u(99).to_string()
            },
            TreeViolation::MissingParent {
                item: u(5),
                parent: "garbage".into()
            },
            TreeViolation::ParentNotFolder {
                item: u(6),
                parent: u(3)
            },
            TreeViolation::Cycle(vec![u(7)]),
            TreeViolation::Cycle(vec![u(8), u(9)]),
            TreeViolation::Cycle(vec![u(10), u(11), u(12)]),
        ]
    );
    assert!(tree_violations(&BTreeMap::new()).is_empty());
}

// ---- deletion guard --------------------------------------------------------

#[test]
fn guard_thresholds_at_their_boundaries() {
    let t = GuardThresholds::default();
    assert!(!t.exceeded(10, 10));
    assert!(!t.exceeded(10, 50));
    assert!(t.exceeded(11, 100));
    assert!(!t.exceeded(11, 110));
    assert!(t.exceeded(11, 109));
    assert!(!t.exceeded(50, 10_000));
    assert!(t.exceeded(51, 10_000));
    assert!(!t.exceeded(0, 0));
    let strict = GuardThresholds {
        count: 0,
        percent: 0,
        absolute: 1000,
    };
    assert!(strict.exceeded(1, 1000));
}

fn library(count: u32) -> Vec<(u32, D)> {
    (1..=count)
        .map(|n| (n, D::pdf(&format!("Doc {n}"), "", 1)))
        .collect()
}

fn refs(docs: &[(u32, D)]) -> Vec<(u32, &D)> {
    docs.iter().map(|(n, d)| (*n, d)).collect()
}

#[test]
fn the_guard_counts_deletions_from_origin() {
    let mut lib = Lib::default();
    let docs = library(100);
    let all = refs(&docs);
    let keep: Vec<(u32, &D)> = all.iter().filter(|(n, _)| *n > 11).cloned().collect();
    let (b, o, t) = three(&mut lib, &all, &keep, &all);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.guard.from_origin.len(), 11);
    assert!(outcome.guard.needs_consent && outcome.guard.blocked());
    assert!(outcome.guard.consented().is_empty());
    let allowed = Run {
        allow: true,
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert!(!allowed.guard.blocked());
    assert_eq!(allowed.guard.consented(), allowed.guard.from_origin);
    // Ten are fine.
    let keep: Vec<(u32, &D)> = all.iter().filter(|(n, _)| *n > 10).cloned().collect();
    let (b, o, t) = three(&mut lib, &all, &keep, &all);
    assert!(!run(&lib, &b, &o, &t).guard.needs_consent);
}

#[test]
fn the_guard_counts_tablet_deletions_without_recorded_consent() {
    let mut lib = Lib::default();
    let docs = library(30);
    let all = refs(&docs);
    let survivors: Vec<(u32, &D)> = all.iter().filter(|(n, _)| *n > 20).cloned().collect();
    let (b, o, t) = three(&mut lib, &all, &all, &survivors);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.guard.from_tablet.len(), 20);
    assert!(outcome.guard.blocked());
    // Origin recorded consent for these deletions: exempt.
    let consented = Run {
        consent: (1..=20).map(u).collect(),
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert!(consented.guard.from_tablet.is_empty());
    assert_eq!(consented.guard.exempt.len(), 20);
    assert!(!consented.guard.needs_consent);
    // Consent for only some of them: the rest still count.
    let partial = Run {
        consent: (1..=5).map(u).collect(),
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert_eq!(partial.guard.from_tablet.len(), 15);
    assert!(partial.guard.blocked());
}

#[test]
fn an_empty_snapshot_always_needs_consent() {
    let mut lib = Lib::default();
    let doc = D::pdf("Only", "", 1);
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(outcome.guard.empty_snapshot && outcome.guard.blocked());
    // An empty library everywhere is not.
    let (b, o, t) = three(&mut lib, &[], &[], &[]);
    let outcome = run(&lib, &b, &o, &t);
    assert!(!outcome.guard.needs_consent);
}

// ---- convergence and restartability ------------------------------------------

/// A library where every kind of decision happens at once.
fn busy_scenario(lib: &mut Lib) -> (RepoTree, RepoTree, RepoTree) {
    let folder = D::folder("Folder", "");
    let notes = D::notebook("Notes", u(9).as_str(), 2, "base");
    let paper = D::pdf("Paper", u(9).as_str(), 3);
    let gone = D::pdf("Gone", "", 1);
    let book = epub("Book");
    let base: Vec<(u32, &D)> = vec![
        (9, &folder),
        (1, &notes),
        (2, &paper),
        (3, &gone),
        (4, &book),
    ];
    let o1 = notes.clone().ink(2, "tablet");
    let o2 = paper.clone().rename("Paper (read)").scroll(3).tags(&["t"]);
    let o5 = D::notebook("Tablet new", "", 1, "fresh");
    let ours: Vec<(u32, &D)> = vec![(9, &folder), (1, &o1), (2, &o2), (4, &book), (5, &o5)];
    let t1 = notes.clone().ink(2, "origin").rename("Notes!");
    let t2 = paper.clone().tags(&["o"]).set("pinned", json!(true));
    let t4 = book.clone().trash();
    let theirs: Vec<(u32, &D)> = vec![(9, &folder), (1, &t1), (2, &t2), (3, &gone), (4, &t4)];
    three(lib, &base, &ours, &theirs)
}

#[test]
fn remerging_a_merge_result_is_a_no_op() {
    let mut lib = Lib::default();
    let (b, o, t) = busy_scenario(&mut lib);
    let first = run(&lib, &b, &o, &t);
    assert_eq!(first.forks.len(), 1);
    let merged = lib.realise_tree(&first);
    // The tablet applied the merge: base, ours and theirs are all it.
    let again = run(&lib, &merged, &merged, &merged);
    assert!(
        again
            .resolutions
            .values()
            .all(|resolution| *resolution == Resolution::Unchanged)
    );
    assert_eq!(lib.realise_tree(&again), merged);
    assert!(again.notes.is_empty() && again.forks.is_empty());
    // Another tablet still at origin's old main takes the merge as is.
    let other = run(&lib, &t, &t, &merged);
    assert_eq!(lib.realise_tree(&other), merged);
    // The same changes arriving from both sides converge.
    let both = run(&lib, &b, &merged, &merged);
    assert_eq!(lib.realise_tree(&both), merged);
    // The tablet read some documents meanwhile: still nothing to do.
    let read = RepoTree {
        docs: merged
            .docs
            .iter()
            .map(|(id, version)| (id.clone(), version.clone()))
            .collect(),
        ..merged.clone()
    };
    assert!(run(&lib, &merged, &read, &merged).forks.is_empty());
}

#[test]
fn recomputing_a_merge_gives_identical_results() {
    let mut lib = Lib::default();
    let (b, o, t) = busy_scenario(&mut lib);
    let first = run(&lib, &b, &o, &t);
    let second = run(&lib, &b, &o, &t);
    assert_eq!(first, second);
    // A hash-map blob source (random iteration order) changes nothing.
    let hashed: HashMap<Oid, Vec<u8>> = lib.blobs.clone().into_iter().collect();
    let device = DeviceName::new(DEVICE).unwrap();
    let consent = BTreeSet::new();
    let params = MergeParams {
        rules: MirrorRules::latest(),
        device_name: &device,
        date: DATE,
        allow_deletions: false,
        thresholds: GuardThresholds::default(),
        consented_deletions: &consent,
    };
    let third = merge(
        &MergeInput {
            base: &b,
            ours: &o,
            theirs: &t,
        },
        &hashed,
        &NoHistory,
        &params,
    )
    .unwrap();
    assert_eq!(first, third);
}

#[test]
fn the_busy_scenario_decides_every_document() {
    let mut lib = Lib::default();
    let (b, o, t) = busy_scenario(&mut lib);
    let outcome = run(&lib, &b, &o, &t);
    let copy = outcome.forks[0].copy.clone();
    assert_eq!(
        outcome.resolutions[&u(1)],
        Resolution::Forked { copy: copy.clone() }
    );
    assert_eq!(outcome.resolutions[&u(2)], Resolution::Merged);
    assert_eq!(
        outcome.resolutions[&u(3)],
        Resolution::Deleted(Some(Side::Ours))
    );
    assert_eq!(
        outcome.resolutions[&u(4)],
        Resolution::Changed(Side::Theirs)
    );
    assert_eq!(outcome.resolutions[&u(5)], Resolution::Added(Side::Ours));
    assert_eq!(outcome.resolutions[&u(9)], Resolution::Unchanged);
    let paper = lib.md(&outcome, &u(2));
    assert_eq!(paper["visibleName"], json!("Paper (read)"));
    assert_eq!(paper["pinned"], json!(true));
    assert_eq!(lib.content(&outcome, &u(2))["tags"], json!(["o", "t"]));
    assert_eq!(
        outcome.summary(),
        Summary {
            unchanged: 1,
            changed_theirs: 1,
            added: 1,
            deleted: 1,
            merged: 1,
            forked: 2,
            ..Summary::default()
        }
    );
}

// ---- misc ------------------------------------------------------------------

#[test]
fn carried_forward_paths_and_non_documents_are_kept() {
    let mut lib = Lib::default();
    let extra = lib.put(b"future");
    let doc = D::notebook("Notes", "", 1, "base");
    let with_other = |lib: &mut Lib, d: &D| {
        let mut version = d.build(lib);
        version
            .other
            .insert("future/thing.bin".into(), extra.clone());
        version
    };
    let loose = DocVersion {
        files: BTreeMap::from([(DocFile::Content, lib.put(b"[]"))]),
        other: BTreeMap::from([("notes.txt".to_string(), extra.clone())]),
    };
    let mk = |lib: &mut Lib, d: &D| RepoTree {
        docs: BTreeMap::from([(u(1), with_other(lib, d)), (u(2), loose.clone())]),
        extra: BTreeMap::from([(".rmu/hub.json".to_string(), extra.clone())]),
    };
    let b = mk(&mut lib, &doc);
    let o = mk(&mut lib, &doc.clone().ink(2, "tablet"));
    let mut t = mk(&mut lib, &doc.clone().rename("Renamed"));
    t.extra
        .insert("README.md".to_string(), lib.put(b"hand-written"));
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.docs[&u(1)].other["future/thing.bin"], extra);
    assert_eq!(outcome.docs[&u(2)], MergedVersion::existing(&loose));
    assert_eq!(outcome.extra, t.extra);
}

#[test]
fn a_quarantined_document_carried_forward_is_never_deleted_by_the_tablet() {
    // A quarantined document's snapshot is its base version, carried
    // forward: to the merge it is unchanged on the tablet, so the tablet
    // never deletes or rewrites it in origin. (Apply, in the sync cycle,
    // never touches the malformed files on the tablet either.)
    let mut lib = Lib::default();
    let doc = D::notebook("Malformed on the tablet", "", 2, "base");
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &doc)], &[(1, &doc)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &doc));
    let inked = doc.clone().ink(3, "origin");
    let (b, o, t) = three(&mut lib, &[(1, &doc)], &[(1, &doc)], &[(1, &inked)]);
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.docs[&u(1)], version_of(&mut lib, &inked));
    assert!(outcome.guard.from_origin.is_empty());
}

#[test]
fn carried_forward_paths_and_modes_are_fork_class() {
    let mut lib = Lib::default();
    let doc = D::pdf("Paper", "", 2);
    let base = doc.build(&mut lib);
    let ours = doc.clone().ink(1, "tablet").build(&mut lib);
    // Origin gained a file of a newer rule set.
    let mut newer = base.clone();
    newer
        .other
        .insert("future/thing.bin".into(), lib.put(b"future"));
    // Origin changed a file's mode.
    let mut moded = base.clone();
    moded.files.get_mut(&DocFile::PayloadPdf).unwrap().mode = FileMode::Executable;
    [newer, moded].into_iter().for_each(|theirs| {
        let tree = |version: &DocVersion| RepoTree {
            docs: BTreeMap::from([(u(1), version.clone())]),
            extra: BTreeMap::new(),
        };
        let outcome = run(&lib, &tree(&base), &tree(&ours), &tree(&theirs));
        assert_eq!(outcome.forks.len(), 1);
        let copy = &outcome.docs[&outcome.forks[0].copy];
        assert_eq!(outcome.docs[&u(1)].other, theirs.other);
        assert_eq!(
            outcome.docs[&u(1)].files[&DocFile::PayloadPdf],
            MergedEntry::Existing(theirs.files[&DocFile::PayloadPdf].clone())
        );
        assert_eq!(
            page_blobs(copy),
            page_blobs(&MergedVersion::existing(&ours))
        );
    });
}

#[test]
fn a_large_reattach_with_a_trashed_library_part_is_fast() {
    // Reattach: empty bases, every document new on both and identical,
    // a few hundred of them in the trash. Trash vs edit used to be cubic
    // here (minutes); it must stay roughly linear.
    let mut lib = Lib::default();
    let docs: Vec<(u32, D)> = (1..=2000u32)
        .map(|n| {
            let doc = D::pdf(&format!("Doc {n}"), "", 1);
            (n, if n % 7 == 0 { doc.trash() } else { doc })
        })
        .collect();
    let all = refs(&docs);
    let (b, o, t) = three(&mut lib, &[], &all, &all);
    let started = std::time::Instant::now();
    let outcome = run(&lib, &b, &o, &t);
    let elapsed = started.elapsed();
    assert_eq!(outcome.docs.len(), 2000);
    assert!(outcome.notes.is_empty());
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "merge took {elapsed:?}"
    );
}

#[test]
fn errors_are_typed() {
    let mut lib = Lib::default();
    let doc = D::notebook("Notes", "", 1, "base");
    let broken = DocVersion {
        files: BTreeMap::from([(DocFile::Metadata, lib.put(b"{not json"))]),
        other: BTreeMap::new(),
    };
    let b = tree(&mut lib, &[(1, &doc)]);
    let o = tree(&mut lib, &[(1, &doc.clone().ink(2, "x"))]);
    let t = RepoTree {
        docs: BTreeMap::from([(u(1), broken)]),
        extra: BTreeMap::new(),
    };
    assert!(matches!(
        Run::default().go(&lib, &b, &o, &t),
        Err(crate::error::Error::HubMerge(MergeError::InvalidJson { doc, .. })) if doc == u(1)
    ));
    let mut empty = lib.clone();
    empty.blobs.clear();
    let t2 = tree(&mut lib, &[(1, &doc.clone().rename("x"))]);
    assert!(matches!(
        Run::default().go(&empty, &b, &o, &t2),
        Err(crate::error::Error::HubMerge(MergeError::MissingBlob(_)))
    ));
    ["2026-1-09", "20261109", "2026-11-0x", ""]
        .iter()
        .for_each(|date| assert!(validate_date(date).is_err(), "{date}"));
    assert!(validate_date(DATE).is_ok());
}

#[test]
fn notes_read_well() {
    let note = |kind| Note { doc: u(1), kind }.to_string();
    assert_eq!(
        note(NoteKind::MetadataConflict {
            field: "visibleName".into()
        }),
        format!(
            "{}: \"visibleName\" changed on both sides; kept origin's value",
            u(1)
        )
    );
    assert_eq!(
        note(NoteKind::KeptOverDeletion {
            deleted_by: Side::Ours
        }),
        format!(
            "{}: deleted on the tablet but changed on the other side; kept",
            u(1)
        )
    );
    assert!(
        note(NoteKind::CycleBroken {
            parent: String::new()
        })
        .ends_with("moved to the root")
    );
}

// ---- property test ---------------------------------------------------------

/// A small deterministic PRNG (xorshift64*), so the property test needs
/// no dependency and every failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Apply random edits for one side.
fn mutate(rng: &mut Rng, side: &str, docs: &BTreeMap<u32, D>) -> BTreeMap<u32, D> {
    let edits = rng.below(7);
    (0..edits).fold(docs.clone(), |mut docs, step| {
        let folders: Vec<u32> = docs
            .iter()
            .filter(|(_, d)| d.md["type"] == json!("CollectionType"))
            .map(|(n, _)| *n)
            .collect();
        let keys: Vec<u32> = docs.keys().copied().collect();
        let target = keys[rng.below(keys.len() as u64) as usize];
        let parent = match rng.below(folders.len() as u64 + 1) as usize {
            0 => String::new(),
            i => u(folders[i - 1]).to_string(),
        };
        let d = docs[&target].clone();
        let is_folder = d.md["type"] == json!("CollectionType");
        match rng.below(11) {
            0..=2 if !is_folder => {
                let n = 1 + rng.below(4) as u32;
                docs.insert(
                    target,
                    d.ink(n, &format!("{side} ink {step} {}", rng.next())),
                );
            }
            3 if !is_folder => {
                docs.insert(target, d.drop_page(1 + rng.below(3) as u32));
            }
            4 => {
                docs.insert(target, d.rename(&format!("{side} {step}")));
            }
            5 => {
                docs.insert(target, d.move_to(&parent));
            }
            6 => {
                docs.insert(target, d.trash());
            }
            7 => {
                docs.remove(&target);
            }
            8 => {
                let n = 100 + rng.below(1000) as u32;
                docs.insert(
                    n,
                    D::notebook(&format!("{side} new"), &parent, 1, &format!("{side} {n}")),
                );
            }
            9 => {
                let n = 2000 + rng.below(1000) as u32;
                docs.insert(n, D::folder(&format!("{side} folder {n}"), &parent));
            }
            _ if !is_folder => {
                docs.insert(target, d.scroll(rng.below(50) as u32));
            }
            _ => {}
        }
        docs
    })
}

/// The documents whose fork class a side changed against base: new
/// documents, and those whose page files or page entries (without their
/// view state) differ. The generator changes nothing else in the fork
/// class, so this oracle is exact and independent of the engine.
fn fork_edited(base: &BTreeMap<u32, D>, side: &BTreeMap<u32, D>) -> BTreeSet<u32> {
    let pages = |d: &D| -> Option<Value> {
        let mut pages = d.content.as_ref()?.get("cPages")?.get("pages")?.clone();
        pages.as_array_mut()?.iter_mut().for_each(|page| {
            if let Some(entry) = page.as_object_mut() {
                entry.remove("scrollTime");
                entry.remove("verticalScroll");
            }
        });
        Some(pages)
    };
    side.iter()
        .filter(|(n, d)| match base.get(n) {
            None => d.md["type"] == json!("DocumentType"),
            Some(before) => before.files != d.files || pages(before) != pages(d),
        })
        .map(|(n, _)| *n)
        .collect()
}

/// A side's own tree, as tree items.
fn items_of(docs: &BTreeMap<u32, D>) -> BTreeMap<DocId, TreeItem> {
    docs.iter()
        .map(|(n, d)| (u(*n), TreeItem::from_metadata(&d.md)))
        .collect()
}

fn tree_of(lib: &mut Lib, docs: &BTreeMap<u32, D>) -> RepoTree {
    RepoTree {
        docs: docs.iter().map(|(n, d)| (u(*n), d.build(lib))).collect(),
        extra: BTreeMap::new(),
    }
}

/// Every `.rm` blob that is new on a side (not in that document's base)
/// must survive the merge somewhere.
fn new_ink(base: &RepoTree, side: &RepoTree) -> BTreeSet<Oid> {
    side.docs
        .iter()
        .flat_map(|(id, version)| {
            let base_pages: BTreeSet<&Oid> = base
                .docs
                .get(id)
                .map(|base| {
                    base.files
                        .iter()
                        .filter(|(file, _)| file.kind() == FileKind::Page)
                        .map(|(_, entry)| &entry.id)
                        .collect()
                })
                .unwrap_or_default();
            version
                .files
                .iter()
                .filter(|(file, _)| file.kind() == FileKind::Page)
                .map(|(_, entry)| &entry.id)
                .filter(|id| !base_pages.contains(id))
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn property_ink_survives_trees_stay_valid_and_merges_converge() {
    (1..=2000u64).for_each(|seed| {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut lib = Lib::default();
        let base_docs: BTreeMap<u32, D> = BTreeMap::from([
            (1, D::folder("A", "")),
            (2, D::folder("B", "")),
            (3, D::folder("C", u(1).as_str())),
            (10, D::notebook("N1", u(1).as_str(), 3, "b1")),
            (11, D::notebook("N2", u(3).as_str(), 2, "b2")),
            (12, D::pdf("P1", u(2).as_str(), 3)),
            (13, D::notebook("N3", "", 1, "b3")),
        ]);
        let ours_docs = mutate(&mut rng, "tablet", &base_docs);
        let theirs_docs = mutate(&mut rng, "origin", &base_docs);
        let ours_edited = fork_edited(&base_docs, &ours_docs);
        let theirs_edited = fork_edited(&base_docs, &theirs_docs);
        let base = tree_of(&mut lib, &base_docs);
        let ours = tree_of(&mut lib, &ours_docs);
        let theirs = tree_of(&mut lib, &theirs_docs);
        // Odd seeds can resurrect from history; even ones cannot, so
        // orphans go to the root.
        let history: BTreeMap<DocId, Vec<DocVersion>> = base
            .docs
            .iter()
            .filter(|_| seed % 2 == 1)
            .map(|(id, version)| (id.clone(), vec![version.clone()]))
            .collect();
        let runner = Run {
            history: &history,
            ..Run::default()
        };
        let outcome = runner.go(&lib, &base, &ours, &theirs).unwrap();
        let merged_tree = merged_items(&lib, &outcome);

        // Ink is never discarded.
        let kept: BTreeSet<Oid> = outcome
            .docs
            .values()
            .filter(|version| version.is_present())
            .flat_map(page_blobs)
            .collect();
        new_ink(&base, &ours)
            .union(&new_ink(&base, &theirs))
            .for_each(|blob| assert!(kept.contains(blob), "seed {seed}: lost ink {blob}"));

        // Edits beat deletion, hard and soft: a document a side changed
        // outside its own trash is present and not in the trash.
        [(&ours_docs, &ours_edited), (&theirs_docs, &theirs_edited)]
            .into_iter()
            .for_each(|(docs, edited)| {
                let own = items_of(docs);
                edited
                    .iter()
                    .filter(|n| docs.contains_key(n))
                    .map(|n| u(*n))
                    .filter(|id| trash_path(&own, id).is_none())
                    .for_each(|id| {
                        assert!(merged_tree.contains_key(&id), "seed {seed}: {id} deleted");
                        assert_eq!(
                            trash_path(&merged_tree, &id),
                            None,
                            "seed {seed}: edited {id} left in the trash"
                        );
                    });
            });

        // Forks only where both sides changed the fork class.
        outcome.forks.iter().for_each(|fork| {
            let n = (1..3000u32).find(|n| u(*n) == fork.original).unwrap();
            assert!(
                ours_edited.contains(&n) && theirs_edited.contains(&n),
                "seed {seed}: spurious fork of {}",
                fork.original
            );
        });

        // The merged tree is valid.
        assert_eq!(tree_violations(&merged_tree), vec![], "seed {seed}");

        // Deterministic.
        assert_eq!(
            runner.go(&lib, &base, &ours, &theirs).unwrap(),
            outcome,
            "seed {seed}"
        );

        // Converged: re-merging the applied result changes nothing.
        let merged = lib.realise_tree(&outcome);
        let again = runner.go(&lib, &merged, &merged, &merged).unwrap();
        assert_eq!(lib.realise_tree(&again), merged, "seed {seed}");
        assert!(
            again.forks.is_empty() && again.notes.is_empty(),
            "seed {seed}"
        );
        // Another tablet that only read documents meanwhile converges too.
        let readers: BTreeMap<u32, D> = theirs_docs
            .iter()
            .map(|(n, d)| (*n, d.clone().scroll(77)))
            .collect();
        let reader = tree_of(&mut lib, &readers);
        let follow = runner.go(&lib, &theirs, &reader, &merged).unwrap();
        assert!(follow.forks.is_empty(), "seed {seed}");
        assert_eq!(follow.guard.from_origin, BTreeSet::new(), "seed {seed}");
    });
}

// ---- review regressions: edits never end in the trash -----------------------

/// The merged tree's items.
fn merged_items(lib: &Lib, outcome: &MergeOutcome) -> BTreeMap<DocId, TreeItem> {
    outcome
        .docs
        .iter()
        .filter(|(_, version)| version.is_present())
        .map(|(id, version)| {
            let md = lib.json(version, &DocFile::Metadata).unwrap();
            (id.clone(), TreeItem::from_metadata(&md))
        })
        .collect()
}

fn in_trash(lib: &Lib, outcome: &MergeOutcome, id: &DocId) -> bool {
    trash_path(&merged_items(lib, outcome), id).is_some()
}

#[test]
fn p13_an_edit_moved_into_a_folder_both_sides_trashed_comes_out() {
    // Tablet B trashed Old (empty there) and kept inking N; tablet A
    // moved N into Old, trashed Old and synced first.
    let mut lib = Lib::default();
    let old = D::folder("Old", "");
    let doc = D::notebook("Notes", "", 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &old)],
        &[
            (1, &doc.clone().ink(2, "tablet ink")),
            (2, &old.clone().trash()),
        ],
        &[
            (1, &doc.clone().move_to(u(2).as_str())),
            (2, &old.clone().trash()),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(!in_trash(&lib, &outcome, &u(1)));
    // Old stays in the trash (both sides put it there); N is taken out to
    // where the tablet has it.
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(TRASH_PARENT));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(""));
    assert!(
        notes_of(&outcome, &u(1)).contains(&NoteKind::RestoredFromTrash {
            parent: String::new()
        })
    );
}

#[test]
fn p14_an_edit_inside_a_folder_filed_into_a_trashed_folder_comes_out() {
    // The tablet filed folder A into Old and trashed Old; origin trashed
    // Old too and inked a notebook in A.
    let mut lib = Lib::default();
    let old = D::folder("Old", "");
    let a = D::folder("A", "");
    let doc = D::notebook("Notes", u(3).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &old), (3, &a)],
        &[
            (1, &doc),
            (2, &old.clone().trash()),
            (3, &a.clone().move_to(u(2).as_str())),
        ],
        &[
            (1, &doc.clone().ink(2, "origin ink")),
            (2, &old.clone().trash()),
            (3, &a),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(!in_trash(&lib, &outcome, &u(1)));
    assert_eq!(lib.md(&outcome, &u(3))["parent"], json!(""));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(3).as_str()));
}

#[test]
fn p2_an_edit_moved_into_a_new_trashed_folder_comes_out() {
    // Origin created folder X, moved N into it and trashed X; the tablet
    // inked N. Neither the tablet nor base has X.
    let mut lib = Lib::default();
    let x = D::folder("X", "").trash();
    let doc = D::notebook("Notes", "", 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc)],
        &[(1, &doc.clone().ink(2, "tablet"))],
        &[(1, &doc.clone().move_to(u(2).as_str())), (2, &x)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(!in_trash(&lib, &outcome, &u(1)));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(""));
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(TRASH_PARENT));
}

#[test]
fn p3_an_edit_resurrected_into_a_trashed_folder_comes_out() {
    // Origin deleted folder F (with N) and trashed F's parent P; the
    // tablet inked N. N is kept, F resurrected into P: then P must come
    // back too.
    let mut lib = Lib::default();
    let p = D::folder("P", "");
    let f = D::folder("F", u(2).as_str());
    let doc = D::notebook("Notes", u(3).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &p), (3, &f)],
        &[(1, &doc.clone().ink(2, "tablet")), (2, &p), (3, &f)],
        &[(2, &p.clone().trash())],
    );
    let history: BTreeMap<DocId, Vec<DocVersion>> =
        BTreeMap::from([(u(3), vec![f.build(&mut lib)])]);
    let outcome = Run {
        history: &history,
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert_eq!(outcome.resolutions[&u(3)], Resolution::Resurrected);
    assert!(!in_trash(&lib, &outcome, &u(1)));
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(""));
}

#[test]
fn p4_a_cycle_broken_into_a_trashed_folder_does_not_trash_an_edit() {
    // Base: T, X in T, Y. Tablet: X into Y, ink D in X. Origin: Y into X,
    // trash T. Cycle X <-> Y; X gets origin's parent T, which is in the
    // trash with D below it: T must come back.
    let mut lib = Lib::default();
    let t_folder = D::folder("T", "");
    let x = D::folder("X", u(2).as_str());
    let y = D::folder("Y", "");
    let doc = D::notebook("Notes", u(3).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &t_folder), (3, &x), (4, &y)],
        &[
            (1, &doc.clone().ink(2, "tablet")),
            (2, &t_folder),
            (3, &x.clone().move_to(u(4).as_str())),
            (4, &y),
        ],
        &[
            (1, &doc),
            (2, &t_folder.clone().trash()),
            (3, &x),
            (4, &y.clone().move_to(u(3).as_str())),
        ],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(tree_violations(&merged_items(&lib, &outcome)), vec![]);
    assert!(!in_trash(&lib, &outcome, &u(1)));
}

#[test]
fn p1_a_deep_resurrection_chain_is_resolved_completely() {
    let mut lib = Lib::default();
    let folders: Vec<(u32, D)> = (1..=8u32)
        .map(|n| {
            let parent = if n == 1 {
                String::new()
            } else {
                u(n - 1).to_string()
            };
            (n, D::folder(&format!("F{n}"), &parent))
        })
        .collect();
    let doc = D::notebook("Deep", u(8).as_str(), 1, "base");
    let inked = doc.clone().ink(2, "tablet");
    let base: Vec<(u32, &D)> = folders
        .iter()
        .map(|(n, d)| (*n, d))
        .chain([(100, &doc)])
        .collect();
    let ours: Vec<(u32, &D)> = folders
        .iter()
        .map(|(n, d)| (*n, d))
        .chain([(100, &inked)])
        .collect();
    let (b, o, t) = three(&mut lib, &base, &ours, &[]);
    let history: BTreeMap<DocId, Vec<DocVersion>> = folders
        .iter()
        .map(|(n, d)| (u(*n), vec![d.build(&mut lib)]))
        .collect();
    let outcome = Run {
        history: &history,
        ..Run::default()
    }
    .go(&lib, &b, &o, &t)
    .unwrap();
    assert_eq!(tree_violations(&merged_items(&lib, &outcome)), vec![]);
    assert!((1..=8).all(|n| outcome.resolutions[&u(n)] == Resolution::Resurrected));
}

#[test]
fn fixpoint_reports_change_and_refuses_to_run_past_its_bound() {
    // Counts down to zero: three changing steps, then a quiet one.
    let countdown =
        |n: u32| -> Result<(u32, bool)> { Ok(if n == 0 { (0, false) } else { (n - 1, true) }) };
    assert_eq!(fixpoint(3, 3, "test", countdown).unwrap(), (0, true));
    assert_eq!(fixpoint(0, 0, "test", countdown).unwrap(), (0, false));
    assert!(matches!(
        fixpoint(4, 3, "test", countdown),
        Err(crate::error::Error::HubMerge(MergeError::NoFixpoint {
            rule: "test",
            bound: 3
        }))
    ));
    // Errors from a step pass through.
    assert!(matches!(
        fixpoint(1, 3, "test", |_| -> Result<(u32, bool)> {
            Err(MergeError::InvalidDate("x".into()).into())
        }),
        Err(crate::error::Error::HubMerge(MergeError::InvalidDate(_)))
    ));
}

// ---- re-review follow-ups ---------------------------------------------------

/// A blob source that counts reads.
struct Counting<'a> {
    blobs: &'a BTreeMap<Oid, Vec<u8>>,
    reads: std::cell::Cell<usize>,
}

impl BlobSource for Counting<'_> {
    fn read(&self, id: &Oid) -> Result<Vec<u8>> {
        self.reads.set(self.reads.get() + 1);
        self.blobs.read(id)
    }
}

#[test]
fn an_ordinary_sync_reads_only_the_merged_metadata() {
    // The tablet is behind; origin has only view-state churn on 1000
    // PDFs. Every document is decided by blob ids, and nothing is near
    // the trash, so the only reads are the merged `.metadata` the tree
    // rules need: one per document. (Deciding edits eagerly read four
    // times as much.)
    let mut lib = Lib::default();
    let docs: Vec<(u32, D)> = library(1000);
    let read: Vec<(u32, D)> = docs
        .iter()
        .map(|(n, d)| (*n, d.clone().scroll(*n)))
        .collect();
    let (b, o, t) = three(&mut lib, &refs(&docs), &refs(&docs), &refs(&read));
    let counting = Counting {
        blobs: &lib.blobs,
        reads: std::cell::Cell::new(0),
    };
    let device = DeviceName::new(DEVICE).unwrap();
    let consent = BTreeSet::new();
    let params = MergeParams {
        rules: MirrorRules::latest(),
        device_name: &device,
        date: DATE,
        allow_deletions: false,
        thresholds: GuardThresholds::default(),
        consented_deletions: &consent,
    };
    let outcome = merge(
        &MergeInput {
            base: &b,
            ours: &o,
            theirs: &t,
        },
        &counting,
        &NoHistory,
        &params,
    )
    .unwrap();
    assert_eq!(outcome.docs.len(), 1000);
    assert_eq!(counting.reads.get(), 1000);
}

#[test]
fn a_missing_parent_comes_back_from_origins_own_version_without_history() {
    // The tablet deleted folder F; origin inked a notebook in F. The
    // notebook is kept, F is deleted (unchanged in origin), and F comes
    // back from origin's own version, not the root, even with no history.
    let mut lib = Lib::default();
    let folder = D::folder("F", "");
    let doc = D::notebook("Notes", u(2).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &folder)],
        &[],
        &[(1, &doc.clone().ink(2, "origin")), (2, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(2)], Resolution::Resurrected);
    assert_eq!(outcome.docs[&u(2)], version_of(&mut lib, &folder));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(2).as_str()));
}

#[test]
fn a_document_its_own_editor_trashed_through_a_folder_stays_in_the_trash() {
    // The tablet inked N in F, then trashed F: N stays in the trash, with
    // no note (the editing side's own tree has it on a trash path).
    let mut lib = Lib::default();
    let folder = D::folder("F", "");
    let doc = D::notebook("Notes", u(2).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &folder)],
        &[
            (1, &doc.clone().ink(2, "tablet")),
            (2, &folder.clone().trash()),
        ],
        &[(1, &doc), (2, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(2).as_str()));
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(TRASH_PARENT));
    assert!(outcome.notes.is_empty());
}

#[test]
fn a_pre_trash_parent_that_is_gone_is_not_used() {
    // Base: G, T in G, N in T. Origin trashed T, then deleted G; the
    // tablet inked N. T's pre-trash parent G is gone, so T stays in the
    // trash and N comes out to the root: never under a missing parent.
    let mut lib = Lib::default();
    let g = D::folder("G", "");
    let t_folder = D::folder("T", u(1).as_str());
    let doc = D::notebook("Notes", u(2).as_str(), 1, "base");
    let (b, o, t) = three(
        &mut lib,
        &[(1, &g), (2, &t_folder), (3, &doc)],
        &[(1, &g), (2, &t_folder), (3, &doc.clone().ink(2, "tablet"))],
        &[(2, &t_folder.clone().trash()), (3, &doc)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert!(!outcome.docs.contains_key(&u(1)));
    assert_eq!(lib.md(&outcome, &u(2))["parent"], json!(TRASH_PARENT));
    assert_eq!(lib.md(&outcome, &u(3))["parent"], json!(""));
    assert_eq!(tree_violations(&merged_items(&lib, &outcome)), vec![]);
}

#[test]
fn a_move_against_a_deletion_is_kept() {
    let mut lib = Lib::default();
    let folder = D::folder("F", "");
    let doc = D::pdf("Paper", "", 1);
    let (b, o, t) = three(
        &mut lib,
        &[(1, &doc), (2, &folder)],
        &[(1, &doc.clone().move_to(u(2).as_str())), (2, &folder)],
        &[(2, &folder)],
    );
    let outcome = run(&lib, &b, &o, &t);
    assert_eq!(outcome.resolutions[&u(1)], Resolution::Kept(Side::Ours));
    assert_eq!(lib.md(&outcome, &u(1))["parent"], json!(u(2).as_str()));
}

#[test]
fn a_restore_heavy_merge_is_fast() {
    // 500 folders trashed in origin, each holding a notebook the tablet
    // inked, plus 1500 other inked notebooks: 500 restores.
    let mut lib = Lib::default();
    let folders: Vec<(u32, D)> = (1..=500u32)
        .map(|n| (n, D::folder(&format!("F{n}"), "")))
        .collect();
    let notebooks: Vec<(u32, D)> = (1..=2000u32)
        .map(|n| {
            let parent = if n <= 500 {
                u(n).to_string()
            } else {
                String::new()
            };
            (
                10_000 + n,
                D::notebook(&format!("N{n}"), &parent, 1, "base"),
            )
        })
        .collect();
    let base: Vec<(u32, &D)> = refs(&folders).into_iter().chain(refs(&notebooks)).collect();
    let inked: Vec<(u32, D)> = notebooks
        .iter()
        .map(|(n, d)| (*n, d.clone().ink(2, &format!("tablet {n}"))))
        .collect();
    let trashed: Vec<(u32, D)> = folders
        .iter()
        .map(|(n, d)| (*n, d.clone().trash()))
        .collect();
    let ours: Vec<(u32, &D)> = refs(&folders).into_iter().chain(refs(&inked)).collect();
    let theirs: Vec<(u32, &D)> = refs(&trashed).into_iter().chain(refs(&notebooks)).collect();
    let (b, o, t) = three(&mut lib, &base, &ours, &theirs);
    let started = std::time::Instant::now();
    let outcome = run(&lib, &b, &o, &t);
    let elapsed = started.elapsed();
    let restores = outcome
        .notes
        .iter()
        .filter(|note| matches!(note.kind, NoteKind::RestoredFromTrash { .. }))
        .count();
    assert_eq!(restores, 500);
    let merged = merged_items(&lib, &outcome);
    assert!((1..=500).all(|n| trash_path(&merged, &u(10_000 + n)).is_none()));
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "merge took {elapsed:?}"
    );
}

#[test]
fn the_restore_note_does_not_claim_the_item_itself_was_edited() {
    let note = Note {
        doc: u(1),
        kind: NoteKind::RestoredFromTrash {
            parent: String::new(),
        },
    };
    assert_eq!(
        note.to_string(),
        format!(
            "{}: moved out of the trash to the root, so an edit is not lost",
            u(1)
        )
    );
}

#[test]
fn unchanged_is_the_per_document_tables_no_change() {
    let mut lib = Lib::default();
    let rules = MirrorRules::latest();
    let skeleton = D::skeleton("Imported", "").tags(&["a", "b"]);
    let base = skeleton.build(&mut lib);
    let check = |lib: &mut Lib, side: &D| {
        let side = side.build(lib);
        unchanged(rules, &u(1), &base, &side, &lib.blobs).unwrap()
    };
    // Opened (materialised), read, tags reordered: no change.
    assert!(check(
        &mut lib,
        &opened(&skeleton, 100, 3).tags(&["a", "b"])
    ));
    assert!(check(
        &mut lib,
        &skeleton.clone().set("lastOpened", json!("9"))
    ));
    assert!(check(&mut lib, &skeleton.clone().tags(&["b", "a"])));
    // Renamed, inked, tagged: changes.
    assert!(!check(&mut lib, &skeleton.clone().rename("Other")));
    assert!(!check(
        &mut lib,
        &opened(&skeleton, 100, 3).tags(&["a", "b"]).ink(101, "ink")
    ));
    assert!(!check(&mut lib, &skeleton.clone().tags(&["a"])));
    // Never for an absent side.
    assert!(!unchanged(rules, &u(1), &base, &DocVersion::default(), &lib.blobs).unwrap());
}
