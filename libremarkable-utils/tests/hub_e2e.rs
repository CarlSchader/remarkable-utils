//! End-to-end tests of the `rmu hub` sync cycle (`hub::cycle`): two or
//! three fake tablets, a bare origin and one local cache per tablet in a
//! scratch directory. No network, no real tablet, hermetic git (see
//! `hub_e2e/world.rs`). Every scenario ends with the same oracles
//! (`World::check`): the tablets converge on origin, no ink is lost,
//! xochitl runs, no lock or journal is left.

#[path = "hub_e2e/world.rs"]
mod world;

use libremarkable_utils::hub::cycle::MainUpdate;
use world::{Kind, World};

#[test]
fn clone_into_an_empty_origin_then_a_second_tablet() {
    let world = World::new(2);
    let folder = world.create(0, Kind::Folder, "Projects", "");
    let notes = world.create(0, Kind::Notebook { pages: 2 }, "Notes", folder.as_str());
    let paper = world.create(0, Kind::Pdf, "Paper", "");
    let report = world.clone_tablet(0).unwrap();
    assert!(
        matches!(report.update, MainUpdate::Merge(_)),
        "{:?}",
        report.update
    );
    assert!(report.applied.is_empty(), "{:?}", report.applied);
    let origin = world.origin_docs();
    assert!(
        origin.contains_key(&folder) && origin.contains_key(&notes) && origin.contains_key(&paper)
    );

    // The second tablet has a document of its own; it gets origin's and
    // origin gets its.
    let own = world.create(1, Kind::Notebook { pages: 1 }, "Mine", "");
    let report = world.clone_tablet(1).unwrap();
    assert_eq!(report.applied.len(), 3, "{report:?}");
    world.converge();
    assert!(world.origin_docs().contains_key(&own));
    assert!(world.conflicts().is_empty());
}

/// Two tablets attached to one origin, each with a little library.
fn two_tablets() -> (World, Library) {
    let world = World::new(2);
    let library = Library::create(&world, 0);
    world.clone_tablet(0).unwrap();
    world.clone_tablet(1).unwrap();
    world.converge();
    (world, library)
}

/// Fabricated documents created on one tablet.
struct Library {
    folder: libremarkable_utils::hub::model::DocId,
    notes: libremarkable_utils::hub::model::DocId,
    sketch: libremarkable_utils::hub::model::DocId,
    paper: libremarkable_utils::hub::model::DocId,
    archive: libremarkable_utils::hub::model::DocId,
    diary: libremarkable_utils::hub::model::DocId,
}

impl Library {
    fn create(world: &World, i: usize) -> Self {
        let folder = world.create(i, Kind::Folder, "Projects", "");
        let archive = world.create(i, Kind::Folder, "Archive", "");
        let notes = world.create(i, Kind::Notebook { pages: 2 }, "Notes", folder.as_str());
        let sketch = world.create(i, Kind::Notebook { pages: 1 }, "Sketch", "");
        let paper = world.create(i, Kind::Pdf, "Paper", folder.as_str());
        let diary = world.create(i, Kind::Notebook { pages: 2 }, "Diary", archive.as_str());
        Self {
            folder,
            notes,
            sketch,
            paper,
            archive,
            diary,
        }
    }
}

#[test]
fn edits_on_both_tablets_converge_without_losing_ink() {
    let (world, lib) = two_tablets();
    // A: ink, an inserted page, a rename, a move, a new folder.
    let notes_page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &notes_page);
    world.insert_page(0, &lib.sketch);
    world.rename(0, &lib.paper, "Paper (read)");
    let inbox = world.create(0, Kind::Folder, "Inbox", "");
    world.move_to(0, &lib.sketch, inbox.as_str());
    // B: ink elsewhere, a new notebook, a rename of another document,
    // reading (view state only). (Ink on the sketch would fork it: in
    // phase 1 a fork-class change on both sides forks the document.)
    let diary_page = world.pages(1, &lib.diary)[1].clone();
    world.ink(1, &lib.diary, &diary_page);
    let journal = world.create(
        1,
        Kind::Notebook { pages: 1 },
        "Journal",
        lib.folder.as_str(),
    );
    world.rename(1, &lib.archive, "Old stuff");
    world.read(1, &lib.notes);
    world.sync(0).unwrap();
    let report = world.sync(1).unwrap();
    assert!(matches!(report.update, MainUpdate::Merge(_)));
    world.converge();
    assert!(world.conflicts().is_empty(), "{:?}", world.conflicts());
    let b = world.device(1);
    assert_eq!(b.metadata(&lib.sketch).unwrap()["parent"], inbox.as_str());
    assert_eq!(
        b.metadata(&lib.paper).unwrap()["visibleName"],
        "Paper (read)"
    );
    assert_eq!(
        world.device(0).metadata(&lib.archive).unwrap()["visibleName"],
        "Old stuff"
    );
    assert!(world.device(0).metadata(&journal).is_some());
    // The sketch has A's inserted page.
    assert_eq!(world.pages(1, &lib.sketch).len(), 2);
}

