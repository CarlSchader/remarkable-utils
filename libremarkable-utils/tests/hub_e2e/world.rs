//! The end-to-end world: a bare origin, fake tablets and one local cache
//! per tablet (each tablet syncs from its own "computer"), all in a
//! scratch directory. Git is hermetic: `GIT_CONFIG_GLOBAL=/dev/null`,
//! `GIT_CONFIG_NOSYSTEM=1`, no config from the environment, SHA-1, a
//! private `HOME`, an explicit identity. Every document, name and id is
//! fabricated.
//!
//! The tablet "user" edits files the way xochitl does (ink appends stroke
//! markers to a page's `.rm` file, renames rewrite `.metadata`, ...), and
//! every ink marker ever drawn is remembered unless the user deliberately
//! deleted the page holding it on the tablet that deleted it: the ink
//! oracle requires every remembered marker to survive on origin and on
//! every tablet after convergence.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use libremarkable_utils::error::{Error, Result};
use libremarkable_utils::git::{Git, ObjectKind};
use libremarkable_utils::hub::cycle::{
    BackupReport, CycleError, DryRunReport, Hub, HubOptions, MainUpdate, StatusReport, SyncReport,
};
use libremarkable_utils::hub::device::{DeviceError, FakeTablet, LockRequest, Tablet};
use libremarkable_utils::hub::model::{
    self, Classification, DeviceName, DocFile, DocId, JsonFile, MirrorRules, TabletState,
};
use libremarkable_utils::progress::NoProgress;
use serde_json::{Value, json};
use test_support::Scratch;

pub const DATE: &str = "2026-11-09";
pub const MODEL: &str = "reMarkable Fake";
pub const FIRMWARE: &str = "3.28.0";
const RM_HEADER: &str = "reMarkable .lines file, version=6          \n";

pub fn rules() -> &'static MirrorRules {
    MirrorRules::latest()
}

/// A fabricated document id.
pub fn uuid(n: u64) -> DocId {
    DocId::new(format!("{:08x}-0000-4000-8000-{:012x}", 0x8c1f_0b9e_u32, n)).unwrap()
}

/// A fabricated page id.
pub fn page_id(n: u64) -> String {
    format!("9d2e1c0f-0000-4000-8000-{n:012x}")
}

/// What a fresh scratch directory is called.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rmu-hub-e2e-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ))
}

/// Copy a directory tree (regular files and directories only).
pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    fs::read_dir(from).unwrap().for_each(|entry| {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).unwrap();
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path()).unwrap(), &target).unwrap();
        }
    });
}

/// The hermetic environment of every git process.
pub fn hermetic_git(home: &Path) -> Git {
    [
        ("GIT_CONFIG_GLOBAL", PathBuf::from("/dev/null")),
        ("GIT_CONFIG_NOSYSTEM", PathBuf::from("1")),
        ("GIT_CONFIG_COUNT", PathBuf::from("0")),
        ("GIT_CONFIG_PARAMETERS", PathBuf::new()),
        ("GIT_DEFAULT_HASH", PathBuf::from("sha1")),
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.to_path_buf()),
        ("GIT_AUTHOR_NAME", PathBuf::from("Test Author")),
        ("GIT_AUTHOR_EMAIL", PathBuf::from("author@example.invalid")),
        ("GIT_COMMITTER_NAME", PathBuf::from("Test Committer")),
        (
            "GIT_COMMITTER_EMAIL",
            PathBuf::from("committer@example.invalid"),
        ),
    ]
    .into_iter()
    .fold(Git::new(), |git, (key, value)| git.env(key, value))
}

/// One tablet and the computer that syncs it.
pub struct Device {
    pub name: String,
    pub tablet: Arc<FakeTablet>,
    /// The fake's root (`xochitl/`, `state/`).
    pub dir: PathBuf,
    /// This computer's cache directory.
    pub cache: PathBuf,
}

impl Device {
    fn open(dir: PathBuf, cache: PathBuf, name: &str) -> Self {
        let tablet = FakeTablet::builder(&dir)
            .device_info(MODEL, FIRMWARE)
            .build()
            .unwrap();
        Self {
            name: name.to_string(),
            tablet: Arc::new(tablet),
            dir,
            cache,
        }
    }