#[test]
fn trash_empty_trash_and_deletions_converge() {
    let (world, lib) = two_tablets();
    // A trashes the folder; B inks a notebook inside it meanwhile: the
    // edit pulls it out of the trash.
    world.trash(0, &lib.folder);
    world.sync(0).unwrap();
    let page = world.pages(1, &lib.notes)[0].clone();
    world.ink(1, &lib.notes, &page);
    world.sync(1).unwrap();
    world.converge();
    assert_ne!(
        world.device(0).metadata(&lib.notes).unwrap()["parent"],
        "trash"
    );
    // A deletes the sketch outright; B empties a trash holding the paper.
    world.delete(0, &lib.sketch);
    world.trash(1, &lib.paper);
    world.empty_trash(1);
    world.sync(0).unwrap();
    world.sync(1).unwrap();
    world.converge();
    let origin = world.origin_docs();
    assert!(!origin.contains_key(&lib.sketch));
    assert!(!origin.contains_key(&lib.paper));
    assert!(world.conflicts().is_empty());
}

#[test]
fn a_deletion_loses_to_a_concurrent_edit() {
    let (world, lib) = two_tablets();
    world.delete(0, &lib.sketch);
    let page = world.pages(1, &lib.sketch)[0].clone();
    world.ink(1, &lib.sketch, &page);
    world.sync(0).unwrap();
    world.sync(1).unwrap();
    world.converge();
    assert!(world.device(0).metadata(&lib.sketch).is_some());
}

/// Both tablets ink the same page; the second to sync forks.
fn same_page_conflict() -> (World, Library) {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &page);
    world.ink(1, &lib.notes, &page);
    world.sync(0).unwrap();
    (world, lib)
}

#[test]
fn a_same_page_conflict_forks_once_with_a_deterministic_id() {
    let (world, lib) = same_page_conflict();
    let report = world.sync(1).unwrap();
    assert_eq!(report.forks.len(), 1, "{report:?}");
    let copy = report.forks[0].copy.clone();
    assert_eq!(report.forks[0].original, lib.notes);
    assert_eq!(report.forks[0].name, "Notes (conflict Tablet B 2026-11-09)");
    world.converge();
    assert_eq!(
        world.conflicts(),
        vec![(copy.clone(), report.forks[0].name.clone())]
    );
    // The original has origin's (A's) version on both tablets; B's ink
    // lives on in the copy, next to it.
    let b = world.device(1);
    assert_eq!(b.metadata(&copy).unwrap()["parent"], lib.folder.as_str());
    // Recomputing the same merge in an identical world gives the same id.
    let (again, _) = same_page_conflict();
    let report = again.sync(1).unwrap();
    assert_eq!(report.forks[0].copy, copy);
    // Status lists the open conflict copy until it is renamed.
    let status = world.status(0).unwrap();
    assert_eq!(status.open_conflicts.len(), 1);
    assert_eq!(status.open_conflicts[0].copy, copy);
    world.rename(0, &copy, "Notes, B's version");
    world.converge();
    assert!(world.status(1).unwrap().open_conflicts.is_empty());
}

#[test]
fn first_open_on_two_tablets_does_not_fork() {
    let (world, lib) = two_tablets();
    world.open_pdf(0, &lib.paper, 3);
    world.open_pdf(1, &lib.paper, 3);
    world.sync(0).unwrap();
    world.sync(1).unwrap();
    world.converge();
    assert!(world.conflicts().is_empty(), "{:?}", world.conflicts());
    // Origin's (A's) page ids won on both tablets.
    assert_eq!(world.pages(0, &lib.paper), world.pages(1, &lib.paper));
    assert_eq!(world.pages(1, &lib.paper).len(), 3);
}