    pub fn xochitl(&self) -> &Path {
        self.tablet.xochitl_dir()
    }

    pub fn state(&self) -> Option<TabletState> {
        fs::read(self.tablet.paths().state_file())
            .ok()
            .map(|bytes| TabletState::parse(&bytes).unwrap())
    }

    fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.tablet.read_file(path)
    }

    fn json(&self, path: &str) -> Option<Value> {
        self.file(path)
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }

    fn put_json(&self, path: &str, value: &Value) {
        self.tablet
            .write_file(
                path,
                serde_json::to_string_pretty(value).unwrap().as_bytes(),
            )
            .unwrap();
    }

    pub fn metadata(&self, doc: &DocId) -> Option<Value> {
        self.json(&format!("{doc}.metadata"))
    }

    pub fn content(&self, doc: &DocId) -> Option<Value> {
        self.json(&format!("{doc}.content"))
    }

    /// Every mirrored file on the tablet, per document.
    pub fn docs(&self) -> BTreeMap<DocId, BTreeMap<DocFile, Vec<u8>>> {
        walk(self.xochitl(), self.xochitl())
            .into_iter()
            .filter_map(|(path, bytes)| match rules().classify(&path).unwrap() {
                Classification::Mirrored(file) => Some((file, bytes)),
                Classification::Excluded(_) => None,
            })
            .fold(BTreeMap::new(), |mut docs, (file, bytes)| {
                docs.entry(file.doc)
                    .or_insert_with(BTreeMap::new)
                    .insert(file.file, bytes);
                docs
            })
    }

    /// Every document id on the tablet with its visible name.
    pub fn names(&self) -> BTreeMap<DocId, String> {
        self.docs()
            .iter()
            .filter_map(|(doc, files)| {
                let metadata: Value =
                    serde_json::from_slice(files.get(&DocFile::Metadata)?).ok()?;
                Some((doc.clone(), metadata["visibleName"].as_str()?.to_string()))
            })
            .collect()
    }
}

/// Every regular file below `dir`, relative to `root`.
fn walk(root: &Path, dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map(|entries| entries.map(|entry| entry.unwrap()).collect())
        .unwrap_or_default();
    entries.sort_by_key(|entry| entry.file_name());
    entries
        .into_iter()
        .flat_map(|entry| {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(root, &path)
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                vec![(relative, fs::read(&path).unwrap())]
            } else {
                Vec::new()
            }
        })
        .collect()
}

/// Ink markers: every stroke drawn, unless deliberately deleted.
#[derive(Default)]
pub struct Inks {
    pub expected: BTreeSet<String>,
    counter: u64,
}

/// A fabricated document before it is written to a tablet.
pub enum Kind {
    Folder,
    Notebook { pages: u64 },
    Pdf,
}

/// The world (see the module docs).
pub struct World {
    /// Everything lives here; removed on drop.
    scratch: Scratch,
    pub root: PathBuf,
    pub git: Git,
    pub origin: String,
    pub devices: Vec<Device>,
    pub inks: Mutex<Inks>,
    ids: Mutex<u64>,
    pub options: HubOptions,
}

impl World {
    /// A world with `tablets` fake tablets and an initialised origin.
    pub fn new(tablets: usize) -> Self {
        Self::at(Scratch::new("hub-e2e"), tablets, true)
    }

    fn at(scratch: Scratch, tablets: usize, init: bool) -> Self {
        let root = scratch.path().to_path_buf();
        fs::create_dir_all(root.join("home")).unwrap();
        let git = hermetic_git(&root.join("home"));
        let origin = root.join("origin.git").to_string_lossy().into_owned();
        let devices = (0..tablets)
            .map(|i| {
                Device::open(
                    root.join(format!("tablet-{i}")),
                    root.join(format!("computer-{i}")).join("cache"),
                    &format!("Tablet {}", (b'A' + i as u8) as char),
                )
            })
            .collect();
        let world = Self {
            scratch,
            root,
            git,
            origin,
            devices,
            inks: Mutex::new(Inks::default()),
            ids: Mutex::new(1),
            options: HubOptions {
                date: Some(DATE.to_string()),
                ..HubOptions::default()
            },
        };
        if init {
            world.hub(0).init(&world.origin).unwrap();
        }
        world
    }

    /// A copy of this world in a new directory: origin, tablets (files,
    /// hub state) and caches; ink bookkeeping included. The fakes start
    /// fresh (xochitl running, a new boot id, no faults).
    pub fn duplicate(&self) -> Self {
        let scratch = Scratch::new("hub-e2e");
        copy_dir(&self.root, scratch.path());
        let copy = Self::at(scratch, self.devices.len(), false);
        let inks = self.inks.lock().unwrap();
        *copy.inks.lock().unwrap() = Inks {
            expected: inks.expected.clone(),
            counter: inks.counter,
        };
        drop(inks);
        let ids = *self.ids.lock().unwrap();
        *copy.ids.lock().unwrap() = ids;
        copy
    }

    pub fn device(&self, i: usize) -> &Device {
        &self.devices[i]
    }

    pub fn tablet(&self, i: usize) -> &FakeTablet {
        &self.devices[i].tablet
    }

    /// A hub for one rmu run on tablet `i`'s computer.
    pub fn hub(&self, i: usize) -> Hub<'static> {
        self.hub_with(i, self.options.clone())
    }

    pub fn hub_with(&self, i: usize, options: HubOptions) -> Hub<'static> {
        static PROGRESS: NoProgress = NoProgress;
        Hub::new(
            self.git.clone(),
            self.devices[i].cache.clone(),
            LockRequest::for_this_process("test-host"),
            &PROGRESS,
        )
        .with_options(HubOptions {
            origin: Some(self.origin.clone()),
            ..options
        })
    }

    pub fn clone_tablet(&self, i: usize) -> Result<SyncReport> {
        let name = DeviceName::new(self.devices[i].name.clone()).unwrap();
        self.hub(i)
            .clone_tablet(self.tablet(i), &self.origin, Some(name), false)
    }

    pub fn reattach(&self, i: usize) -> Result<SyncReport> {
        self.hub(i)
            .clone_tablet(self.tablet(i), &self.origin, None, true)
    }

    pub fn sync(&self, i: usize) -> Result<SyncReport> {
        self.hub(i).sync(self.tablet(i))
    }

    pub fn sync_with(&self, i: usize, options: HubOptions) -> Result<SyncReport> {
        self.hub_with(i, options).sync(self.tablet(i))
    }

    pub fn backup(&self, i: usize) -> Result<BackupReport> {
        self.hub(i).backup(self.tablet(i))
    }

    pub fn status(&self, i: usize) -> Result<StatusReport> {
        self.hub(i).status(self.tablet(i))
    }

    pub fn dry_run(&self, i: usize) -> Result<DryRunReport> {
        self.hub(i).dry_run(self.tablet(i))
    }

    /// Sync every tablet in turn until a whole round changes nothing.
    pub fn converge(&self) {
        let quiet = (0..6).any(|_| {
            // Every tablet syncs in every round.
            let rounds: Vec<bool> = (0..self.devices.len())
                .map(|i| {
                    let report = self.sync(i).unwrap_or_else(|e| panic!("sync {i}: {e}"));
                    report.update == MainUpdate::UpToDate && report.applied.is_empty()
                })
                .collect();
            rounds.into_iter().all(|quiet| quiet)
        });
        assert!(quiet, "the tablets did not converge");
        self.check();
    }

    // ---- oracles ---------------------------------------------------------------

    /// Origin `main`'s documents (present ones), per mirrored file.
    pub fn origin_docs(&self) -> BTreeMap<DocId, BTreeMap<DocFile, Vec<u8>>> {
        let repo = self.git.open(Path::new(&self.origin)).unwrap();
        let listing =
            model::tree_listing(repo.ls_tree_recursive("refs/heads/main").unwrap()).unwrap();
        let tree = rules().group_tree(listing).unwrap();
        let mut cat = repo.cat_file().unwrap();
        let docs = tree
            .docs
            .iter()
            .filter(|(_, version)| version.is_present())
            .map(|(doc, version)| {
                let files = version
                    .files
                    .iter()
                    .map(|(file, entry)| {
                        let (kind, bytes) = cat.read(&entry.id).unwrap().unwrap();
                        assert_eq!(kind, ObjectKind::Blob);
                        (file.clone(), bytes)
                    })
                    .collect();
                (doc.clone(), files)
            })
            .collect();
        cat.finish().unwrap();
        docs
    }

    /// Everything the end of a run must leave: xochitl running, no lock,
    /// no journal; the tablets equal to origin (`.metadata`/`.content`
    /// once normalised, other files byte for byte, leftovers included);
    /// every remembered ink marker on origin and on every tablet.
    pub fn check(&self) {
        let origin = self.origin_docs();
        self.devices.iter().for_each(|device| {
            assert!(
                device.tablet.is_running(),
                "{}: xochitl is down",
                device.name
            );
            assert!(
                !Path::new(&device.tablet.paths().lock_dir()).exists(),
                "{}: the lock is still held",
                device.name
            );
            let state = device.state().expect("attached");
            assert!(
                state.journal.is_none(),
                "{}: a journal is left",
                device.name
            );
            assert_equivalent(&device.name, &device.docs(), &origin);
        });
        self.check_ink(&origin);
    }

    /// The ink oracle alone.
    pub fn check_ink(&self, origin: &BTreeMap<DocId, BTreeMap<DocFile, Vec<u8>>>) {
        let on_origin = ink_markers(origin);
        let expected = self.inks.lock().unwrap().expected.clone();
        let lost: Vec<&String> = expected.difference(&on_origin).collect();
        assert!(lost.is_empty(), "ink lost from origin: {lost:?}");
        self.devices.iter().for_each(|device| {
            let on_tablet = ink_markers(&device.docs());
            let lost: Vec<&String> = expected.difference(&on_tablet).collect();
            assert!(lost.is_empty(), "{}: ink lost: {lost:?}", device.name);
        });
    }

    /// Conflict copies on origin (`... (conflict ...)` names).
    pub fn conflicts(&self) -> Vec<(DocId, String)> {
        self.origin_docs()
            .iter()
            .filter_map(|(doc, files)| {
                let metadata: Value =
                    serde_json::from_slice(files.get(&DocFile::Metadata)?).ok()?;
                let name = metadata["visibleName"].as_str()?.to_string();
                name.contains(" (conflict ").then(|| (doc.clone(), name))
            })
            .collect()
    }

    // ---- the user ----------------------------------------------------------------

    fn next_id(&self) -> u64 {
        let mut ids = self.ids.lock().unwrap();
        *ids += 1;
        *ids
    }

    /// A new document on tablet `i`; returns its id.
    pub fn create(&self, i: usize, kind: Kind, name: &str, parent: &str) -> DocId {
        let doc = uuid(self.next_id());
        let device = self.device(i);
        let (type_, content) = match &kind {
            Kind::Folder => ("CollectionType", json!([])),
            Kind::Notebook { pages } => {
                let pages: Vec<Value> = (0..*pages)
                    .map(|n| self.page_entry(&page_id(self.next_id()), n as usize, None))
                    .collect();
                ("DocumentType", notebook_content(pages))
            }
            Kind::Pdf => (
                "DocumentType",
                serde_json::from_str(&libremarkable_utils::xochitl::document_content_json("pdf"))
                    .unwrap(),
            ),
        };
        device.put_json(&format!("{doc}.metadata"), &metadata(name, parent, type_));
        device.put_json(&format!("{doc}.content"), &content);
        match kind {
            Kind::Notebook { .. } => {
                let pages = pages_of(&content);
                pages.iter().for_each(|page| self.ink(i, &doc, page));
            }
            Kind::Pdf => device
                .tablet
                .write_file(
                    &format!("{doc}.pdf"),
                    format!("%PDF-1.4 fake {doc}\n").as_bytes(),
                )
                .unwrap(),
            Kind::Folder => {}
        }
        doc
    }

    fn page_entry(&self, id: &str, position: usize, redir: Option<usize>) -> Value {
        let mut entry = json!({
            "id": id,
            "idx": { "timestamp": "1:2", "value": idx(position) },
            "template": { "timestamp": "1:3", "value": "Blank" },
        });
        if let Some(redir) = redir {
            entry["redir"] = json!({ "timestamp": "1:2", "value": redir });
            entry.as_object_mut().unwrap().remove("template");
        }
        entry
    }

    /// Draw on a page: append a fresh stroke marker to its `.rm` file and
    /// update the page entry and `lastModified`, as xochitl would.
    pub fn ink(&self, i: usize, doc: &DocId, page: &str) {
        let device = self.device(i);
        let marker = {
            let mut inks = self.inks.lock().unwrap();
            inks.counter += 1;
            let marker = format!("ink-{}-{}", device.name.replace(' ', "_"), inks.counter);
            inks.expected.insert(marker.clone());
            marker
        };
        let path = format!("{doc}/{page}.rm");
        let mut bytes = device
            .file(&path)
            .unwrap_or_else(|| RM_HEADER.as_bytes().to_vec());
        bytes.extend_from_slice(format!("stroke {marker}\n").as_bytes());
        device.tablet.write_file(&path, &bytes).unwrap();
        if let Some(mut content) = device.content(doc) {
            if let Some(pages) = content["cPages"]["pages"].as_array_mut() {
                pages
                    .iter_mut()
                    .filter(|entry| entry["id"] == page)
                    .for_each(|entry| entry["modifed"] = json!(marker.clone()));
            }
            device.put_json(&format!("{doc}.content"), &content);
        }
        self.touch_modified(i, doc);
    }

    fn touch_modified(&self, i: usize, doc: &DocId) {
        let device = self.device(i);
        if let Some(mut metadata) = device.metadata(doc) {
            let stamp = 1_767_312_000_000u64 + self.next_id();
            metadata["lastModified"] = json!(stamp.to_string());
            device.put_json(&format!("{doc}.metadata"), &metadata);
        }
    }

    /// Insert an inked note page at the end of a notebook; its id.
    pub fn insert_page(&self, i: usize, doc: &DocId) -> String {
        let device = self.device(i);
        let mut content = device.content(doc).expect("a notebook");
        let page = page_id(self.next_id());
        let pages = content["cPages"]["pages"].as_array_mut().unwrap();
        let entry = self.page_entry(&page, pages.len(), None);
        pages.push(entry);
        content["pageCount"] = json!(pages_of(&content).len());
        device.put_json(&format!("{doc}.content"), &content);
        self.ink(i, doc, &page);
        page
    }

    /// The page ids of a document on tablet `i`, in list order.
    pub fn pages(&self, i: usize, doc: &DocId) -> Vec<String> {
        pages_of(&self.device(i).content(doc).expect("a document"))
    }

    fn edit_metadata(&self, i: usize, doc: &DocId, edit: impl FnOnce(&mut Value)) {
        let device = self.device(i);
        let mut metadata = device.metadata(doc).expect("a document");
        edit(&mut metadata);
        device.put_json(&format!("{doc}.metadata"), &metadata);
    }

    pub fn rename(&self, i: usize, doc: &DocId, name: &str) {
        self.edit_metadata(i, doc, |metadata| metadata["visibleName"] = json!(name));
    }

    pub fn move_to(&self, i: usize, doc: &DocId, parent: &str) {
        self.edit_metadata(i, doc, |metadata| metadata["parent"] = json!(parent));
    }

    pub fn trash(&self, i: usize, doc: &DocId) {
        self.move_to(i, doc, "trash");
    }

    /// Opening a document only moves view state (it must never count).
    pub fn read(&self, i: usize, doc: &DocId) {
        let stamp = self.next_id();
        self.edit_metadata(i, doc, |metadata| {
            metadata["lastOpened"] = json!(stamp.to_string());
            metadata["lastOpenedPage"] = json!(stamp % 3);
        });
        let device = self.device(i);
        if let Some(mut content) = device.content(doc)
            && content.is_object()
        {
            content["zoomMode"] = json!(format!("fit-{stamp}"));
            content["extraMetadata"] = json!({ "LastTool": format!("pen-{stamp}") });
            device.put_json(&format!("{doc}.content"), &content);
        }
    }

    /// Open an imported PDF for the first time: xochitl materialises one
    /// page entry per PDF page, with page ids of its own.
    pub fn open_pdf(&self, i: usize, doc: &DocId, pdf_pages: usize) {
        let device = self.device(i);
        let mut content = device.content(doc).expect("a pdf");
        let entries: Vec<Value> = (0..pdf_pages)
            .map(|n| {
                let id = format!("{}-{n}", uuid::Uuid::new_v4());
                self.page_entry(&id, n, Some(n))
            })
            .collect();
        content["cPages"]["pages"] = json!(entries);
        content["cPages"]["original"] = json!({ "timestamp": "1:1", "value": pdf_pages });
        content["cPages"]["uuids"] =
            json!([{ "first": uuid::Uuid::new_v4().to_string(), "second": 1 }]);
        content["pageCount"] = json!(pdf_pages);
        content["sizeInBytes"] = json!(format!("{}", 1000 + i));
        content["tags"] = json!([]);
        content["customZoomScale"] = json!(1);
        device.put_json(&format!("{doc}.content"), &content);
        self.edit_metadata(i, doc, |metadata| metadata["new"] = json!(false));
    }

    /// Delete a document's files outright (as emptying the trash does);
    /// its ink is deliberately gone.
    pub fn delete(&self, i: usize, doc: &DocId) {
        let device = self.device(i);
        self.forget_ink(device.docs().get(doc));
        walk(device.xochitl(), device.xochitl())
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| {
                path.starts_with(&format!("{doc}.")) || path.starts_with(&format!("{doc}/"))
            })
            .for_each(|path| device.tablet.remove_file(&path).unwrap());
        let _ = fs::remove_dir_all(device.xochitl().join(doc.as_str()));
    }

    /// Empty the trash: delete every item whose path reaches it.
    pub fn empty_trash(&self, i: usize) -> Vec<DocId> {
        let device = self.device(i);
        let parents: BTreeMap<DocId, String> = device
            .docs()
            .keys()
            .filter_map(|doc| {
                Some((
                    doc.clone(),
                    device.metadata(doc)?["parent"].as_str()?.to_string(),
                ))
            })
            .collect();
        let trashed: Vec<DocId> = parents
            .keys()
            .filter(|doc| {
                std::iter::successors(Some((*doc).clone()), |current| {
                    DocId::new(parents.get(current)?.as_str()).ok()
                })
                .take(parents.len() + 1)
                .any(|member| parents.get(&member).is_some_and(|p| p == "trash"))
            })
            .cloned()
            .collect();
        trashed.iter().for_each(|doc| self.delete(i, doc));
        trashed
    }

    /// Remove an inked page from a notebook: its ink is deliberately gone.
    pub fn remove_page(&self, i: usize, doc: &DocId, page: &str) {
        let device = self.device(i);
        let path = format!("{doc}/{page}.rm");
        if let Some(bytes) = device.file(&path) {
            let mut inks = self.inks.lock().unwrap();
            markers(&bytes).iter().for_each(|marker| {
                inks.expected.remove(marker);
            });
        }
        device.tablet.remove_file(&path).unwrap();
        let mut content = device.content(doc).unwrap();
        content["cPages"]["pages"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["id"] != page);
        device.put_json(&format!("{doc}.content"), &content);
    }

    fn forget_ink(&self, files: Option<&BTreeMap<DocFile, Vec<u8>>>) {
        let mut inks = self.inks.lock().unwrap();
        files.into_iter().flatten().for_each(|(file, bytes)| {
            if matches!(file, DocFile::Page(_)) {
                markers(bytes).iter().for_each(|marker| {
                    inks.expected.remove(marker);
                });
            }
        });
    }

    /// Break a document's `.metadata` (it must be quarantined).
    pub fn corrupt(&self, i: usize, doc: &DocId) {
        self.device(i)
            .tablet
            .write_file(&format!("{doc}.metadata"), b"{ not json")
            .unwrap();
    }
}