#[test]
fn view_state_never_counts_and_stays_per_tablet() {
    let (world, lib) = two_tablets();
    world.read(0, &lib.notes);
    world.read(1, &lib.notes);
    let report = world.sync(0).unwrap();
    assert_eq!(report.update, MainUpdate::UpToDate, "{report:?}");
    let report = world.sync(1).unwrap();
    assert_eq!(report.update, MainUpdate::UpToDate, "{report:?}");
    // Each tablet keeps its own view state when origin's version comes in.
    let page = world.pages(0, &lib.notes)[1].clone();
    world.ink(0, &lib.notes, &page);
    let before = world.device(1).content(&lib.notes).unwrap()["zoomMode"].clone();
    world.sync(0).unwrap();
    world.sync(1).unwrap();
    assert_eq!(
        world.device(1).content(&lib.notes).unwrap()["zoomMode"],
        before
    );
    world.converge();
}

use libremarkable_utils::hub::cycle::{CycleError, HubOptions};
use libremarkable_utils::hub::device::Fault;
use std::path::Path;
use world::{is_cycle, is_locked};

#[test]
fn the_deletion_guard_needs_consent_which_other_tablets_honour() {
    let world = World::new(2);
    let shelf = world.create(0, Kind::Folder, "Shelf", "");
    let books: Vec<_> = (0..60)
        .map(|n| world.create(0, Kind::Pdf, &format!("Book {n}"), shelf.as_str()))
        .collect();
    world.clone_tablet(0).unwrap();
    world.clone_tablet(1).unwrap();
    world.converge();
    // B deletes 55 books: refused, nothing pushed, the tablet untouched.
    books[..55].iter().for_each(|book| world.delete(1, book));
    let before = world.origin_docs();
    let refused = world.sync(1).unwrap_err();
    assert!(
        is_cycle(&refused, |e| matches!(
            e,
            CycleError::DeletionsNeedConsent { from_origin, .. } if from_origin.len() == 55
        )),
        "{refused}"
    );
    assert_eq!(world.origin_docs().len(), before.len());
    assert!(world.tablet(1).is_running());
    // With consent the deletions go to origin, recorded in a trailer...
    let report = world
        .sync_with(
            1,
            HubOptions {
                allow_deletions: true,
                ..world.options.clone()
            },
        )
        .unwrap();
    assert_eq!(report.deletions.consented().len(), 55);
    // ...and A applies them without being asked again.
    let report = world.sync(0).unwrap();
    assert_eq!(report.applied.len(), 55);
    assert_eq!(report.deletions.exempt.len(), 55);
    world.converge();
    assert_eq!(world.device(0).docs().len(), 6);
    // Below the threshold no consent is needed.
    books[55..].iter().for_each(|book| world.delete(0, book));
    world.sync(0).unwrap();
    world.converge();
    assert_eq!(world.origin_docs().len(), 1);
}

#[test]
fn an_empty_tablet_never_wipes_origin() {
    let (world, lib) = two_tablets();
    [
        &lib.folder,
        &lib.notes,
        &lib.sketch,
        &lib.paper,
        &lib.archive,
        &lib.diary,
    ]
    .into_iter()
    .for_each(|doc| world.delete(1, doc));
    let refused = world.sync(1).unwrap_err();
    assert!(
        is_cycle(&refused, |e| matches!(
            e,
            CycleError::DeletionsNeedConsent {
                empty_snapshot: true,
                ..
            }
        )),
        "{refused}"
    );
    assert_eq!(world.origin_docs().len(), 6);
}