fn idx(position: usize) -> String {
    format!(
        "b{}{}",
        (b'a' + (position / 26) as u8) as char,
        (b'a' + (position % 26) as u8) as char
    )
}

pub fn metadata(name: &str, parent: &str, type_: &str) -> Value {
    json!({
        "createdTime": "1767225600000",
        "lastModified": "1767312000000",
        "lastOpened": "0",
        "lastOpenedPage": 0,
        "new": false,
        "parent": parent,
        "pinned": false,
        "source": "",
        "type": type_,
        "visibleName": name,
    })
}

fn notebook_content(pages: Vec<Value>) -> Value {
    let count = pages.len();
    json!({
        "cPages": {
            "lastOpened": { "timestamp": "1:1", "value": "" },
            "original": { "timestamp": "0:0", "value": -1 },
            "pages": pages,
            "uuids": [{ "first": "0f0e0d0c-0000-4000-8000-00000000a001", "second": 1 }],
        },
        "coverPageNumber": 0,
        "customZoomCenterX": 0,
        "customZoomCenterY": 936,
        "customZoomOrientation": "portrait",
        "customZoomPageHeight": 1872,
        "customZoomPageWidth": 1404,
        "customZoomScale": 1,
        "documentMetadata": {},
        "extraMetadata": {},
        "fileType": "notebook",
        "fontName": "",
        "formatVersion": 2,
        "lineHeight": -1,
        "orientation": "portrait",
        "pageCount": count,
        "pageTags": [],
        "sizeInBytes": "1024",
        "tags": [],
        "textAlignment": "justify",
        "textScale": 1,
        "zoomMode": "bestFit",
    })
}

pub fn pages_of(content: &Value) -> Vec<String> {
    content["cPages"]["pages"]
        .as_array()
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| page["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn markers(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| line.strip_prefix("stroke "))
        .map(str::to_string)
        .collect()
}

/// Every ink marker in some page of `docs`.
pub fn ink_markers(docs: &BTreeMap<DocId, BTreeMap<DocFile, Vec<u8>>>) -> BTreeSet<String> {
    docs.values()
        .flat_map(|files| files.iter())
        .filter(|(file, _)| matches!(file, DocFile::Page(_)))
        .flat_map(|(_, bytes)| markers(bytes))
        .collect()
}

/// A tablet's documents equal origin's: the same documents and files,
/// `.metadata`/`.content` equal once normalised, others byte for byte.
pub fn assert_equivalent(
    who: &str,
    tablet: &BTreeMap<DocId, BTreeMap<DocFile, Vec<u8>>>,
    origin: &BTreeMap<DocId, BTreeMap<DocFile, Vec<u8>>>,
) {
    let (mine, theirs): (BTreeSet<&DocId>, BTreeSet<&DocId>) =
        (tablet.keys().collect(), origin.keys().collect());
    assert_eq!(
        mine.difference(&theirs).collect::<Vec<_>>(),
        Vec::<&&DocId>::new(),
        "{who}: documents origin lacks"
    );
    assert_eq!(
        theirs.difference(&mine).collect::<Vec<_>>(),
        Vec::<&&DocId>::new(),
        "{who}: documents the tablet lacks"
    );
    tablet.iter().for_each(|(doc, files)| {
        let other = &origin[doc];
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            other.keys().collect::<Vec<_>>(),
            "{who}: files of {doc}"
        );
        files.iter().for_each(|(file, bytes)| {
            let kind = match file {
                DocFile::Metadata => Some(JsonFile::Metadata),
                DocFile::Content => Some(JsonFile::Content),
                _ => None,
            };
            match kind {
                Some(kind) => {
                    let parse = |bytes: &[u8]| -> Value {
                        rules().normalise(kind, &serde_json::from_slice(bytes).unwrap())
                    };
                    assert_eq!(
                        parse(bytes),
                        parse(&other[file]),
                        "{who}: {doc} {}",
                        file.repo_name()
                    );
                }
                None => assert!(bytes == &other[file], "{who}: {doc} {}", file.repo_name()),
            }
        });
    });
}

/// Whether an error is the tablet lock held by another (dead) run.
pub fn is_locked(error: &Error) -> bool {
    matches!(error, Error::HubDevice(DeviceError::Locked(_)))
}

pub fn is_cycle(error: &Error, expected: fn(&CycleError) -> bool) -> bool {
    matches!(error, Error::HubCycle(e) if expected(e))
}

/// The fake never reports a broken device contract.
pub fn no_contract_violation<T: std::fmt::Debug>(result: &Result<T>) {
    if let Err(Error::HubDevice(DeviceError::ContractViolation(what))) = result {
        panic!("device contract violated: {what}");
    }
}

/// Read the whole of a reader (helper for tests that stream).
pub fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    out
}

/// Tablet trait object of a device.
pub fn as_tablet(device: &Device) -> &dyn Tablet {
    &*device.tablet
}