#[test]
fn a_quarantined_document_is_never_deleted_or_overwritten() {
    let (world, lib) = two_tablets();
    world.corrupt(0, &lib.sketch);
    // B renames the sketch and A syncs: A's broken copy stays as it is.
    world.rename(1, &lib.sketch, "Sketch v2");
    world.sync(1).unwrap();
    let report = world.sync(0).unwrap();
    assert!(report.quarantined.contains_key(&lib.sketch), "{report:?}");
    assert_eq!(
        world
            .device(0)
            .tablet
            .read_file(&format!("{}.metadata", lib.sketch))
            .unwrap(),
        b"{ not json"
    );
    // B deletes it: origin drops it, A keeps its quarantined files.
    world.delete(1, &lib.sketch);
    world.sync(1).unwrap();
    let report = world.sync(0).unwrap();
    assert!(report.quarantined.contains_key(&lib.sketch));
    assert!(
        world
            .device(0)
            .tablet
            .read_file(&format!("{}.metadata", lib.sketch))
            .is_some()
    );
    assert!(!world.origin_docs().contains_key(&lib.sketch));
    // Repaired, it is an edit against the version it had: kept over the
    // deletion, ink included.
    let device = world.device(0);
    device
        .tablet
        .write_file(
            &format!("{}.metadata", lib.sketch),
            serde_json::to_string(&world::metadata("Sketch (fixed)", "", "DocumentType"))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    world.sync(0).unwrap();
    world.converge();
    assert!(world.origin_docs().contains_key(&lib.sketch));
}

#[test]
fn edits_during_the_sync_are_skipped_then_picked_up() {
    let (world, lib) = two_tablets();
    // B renames the notes; A will apply that. While A's sync has the UI
    // up (before it stops xochitl again to apply), the user inks them on
    // A: that document is skipped, keeps its snapshot base, and the next
    // sync merges the ink with B's rename.
    world.rename(1, &lib.notes, "Notes (renamed on B)");
    world.sync(1).unwrap();
    let page = world.pages(0, &lib.notes)[0].clone();
    let target = lib.notes.clone();
    let device = world.device(0);
    // Find the step-13 arm: the first ArmWatchdog after the Stage.
    let probe = world.duplicate();
    probe.sync(0).unwrap();
    let ops = probe.tablet(0).ops();
    let stage = ops
        .iter()
        .position(|op| *op == libremarkable_utils::hub::device::OpKind::Stage)
        .expect("a stage");
    let marker = "stroke window-ink\n".to_string();
    world
        .inks
        .lock()
        .unwrap()
        .expected
        .insert("window-ink".to_string());
    let page_path = format!("{target}/{page}.rm");
    device.tablet.inject(Fault::EditBefore(
        stage + 2,
        Box::new(move |dir: &Path| {
            let path = dir.join(&page_path);
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(marker.as_bytes());
            std::fs::write(&path, bytes).unwrap();
        }),
    ));
    let report = world.sync(0).unwrap();
    assert_eq!(report.skipped, vec![lib.notes.clone()], "{report:?}");
    let state = device.state().unwrap();
    assert_eq!(
        state.base_overrides.get(&lib.notes),
        Some(&Some(report.snapshot.clone()))
    );
    // Still A's name: the rename was not applied over the edit.
    assert_eq!(device.metadata(&lib.notes).unwrap()["visibleName"], "Notes");
    world.converge();
    assert_eq!(
        world.device(0).metadata(&lib.notes).unwrap()["visibleName"],
        "Notes (renamed on B)"
    );
    assert!(world.conflicts().is_empty(), "{:?}", world.conflicts());
}

#[test]
fn a_rewound_or_replaced_origin_stops_the_sync() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &page);
    world.sync(0).unwrap();
    // Rewind origin's main to its first commit.
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    let first = repo
        .rev_list(&["refs/heads/main"], true)
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    repo.update_ref("refs/heads/main", &first, None).unwrap();
    let before = world.device(1).docs();
    let ops = world.tablet(1).ops().len();
    let error = world.sync(1).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::OriginRewound { .. })),
        "{error}"
    );
    assert_eq!(world.device(1).docs(), before);
    assert!(world.tablet(1).is_running());
    // Refused before the tablet was touched.
    assert!(
        world.tablet(1).ops()[ops..]
            .iter()
            .all(|op| !op.is_mutating())
    );
    // Replace origin with a fresh one: a different repo id.
    std::fs::remove_dir_all(&world.origin).unwrap();
    world.hub(0).init(&world.origin).unwrap();
    let error = world.sync(0).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::RepoMismatch { .. })),
        "{error}"
    );
    // Reattaching is the deliberate way back.
    world.reattach(0).unwrap();
    world.reattach(1).unwrap();
    world.converge();
}

#[test]
fn guards_refuse_paired_attached_and_unattached_tablets() {
    let world = World::new(1);
    let error = world.sync(0).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::NotAttached)),
        "{error}"
    );
    world.create(0, Kind::Notebook { pages: 1 }, "Notes", "");
    world.clone_tablet(0).unwrap();
    let error = world.clone_tablet(0).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::AlreadyAttached { .. })),
        "{error}"
    );
    let error = world.hub(0).init(&world.origin).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(
            e,
            CycleError::AlreadyInitialised { .. }
        )),
        "{error}"
    );
    let error = world
        .hub(0)
        .clone_tablet(
            world.tablet(0),
            "https://user:secret@example.invalid/x.git",
            None,
            true,
        )
        .unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::CredentialsInUrl)),
        "{error}"
    );
    // A cloud-paired tablet is refused before anything changes.
    let paired = World::new(1);
    let dir = paired.device(0).dir.clone();
    let tablet = libremarkable_utils::hub::device::FakeTablet::builder(&dir)
        .cloud_paired(true)
        .build()
        .unwrap();
    let error = paired
        .hub(0)
        .clone_tablet(&tablet, &paired.origin, None, false)
        .unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::CloudPaired)),
        "{error}"
    );
    assert!(tablet.is_running());
    assert!(!is_locked(&error));
}
