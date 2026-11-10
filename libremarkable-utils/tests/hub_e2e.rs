//! End-to-end tests of the `rmu hub` sync cycle (`hub::cycle`): two or
//! three fake tablets, a bare origin and one local cache per tablet in a
//! scratch directory. No network, no real tablet, hermetic git (see
//! `hub_e2e/world.rs`). Every scenario ends with the same oracles
//! (`World::check`): the tablets converge on origin, no ink is lost,
//! xochitl runs, no lock or journal is left.
#![cfg(unix)]

#[path = "hub_e2e/world.rs"]
mod world;

use libremarkable_utils::hub::cycle::{DetachOptions, MainUpdate, Warning};
use libremarkable_utils::hub::merge::NoteKind;
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
    // The refusal names the books, which the tablet no longer has.
    let named = |names: &std::collections::BTreeMap<_, String>, docs: &[_]| {
        docs.iter().enumerate().all(|(n, book)| {
            names.get(book).map(String::as_str) == Some(format!("Book {n}").as_str())
        })
    };
    assert!(
        is_cycle(&refused, |e| matches!(
            e,
            CycleError::DeletionsNeedConsent { names, .. } if names.len() == 55
        )),
        "{refused:?}"
    );
    if let libremarkable_utils::error::Error::HubCycle(CycleError::DeletionsNeedConsent {
        names,
        ..
    }) = &refused
    {
        assert!(named(names, &books[..55]), "{names:?}");
    }
    let status = world.status(1).unwrap();
    assert!(named(&status.names, &books[..55]), "{:?}", status.names);
    assert_eq!(world.origin_docs().len(), before.len());
    world.check_idle(1);
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
    assert!(named(&report.names, &books[..55]), "{:?}", report.names);
    // ...and A applies them without being asked again, and names them
    // after deleting them.
    let report = world.sync(0).unwrap();
    assert_eq!(report.applied.len(), 55);
    assert_eq!(report.deletions.exempt.len(), 55);
    assert!(named(&report.names, &books[..55]), "{:?}", report.names);
    world.converge();
    assert_eq!(world.device(0).docs().len(), 6);
    // Below the threshold no consent is needed.
    books[55..].iter().for_each(|book| world.delete(0, book));
    world.sync(0).unwrap();
    let dry = world.dry_run(1).unwrap();
    assert_eq!(dry.deletes.len(), 5, "{dry:?}");
    assert!(
        dry.deletes
            .iter()
            .all(|doc| dry.status.names.contains_key(doc)),
        "{dry:?}"
    );
    let report = world.sync(1).unwrap();
    assert_eq!(report.deletions.from_tablet.len(), 5, "{report:?}");
    let names: Vec<&str> = report.names.values().map(String::as_str).collect();
    assert_eq!(
        names,
        ["Book 55", "Book 56", "Book 57", "Book 58", "Book 59"]
    );
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
    // Every document the empty tablet would delete is named, from
    // origin's history.
    let expected: std::collections::BTreeMap<_, String> = [
        (&lib.folder, "Projects"),
        (&lib.notes, "Notes"),
        (&lib.sketch, "Sketch"),
        (&lib.paper, "Paper"),
        (&lib.archive, "Archive"),
        (&lib.diary, "Diary"),
    ]
    .into_iter()
    .map(|(doc, name)| (doc.clone(), name.to_string()))
    .collect();
    assert!(
        matches!(
            &refused,
            libremarkable_utils::error::Error::HubCycle(CycleError::DeletionsNeedConsent {
                names,
                ..
            }) if *names == expected
        ),
        "{refused:?}"
    );
    let status = world.status(1).unwrap();
    assert_eq!(status.names, expected);
    assert!(
        status
            .changes
            .values()
            .all(|change| *change == libremarkable_utils::hub::cycle::DocChange::Deleted)
    );
    assert_eq!(world.origin_docs().len(), 6);
    world.check_idle(1);
}

/// A deletes the notes while B inks them; A's deletion reaches origin
/// first, B's sync keeps the inked notes over it, and A's next sync, a
/// pull, brings them back with a note saying why.
#[test]
fn a_document_this_tablet_deleted_comes_back_with_a_note() {
    let (world, lib) = two_tablets();
    world.delete(0, &lib.notes);
    let page = world.pages(1, &lib.notes)[0].clone();
    world.ink(1, &lib.notes, &page);
    world.sync(0).unwrap();
    let kept = world.sync(1).unwrap();
    assert!(
        kept.notes
            .iter()
            .any(|note| note.doc == lib.notes
                && matches!(note.kind, NoteKind::KeptOverDeletion { .. })),
        "{kept:?}"
    );
    // B's own new notebook reaches A as a plain addition: no note.
    let fresh = world.create(1, Kind::Notebook { pages: 1 }, "Fresh", "");
    world.sync(1).unwrap();
    let back = world.sync(0).unwrap();
    assert_eq!(back.update, MainUpdate::Pulled, "{back:?}");
    let returned: Vec<_> = back
        .notes
        .iter()
        .filter(|note| note.kind == NoteKind::ReturnedAfterDeletion)
        .map(|note| &note.doc)
        .collect();
    assert_eq!(returned, vec![&lib.notes], "{back:?}");
    assert!(!back.notes.iter().any(|note| note.doc == fresh));
    assert!(world.device(0).metadata(&lib.notes).is_some());
    world.converge();
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
    edits_during_the_sync(false);
    edits_during_the_sync(true);
}

/// B renames the notes; A will apply that. While A's sync has the UI up
/// (between staging and stopping xochitl again to apply), the user inks
/// them on A: that document is skipped, keeps its snapshot version as its
/// base, and the next sync merges the ink with B's rename. With a change
/// of A's own (`own_change`), the base is the snapshot commit (`main`
/// contains it); without, nothing is pushed and it is the notes' previous
/// base, which holds the same version.
fn edits_during_the_sync(own_change: bool) {
    let (base, lib) = two_tablets();
    base.rename(1, &lib.notes, "Notes (renamed on B)");
    base.sync(1).unwrap();
    if own_change {
        base.rename(0, &lib.sketch, "Sketch (A)");
    }
    let page = base.pages(0, &lib.notes)[0].clone();
    // The op after the (last) Stage: the arm before the step-13 stop.
    let stage = op_index(&base, 0, OpKind::Stage, 2, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    let previous = world
        .device(0)
        .state()
        .unwrap()
        .base_for(&lib.notes)
        .cloned();
    world.inks.lock().unwrap().expect("window-ink", &lib.notes);
    let page_path = format!("{}/{page}.rm", lib.notes);
    world.tablet(0).inject(Fault::EditBefore(
        stage + 1,
        Box::new(move |dir: &Path| {
            let path = dir.join(&page_path);
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(b"stroke window-ink\n");
            std::fs::write(&path, bytes).unwrap();
        }),
    ));
    let report = world.sync(0).unwrap();
    assert_eq!(report.skipped, vec![lib.notes.clone()], "{report:?}");
    let expected = if own_change {
        assert!(matches!(report.update, MainUpdate::Merge(_)), "{report:?}");
        Some(report.snapshot.clone())
    } else {
        assert_eq!(report.update, MainUpdate::Pulled, "{report:?}");
        previous
    };
    let device = world.device(0);
    let state = device.state().unwrap();
    assert_eq!(state.base_overrides.get(&lib.notes), Some(&expected));
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
    world.check_idle(1);
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
    world.check_idle(0);
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
    world.check_idle(0);
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
    assert!(!Path::new(&tablet.paths().lock_dir()).exists());
    assert!(!is_locked(&error));
    // Refused before reading: nothing was downloaded.
    assert!(
        !tablet
            .ops()
            .iter()
            .any(|op| matches!(op, OpKind::StatAll | OpKind::Download)),
        "{:?}",
        tablet.ops()
    );
}

use libremarkable_utils::hub::device::OpKind;
use std::sync::Arc;

/// The index (1-based, as faults count) of the `nth` operation of `kind`
/// in a run of `run` on a copy of the world.
fn op_index(world: &World, tablet: usize, kind: OpKind, nth: usize, run: impl Fn(&World)) -> usize {
    let probe = world.duplicate();
    let before = probe.tablet(tablet).ops().len();
    run(&probe);
    let ops = probe.tablet(tablet).ops();
    ops[before..]
        .iter()
        .enumerate()
        .filter(|(_, op)| **op == kind)
        .nth(nth - 1)
        .map(|(index, _)| index + 1)
        .unwrap_or_else(|| panic!("no {kind:?} #{nth} in {:?}", &ops[before..]))
}

#[test]
fn a_concurrent_pusher_makes_the_push_retry() {
    let (base, lib) = two_tablets();
    let page = base.pages(0, &lib.notes)[1].clone();
    base.ink(0, &lib.notes, &page);
    base.rename(1, &lib.archive, "Attic");
    // Right before A writes its journal, B syncs: A's push is rejected
    // and A goes back to fetch and merge.
    let journal = op_index(&base, 0, OpKind::WriteState, 2, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    world.tablet(0).inject(Fault::EditBefore(
        journal,
        other_computer(&world, 1, None, |hub, tablet, _| {
            let report = hub.sync(tablet).unwrap();
            assert_ne!(report.update, MainUpdate::UpToDate);
        }),
    ));
    let report = world.sync(0).unwrap();
    assert_eq!(report.push_attempts, 2, "{report:?}");
    world.converge();
    assert_eq!(
        world.device(0).metadata(&lib.archive).unwrap()["visibleName"],
        "Attic"
    );
}

/// An edit (for `Fault::EditBefore`) that runs `run` on tablet `i` from
/// its computer (or from the one whose cache is `cache`): another rmu
/// acting while this one is mid-run.
fn other_computer(
    world: &World,
    i: usize,
    cache: Option<std::path::PathBuf>,
    run: impl FnOnce(
        &libremarkable_utils::hub::cycle::Hub<'static>,
        &dyn libremarkable_utils::hub::device::Tablet,
        &str,
    ) + Send
    + 'static,
) -> libremarkable_utils::hub::device::Edit {
    let (git, origin, options) = (
        world.git.clone(),
        world.origin.clone(),
        world.options.clone(),
    );
    let (tablet, cache) = (
        Arc::clone(&world.device(i).tablet),
        cache.unwrap_or_else(|| world.device(i).cache.clone()),
    );
    Box::new(move |_| {
        static PROGRESS: libremarkable_utils::progress::NoProgress =
            libremarkable_utils::progress::NoProgress;
        let hub = libremarkable_utils::hub::cycle::Hub::new(
            git,
            cache,
            libremarkable_utils::hub::device::LockRequest::for_this_process("other-host"),
            &PROGRESS,
        )
        .with_options(HubOptions {
            origin: Some(origin.clone()),
            ..options
        });
        run(&hub, &*tablet, &origin);
    })
}

/// Make origin take the next push to `main` and then drop the connection
/// before acknowledging it (receive-pack is killed once the ref update is
/// committed).
fn lose_next_main_push_ack(world: &World) -> std::path::PathBuf {
    let trigger = world.root.join("lose-ack");
    std::fs::write(&trigger, b"").unwrap();
    let hooks = Path::new(&world.origin).join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    test_support::write_script(
        &hooks.join("reference-transaction"),
        &format!(
            "input=$(cat)\n\
             case \"$input\" in *refs/heads/main*) ;; *) exit 0 ;; esac\n\
             if [ \"$1\" = committed ] && [ -e '{trigger}' ]; then rm -f '{trigger}'; kill -9 $PPID; fi\n\
             exit 0\n",
            trigger = trigger.display()
        ),
    );
    trigger
}

#[test]
fn a_lost_push_acknowledgement_is_resolved_by_looking_at_origin() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.diary)[0].clone();
    world.ink(0, &lib.diary, &page);
    world.rename(1, &lib.sketch, "Sketch (B)");
    world.sync(1).unwrap();
    let trigger = lose_next_main_push_ack(&world);
    // The push lands, its acknowledgement does not: rmu fetches, finds
    // its merge on origin and carries on to apply B's rename.
    let report = world.sync(0).unwrap();
    assert!(!trigger.exists(), "the hook did not fire");
    assert!(matches!(report.update, MainUpdate::Merge(_)));
    assert_eq!(
        world.device(0).metadata(&lib.sketch).unwrap()["visibleName"],
        "Sketch (B)"
    );
    world.converge();
}

#[test]
fn reattach_after_lost_state() {
    let (world, lib) = two_tablets();
    world.rename(1, &lib.paper, "Paper v2");
    world.sync(1).unwrap();
    // A loses its hub state (documents untouched) and reattaches: every
    // document matches a version in origin's history, so nothing forks
    // and B's rename arrives.
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    let report = world.reattach(0).unwrap();
    assert!(report.forks.is_empty(), "{report:?}");
    assert_eq!(
        world.device(0).metadata(&lib.paper).unwrap()["visibleName"],
        "Paper v2"
    );
    world.converge();
    assert!(world.conflicts().is_empty());
    // A document edited since the lost state has no known base: it reads
    // as new on both sides and forks rather than guessing (no ink lost).
    let page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &page);
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    world.reattach(0).unwrap();
    world.converge();
    assert_eq!(world.conflicts().len(), 1);
}

#[test]
fn a_shared_cloud_library_attaches_without_duplicates() {
    // Both tablets synced the same library through a reMarkable cloud
    // account before: same UUIDs, same content, their own view state.
    let world = World::new(2);
    let lib = Library::create(&world, 0);
    let a = world.device(0).docs();
    a.iter().for_each(|(doc, files)| {
        files.iter().for_each(|(file, bytes)| {
            world
                .device(1)
                .tablet
                .write_file(&file.xochitl_path(doc), bytes)
                .unwrap();
        });
    });
    world.read(1, &lib.notes);
    world.read(1, &lib.paper);
    // Except that B has inked the diary since.
    let page = world.pages(1, &lib.diary)[0].clone();
    world.ink(1, &lib.diary, &page);
    world.clone_tablet(0).unwrap();
    let report = world.clone_tablet(1).unwrap();
    // Matching documents are taken once; the diary is new on both sides
    // with different ink, so it forks (B's version as the copy).
    assert_eq!(report.forks.len(), 1, "{report:?}");
    assert_eq!(report.forks[0].original, lib.diary);
    world.converge();
    assert_eq!(world.conflicts().len(), 1);
}

#[test]
fn backup_writes_only_the_device_ref() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.sketch)[0].clone();
    world.ink(0, &lib.sketch, &page);
    let main_before = world.origin_docs();
    let state_before = world.device(0).state().unwrap();
    let docs_before = world.device(0).docs();
    let report = world.backup(0).unwrap();
    assert!(report.pushed);
    let state = world.device(0).state().unwrap();
    assert_eq!(state.device_ref, Some(report.snapshot.clone()));
    assert_eq!(state.base, state_before.base);
    assert_eq!(world.origin_docs(), main_before);
    assert_eq!(world.device(0).docs(), docs_before);
    // The device ref on origin holds the ink.
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    let refs = repo.ls_remote(&world.origin).unwrap();
    assert!(
        refs.iter()
            .any(|r| r.name.starts_with("refs/rmu/devices/") && r.oid == report.snapshot)
    );
    // Again with nothing new: the same snapshot, nothing pushed.
    let again = world.backup(0).unwrap();
    assert!(!again.pushed);
    assert_eq!(again.snapshot, report.snapshot);
    world.converge();
}

#[test]
fn status_and_dry_run_change_nothing() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &page);
    world.delete(1, &lib.sketch);
    world.sync(1).unwrap();
    let origin = world.origin_docs();
    let docs = world.device(0).docs();
    let ops = world.tablet(0).ops().len();
    let status = world.status(0).unwrap();
    assert_eq!(
        status.changes.get(&lib.notes),
        Some(&libremarkable_utils::hub::cycle::DocChange::Modified)
    );
    assert_eq!(status.behind, 1);
    assert!(!status.guard.blocked());
    let dry = world.dry_run(0).unwrap();
    assert_eq!(dry.deletes, vec![lib.sketch.clone()]);
    assert!(dry.would_push);
    assert!(
        world.tablet(0).ops()[ops..]
            .iter()
            .all(|op| !op.is_mutating())
    );
    assert_eq!(world.origin_docs(), origin);
    assert_eq!(world.device(0).docs(), docs);
    world.converge();
    let dry = world.dry_run(0).unwrap();
    assert!(!dry.would_push && dry.writes.is_empty() && dry.deletes.is_empty());
}

// ---- crash at every step ---------------------------------------------------------

/// The crash scenario: B's changes are on origin (a rename, a new
/// notebook, a deletion, ink on a page A also inks, ink in a notebook A
/// holds quarantined, the deletion of two others A holds quarantined), A
/// has its own (that ink, an inserted page, a rename, a new folder with a
/// move, a trashed document, a corrupt `.metadata`, two lost `.metadata`,
/// one after new ink, one after a rename). A's sync therefore reads, merges with a fork, pushes,
/// stages, applies writes and deletions, leaves two quarantined
/// documents alone, and records. [`settle`] repairs them at the end.
fn crash_template() -> World {
    let (world, lib) = two_tablets();
    let ledger = world.create(0, Kind::Notebook { pages: 1 }, "Ledger", "");
    let scratch = world.create(0, Kind::Notebook { pages: 1 }, "Scratch", "");
    let spare = world.create(0, Kind::Notebook { pages: 1 }, "Spare", "");
    world.sync(0).unwrap();
    world.sync(1).unwrap();
    let page = world.pages(0, &lib.notes)[0].clone();
    world.rename(1, &lib.paper, "Paper (B)");
    world.create(
        1,
        Kind::Notebook { pages: 1 },
        "From B",
        lib.folder.as_str(),
    );
    world.delete(1, &lib.diary);
    world.ink(1, &lib.notes, &page);
    world.insert_page(1, &ledger);
    world.delete(1, &scratch);
    world.delete(1, &spare);
    world.sync(1).unwrap();
    world.ink(0, &lib.notes, &page);
    world.insert_page(0, &lib.sketch);
    world.rename(0, &lib.archive, "Attic");
    let inbox = world.create(0, Kind::Folder, "Inbox", "");
    world.move_to(0, &lib.sketch, inbox.as_str());
    world.trash(0, &lib.paper);
    world.corrupt(0, &ledger);
    let scratch_page = world.pages(0, &scratch)[0].clone();
    world.ink(0, &scratch, &scratch_page);
    world.lose_metadata(0, &scratch);
    // Renamed, then its `.metadata` lost: every file left is exactly the
    // snapshot's, so it looks like a deletion interrupted after
    // `.metadata`. No run planned it; it must never be deleted.
    world.rename(0, &spare, "Spare (A)");
    world.lose_metadata(0, &spare);
    world
}

/// Converge, repair what A holds quarantined, converge again; the
/// conflict copies at the end. With the repairs, the ledger takes B's
/// page (A never changed it) and the scratch notebook comes back (A's
/// ink beats B's deletion); the ink oracle then covers both on A too.
fn settle(world: &World) -> Vec<(libremarkable_utils::hub::model::DocId, String)> {
    world.converge();
    world.repair(0);
    world.converge();
    // The renamed spare notebook beats B's deletion with its pages: the
    // ink oracle cannot tell (B deleted that ink deliberately), so look.
    let origin = world.origin_docs();
    let spare = world
        .device(0)
        .names()
        .into_iter()
        .find(|(_, name)| name == "Spare (A)")
        .map(|(doc, _)| doc)
        .expect("the spare notebook is on the tablet");
    assert!(
        origin[&spare]
            .keys()
            .any(|file| matches!(file, libremarkable_utils::hub::model::DocFile::Page(_))),
        "the spare notebook lost its pages"
    );
    world.conflicts()
}

/// What a clean run of the scenario ends with: A's op log, and the
/// conflict copies after convergence.
struct Reference {
    ops: Vec<OpKind>,
    mutations: usize,
    conflicts: Vec<(libremarkable_utils::hub::model::DocId, String)>,
}

fn reference(template: &World) -> Reference {
    let world = template.duplicate();
    let report = world.sync(0).unwrap();
    assert_eq!(report.forks.len(), 1, "{report:?}");
    assert!(!report.applied.is_empty());
    let ops = world.tablet(0).ops();
    let mutations = ops.iter().filter(|op| op.is_mutating()).count();
    // Two starts per cycle (after the snapshot, after the apply), each
    // after a stop with a fresh watchdog.
    let count = |kind: OpKind| ops.iter().filter(|op| **op == kind).count();
    assert_eq!(count(OpKind::StartXochitl), 2, "{ops:?}");
    assert_eq!(count(OpKind::StopXochitl), 2, "{ops:?}");
    assert_eq!(report.quarantined.len(), 3, "{report:?}");
    let again = world.sync(0).unwrap();
    assert!(
        again.update == MainUpdate::UpToDate && again.applied.is_empty(),
        "{again:?}"
    );
    Reference {
        ops,
        mutations,
        conflicts: settle(&world),
    }
}

/// After a failed (or interrupted) run: xochitl must be up or come back
/// with the watchdog; the next run converges with no ink lost and
/// exactly the reference's conflict copies.
fn recover(world: &World, reference: &Reference, what: &str) {
    let tablet = world.tablet(0);
    tablet.reconnect();
    let armed = tablet.watchdog_armed().is_some();
    assert!(
        tablet.is_running() || armed,
        "{what}: xochitl is down and no watchdog would bring it back"
    );
    // Time passes: an armed watchdog fires.
    if armed {
        tablet.fire_watchdog();
    }
    assert!(tablet.is_running(), "{what}: xochitl did not come back");
    let next = world.sync(0);
    world::no_contract_violation(&next);
    match next {
        Ok(_) => {}
        // A crash between taking the lock and arming the first watchdog,
        // or between disarming the last and releasing the lock, leaves a
        // lock nothing makes stale: the user is told to --break-lock.
        Err(error) if is_locked(&error) => {
            assert!(!armed, "{what}: a lock with a fired watchdog must be stale");
            world
                .sync_with(
                    0,
                    HubOptions {
                        break_lock: true,
                        ..world.options.clone()
                    },
                )
                .unwrap_or_else(|e| panic!("{what}: the run after --break-lock failed: {e}"));
        }
        Err(error) => panic!("{what}: the next run failed: {error}"),
    }
    // The recovering run left the tablet in step with origin (it
    // snapshotted what reconciliation wrote, not the read before it).
    let quiet = world.sync(0).unwrap();
    assert!(
        quiet.update == MainUpdate::UpToDate && quiet.applied.is_empty(),
        "{what}: the run after the recovery was not quiet: {quiet:?}"
    );
    assert_eq!(
        settle(world),
        reference.conflicts,
        "{what}: conflict copies"
    );
}

/// Run A's sync with `fault` injected, then recover.
fn crash_with(template: &World, reference: &Reference, fault: Fault, what: &str) {
    let world = template.duplicate();
    world.tablet(0).inject(fault);
    let result = world.sync(0);
    world::no_contract_violation(&result);
    recover(&world, reference, what);
}

#[test]
fn crash_before_every_operation() {
    let template = crash_template();
    let reference = reference(&template);
    (1..=reference.ops.len()).for_each(|k| {
        crash_with(
            &template,
            &reference,
            Fault::DisconnectAtOp(k),
            &format!("disconnect at op {k} ({:?})", reference.ops[k - 1]),
        );
    });
}

#[test]
fn crash_after_every_mutation_took_effect() {
    let template = crash_template();
    let reference = reference(&template);
    (1..=reference.mutations).for_each(|m| {
        crash_with(
            &template,
            &reference,
            Fault::DisconnectAfter(m),
            &format!("disconnect after mutation {m}"),
        );
    });
}

#[test]
fn lost_acknowledgement_of_every_mutation() {
    let template = crash_template();
    let reference = reference(&template);
    (1..=reference.mutations).for_each(|m| {
        crash_with(
            &template,
            &reference,
            Fault::LoseAck(m),
            &format!("lost ack of mutation {m}"),
        );
    });
}

#[test]
fn failure_of_every_operation() {
    let template = crash_template();
    let reference = reference(&template);
    (1..=reference.ops.len()).for_each(|k| {
        crash_with(
            &template,
            &reference,
            Fault::FailOp(k),
            &format!("failure of op {k} ({:?})", reference.ops[k - 1]),
        );
    });
}

#[test]
fn apply_killed_after_every_move() {
    let template = crash_template();
    let reference = reference(&template);
    // Until the apply survives the kill point (it has fewer moves).
    let completed = (0..200).find(|&k| {
        let world = template.duplicate();
        world.tablet(0).inject(Fault::KillApplyAfter(k));
        let result = world.sync(0);
        world::no_contract_violation(&result);
        let survived = result.is_ok();
        recover(
            &world,
            &reference,
            &format!("apply killed after {k} move(s)"),
        );
        survived
    });
    assert!(completed.is_some_and(|k| k > 3), "{completed:?}");
}

#[test]
fn watchdog_fires_after_every_move() {
    let template = crash_template();
    let reference = reference(&template);
    let completed = (1..200).find(|&k| {
        let world = template.duplicate();
        world.tablet(0).inject(Fault::FireWatchdogAfterMoves(k));
        let result = world.sync(0);
        world::no_contract_violation(&result);
        let survived = result.is_ok();
        recover(
            &world,
            &reference,
            &format!("watchdog fired after {k} move(s)"),
        );
        survived
    });
    assert!(completed.is_some_and(|k| k > 3), "{completed:?}");
}

/// After an interrupted apply, the user writes in every notebook the
/// apply already brought in. Those edits are on top of the merge, not a
/// conflict with it: the next run must not fork them.
#[test]
fn edits_after_an_interrupted_apply_are_not_conflicts() {
    let template = crash_template();
    let reference = reference(&template);
    let record = op_index(&template, 0, OpKind::WriteState, 3, |w| {
        w.sync(0).unwrap();
    });
    let faults = std::iter::once((
        Fault::DisconnectAtOp(record),
        "crash before the record".to_string(),
    ))
    .chain((0..12).map(|k| {
        (
            Fault::KillApplyAfter(k),
            format!("apply killed after {k} move(s)"),
        )
    }));
    faults.for_each(|(fault, what)| {
        let world = template.duplicate();
        world.tablet(0).inject(fault);
        let _ = world.sync(0);
        let tablet = world.tablet(0);
        tablet.reconnect();
        tablet.fire_watchdog();
        let device = world.device(0);
        device
            .names()
            .into_iter()
            .filter(|(_, name)| name == "From B")
            .for_each(|(doc, _)| {
                let page = world.pages(0, &doc)[0].clone();
                world.ink(0, &doc, &page);
            });
        world.sync(0).unwrap_or_else(|e| panic!("{what}: {e}"));
        world.converge();
        assert_eq!(world.conflicts(), reference.conflicts, "{what}");
    });
}

/// The first kill point whose recovery finishes a half-applied
/// document, and the documents it finishes.
fn kill_leaving_a_half_applied_document(
    template: &World,
) -> (usize, Vec<libremarkable_utils::hub::model::DocId>) {
    (0..30)
        .find_map(|k| {
            let crashed = crashed_by(template, Fault::KillApplyAfter(k));
            let report = crashed.sync(0).unwrap();
            let finished = report.reconciled?.finished;
            (!finished.is_empty()).then_some((k, finished))
        })
        .expect("a kill point that leaves a half-applied document")
}

/// Crashes during reconciliation itself: after a first crash that left a
/// journal, the recovering run is cut at every one of its operations.
fn crash_during_reconciliation(template: &World, reference: &Reference, first: Fault, what: &str) {
    let crashed = crashed_by(template, first);
    // The recovering run, clean, on a copy: its operations.
    let probe = crashed.duplicate();
    let report = probe.sync(0).unwrap();
    assert!(report.reconciled.is_some(), "{what}: {report:?}");
    let ops = probe.tablet(0).ops();
    (1..=ops.len()).for_each(|k| {
        let world = crashed.duplicate();
        world.tablet(0).inject(Fault::DisconnectAtOp(k));
        let result = world.sync(0);
        world::no_contract_violation(&result);
        recover(
            &world,
            reference,
            &format!("{what}, then op {k} ({:?})", ops[k - 1]),
        );
    });
}

#[test]
fn crash_during_reconciliation_after_a_killed_apply() {
    let template = crash_template();
    let reference = reference(&template);
    let (kill, _) = kill_leaving_a_half_applied_document(&template);
    crash_during_reconciliation(
        &template,
        &reference,
        Fault::KillApplyAfter(kill),
        &format!("apply killed after {kill} moves"),
    );
}

#[test]
fn crash_during_reconciliation_after_an_unrecorded_apply() {
    let template = crash_template();
    let reference = reference(&template);
    let record = op_index(&template, 0, OpKind::WriteState, 3, |w| {
        w.sync(0).unwrap();
    });
    crash_during_reconciliation(
        &template,
        &reference,
        Fault::DisconnectAtOp(record),
        "crash before the record",
    );
}

/// Reconciliation finished a document, then crashed before writing the
/// result; the user then writes in that document. Its own sentinel shows
/// the document finished: the edit is on top of the merge, not a
/// conflict.
#[test]
fn an_edit_after_a_crashed_reconciliation_is_not_a_conflict() {
    let template = crash_template();
    let reference = reference(&template);
    let (kill, finished) = kill_leaving_a_half_applied_document(&template);
    let crashed = crashed_by(&template, Fault::KillApplyAfter(kill));
    let written = op_index(&crashed, 0, OpKind::WriteState, 1, |w| {
        w.sync(0).unwrap();
    });
    let world = crashed.duplicate();
    world.tablet(0).inject(Fault::DisconnectAtOp(written));
    assert!(world.sync(0).is_err());
    world.tablet(0).reconnect();
    world.tablet(0).fire_watchdog();
    let inked = finished
        .iter()
        .filter_map(|doc| {
            let page = world
                .device(0)
                .content(doc)
                .and_then(|content| world::pages_of(&content).first().cloned())?;
            world.ink(0, doc, &page);
            Some(doc)
        })
        .count();
    assert!(inked > 0, "nothing to write in among {finished:?}");
    world.sync(0).unwrap();
    assert_eq!(settle(&world), reference.conflicts);
}

/// A copy of `template` after A's sync failed with `fault`, the
/// connection back and the watchdog fired.
fn crashed_by(template: &World, fault: Fault) -> World {
    let crashed = template.duplicate();
    crashed.tablet(0).inject(fault);
    assert!(crashed.sync(0).is_err());
    crashed.tablet(0).reconnect();
    crashed.tablet(0).fire_watchdog();
    crashed
}

/// A pre-read cut inside a file ends the `fast-import` it streamed into;
/// it must only shrink the pre-read, never fail the run.
#[test]
fn a_pre_read_cut_inside_a_file_only_shrinks_it() {
    let template = crash_template();
    let reference = reference(&template);
    (0..3).for_each(|k| {
        let world = template.duplicate();
        // The next download is the pre-read's (a copied world's stats are
        // all new).
        world.tablet(0).inject(Fault::CutDownloadInFile(k));
        world
            .sync(0)
            .unwrap_or_else(|e| panic!("pre-read cut inside file {}: {e}", k + 1));
        assert_eq!(settle(&world), reference.conflicts);
    });
}

/// Another computer syncs the same tablet between this run's first fetch
/// and its lock: the newer base and device ref it records must not read
/// as a rewound origin or a foreign device ref.
#[test]
fn a_run_that_held_the_lock_first_is_seen_by_fetching_again() {
    let (base, lib) = two_tablets();
    let page = base.pages(0, &lib.notes)[0].clone();
    base.ink(0, &lib.notes, &page);
    base.rename(1, &lib.paper, "Paper (B)");
    base.sync(1).unwrap();
    let lock = op_index(&base, 0, OpKind::AcquireLock, 1, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    let elsewhere = world.root.join("computer-x").join("cache");
    world.tablet(0).inject(Fault::EditBefore(
        lock,
        other_computer(&world, 0, Some(elsewhere), |hub, tablet, _| {
            assert!(matches!(
                hub.sync(tablet).unwrap().update,
                MainUpdate::Merge(_)
            ));
        }),
    ));
    let report = world.sync(0).unwrap();
    assert_eq!(report.update, MainUpdate::UpToDate, "{report:?}");
    world.converge();
}

/// A document that lost its `.metadata` after new ink, on a tablet that
/// reattaches (so it is in no base): its files are not rmu's leftovers
/// and are never overwritten by origin's version.
#[test]
fn reattach_never_overwrites_a_document_that_lost_its_metadata() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.sketch)[0].clone();
    world.ink(0, &lib.sketch, &page);
    let marker = world
        .inks
        .lock()
        .unwrap()
        .expected
        .keys()
        .last()
        .cloned()
        .unwrap();
    world.lose_metadata(0, &lib.sketch);
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    let report = world.reattach(0).unwrap();
    assert!(report.orphans.contains(&lib.sketch), "{report:?}");
    let rm = format!("{}/{page}.rm", lib.sketch);
    let kept = String::from_utf8(world.device(0).tablet.read_file(&rm).unwrap()).unwrap();
    assert!(kept.contains(&marker), "the tablet's ink was overwritten");
    // Repaired, it is new on both sides with different ink: a conflict
    // copy, ink kept.
    world.repair(0);
    world.converge();
    assert_eq!(world.conflicts().len(), 1);
}

#[test]
fn download_cut_after_every_file() {
    let template = crash_template();
    // A backup first records the manifest, so the next pre-read finds
    // nothing to download; files A's user writes right after it (before
    // the lock) are what the stopped read downloads, and what the cut
    // hits.
    let prepared = || {
        let world = template.duplicate();
        world.backup(0).unwrap();
        world
    };
    let probe = prepared();
    let before = probe.tablet(0).ops().len();
    probe.sync(0).unwrap();
    let ops = probe.tablet(0).ops();
    let lock = ops[before..]
        .iter()
        .position(|op| *op == OpKind::AcquireLock)
        .unwrap()
        + 1;
    assert!(!ops[before..before + lock].contains(&OpKind::Download));
    let edit = |world: &World| -> usize {
        let device = world.device(0);
        let pages: Vec<(String, libremarkable_utils::hub::model::DocId)> = device
            .names()
            .into_keys()
            .filter_map(|doc| {
                let page = world::pages_of(&device.content(&doc)?).first()?.clone();
                device.metadata(&doc)?;
                Some((format!("{doc}/{page}.rm"), doc))
            })
            .take(4)
            .collect();
        pages.iter().enumerate().for_each(|(n, (_, doc))| {
            world
                .inks
                .lock()
                .unwrap()
                .expect(&format!("cut-ink-{n}"), doc);
        });
        let edited = pages.len();
        world.tablet(0).inject(Fault::EditBefore(
            lock,
            Box::new(move |dir: &Path| {
                pages.iter().enumerate().for_each(|(n, (path, _))| {
                    let file = dir.join(path);
                    let mut bytes = std::fs::read(&file).unwrap();
                    bytes.extend_from_slice(format!("stroke cut-ink-{n}\n").as_bytes());
                    std::fs::write(&file, bytes).unwrap();
                });
            }),
        ));
        edited
    };
    // The same edits without a cut: the conflict copies to expect.
    let clean = prepared();
    let edited = edit(&clean);
    assert!(edited >= 2, "{edited}");
    clean.sync(0).unwrap();
    let reference = Reference {
        ops: Vec::new(),
        mutations: 0,
        conflicts: settle(&clean),
    };
    // The stopped read downloads exactly the edited pages.
    (0..edited).for_each(|k| {
        let world = prepared();
        edit(&world);
        world.tablet(0).inject(Fault::FailDownloadAfter(k));
        let result = world.sync(0);
        world::no_contract_violation(&result);
        assert!(
            result.is_err(),
            "cut after {k} file(s): the stopped read was not cut"
        );
        recover(
            &world,
            &reference,
            &format!("download cut after {k} file(s)"),
        );
    });
}

// ---- interleavings ---------------------------------------------------------------

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[self.below(items.len())])
    }
}

/// Three tablets edit and sync in a seeded, interleaved order (ink,
/// inserted pages, renames, moves, trash, emptying the trash, new
/// folders and notebooks, deletions, reading); at the end they converge
/// and no ink is lost. Forks may happen (phase 1 forks a document whose
/// pages changed on two tablets); every one keeps both versions.
fn interleaving(seed: u64) {
    let world = World::new(3);
    Library::create(&world, 0);
    (0..3).for_each(|i| {
        world.clone_tablet(i).unwrap();
    });
    world.converge();
    let mut rng = Rng(seed);
    (0..60).for_each(|step| {
        let i = rng.below(3);
        let device = world.device(i);
        let names = device.names();
        let docs: Vec<_> = names.keys().cloned().collect();
        let kind = |doc: &libremarkable_utils::hub::model::DocId| {
            device
                .metadata(doc)
                .map(|m| m["type"].as_str().unwrap_or("").to_string())
        };
        let notebooks: Vec<_> = docs
            .iter()
            .filter(|doc| {
                kind(doc).as_deref() == Some("DocumentType")
                    && device
                        .content(doc)
                        .is_some_and(|c| c["fileType"] == "notebook")
            })
            .cloned()
            .collect();
        let folders: Vec<_> = docs
            .iter()
            .filter(|doc| kind(doc).as_deref() == Some("CollectionType"))
            .cloned()
            .collect();
        match rng.below(13) {
            0 | 1 => {
                if let Some(doc) = rng.pick(&notebooks) {
                    let pages = world.pages(i, doc);
                    if let Some(page) = rng.pick(&pages) {
                        world.ink(i, doc, page);
                    }
                }
            }
            2 => {
                if let Some(doc) = rng.pick(&notebooks) {
                    world.insert_page(i, doc);
                }
            }
            3 => {
                if let Some(doc) = rng.pick(&docs) {
                    world.rename(i, doc, &format!("Renamed {step}"));
                }
            }
            4 => {
                // Documents only: the UI never moves a folder into itself.
                let target = rng
                    .pick(&folders)
                    .map(|f| f.to_string())
                    .unwrap_or_default();
                if let Some(doc) = rng.pick(&notebooks) {
                    world.move_to(i, doc, &target);
                }
            }
            5 => {
                if let Some(doc) = rng.pick(&docs) {
                    world.trash(i, doc);
                }
            }
            6 => {
                world.empty_trash(i);
            }
            7 => {
                let parent = rng
                    .pick(&folders)
                    .map(|f| f.to_string())
                    .unwrap_or_default();
                world.create(
                    i,
                    Kind::Notebook { pages: 1 },
                    &format!("New {step}"),
                    &parent,
                );
            }
            8 => {
                world.create(i, Kind::Folder, &format!("Folder {step}"), "");
            }
            9 => {
                if let Some(doc) = rng.pick(&notebooks) {
                    world.delete(i, doc);
                }
            }
            10 => {
                if let Some(doc) = rng.pick(&docs) {
                    world.read(i, doc);
                }
            }
            11 => {
                if let Some(doc) = rng.pick(&notebooks) {
                    let pages = world.pages(i, doc);
                    if pages.len() > 1
                        && let Some(page) = rng.pick(&pages)
                    {
                        world.remove_page(i, doc, page);
                    }
                }
            }
            _ => {
                world
                    .sync(i)
                    .unwrap_or_else(|e| panic!("seed {seed} step {step}: sync {i}: {e}"));
            }
        }
    });
    world.converge();
}

#[test]
fn interleaved_edits_and_syncs_across_three_tablets() {
    [1, 7, 42, 1234, 99_991].into_iter().for_each(interleaving);
}

// ---- more guards and races -------------------------------------------------------

#[test]
fn concurrent_first_clones_into_an_empty_origin() {
    // Both tablets clone at once; B's clone lands while A's is between
    // its journal and its push. A loses the race, falls back to merging
    // with what B pushed, and nothing is duplicated.
    let base = World::new(2);
    let mine = base.create(0, Kind::Notebook { pages: 1 }, "A's notes", "");
    let theirs = base.create(1, Kind::Notebook { pages: 1 }, "B's notes", "");
    let journal = op_index(&base, 0, OpKind::WriteState, 3, |w| {
        w.clone_tablet(0).unwrap();
    });
    let world = base.duplicate();
    world.tablet(0).inject(Fault::EditBefore(
        journal,
        other_computer(&world, 1, None, |hub, tablet, origin| {
            hub.clone_tablet(tablet, origin, None, false).unwrap();
        }),
    ));
    let report = world.clone_tablet(0).unwrap();
    assert_eq!(report.push_attempts, 2, "{report:?}");
    world.converge();
    let origin = world.origin_docs();
    assert!(origin.contains_key(&mine) && origin.contains_key(&theirs));
    assert!(world.conflicts().is_empty());
}

#[test]
fn a_second_model_attaching_is_warned_about() {
    let world = World::new(2);
    world.create(0, Kind::Notebook { pages: 1 }, "Notes", "");
    world.clone_tablet(0).unwrap();
    let other = libremarkable_utils::hub::device::FakeTablet::builder(&world.device(1).dir)
        .device_info("reMarkable Other", "3.30.0")
        .build()
        .unwrap();
    let report = world
        .hub(1)
        .clone_tablet(&other, &world.origin, None, false)
        .unwrap();
    assert_eq!(report.warnings.len(), 1, "{report:?}");
    assert!(
        matches!(&report.warnings[0], Warning::MixedModel { model, .. } if model == "reMarkable Other"),
        "{report:?}"
    );
    // The same model again: no warning.
    let world = World::new(2);
    world.create(0, Kind::Notebook { pages: 1 }, "Notes", "");
    world.clone_tablet(0).unwrap();
    assert!(world.clone_tablet(1).unwrap().warnings.is_empty());
}

#[test]
fn a_device_ref_that_does_not_continue_the_tablets_stops_the_sync() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &page);
    // Someone rewrites A's device ref on origin to an unrelated history.
    let state = world.device(0).state().unwrap();
    let reference = format!("refs/rmu/devices/{}", state.device_id);
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    let main = repo.rev_parse("refs/heads/main").unwrap().unwrap();
    let first = repo
        .rev_list(&["refs/heads/main"], true)
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert_ne!(Some(&first), state.device_ref.as_ref());
    repo.update_ref(&reference, &first, None).unwrap();
    let error = world.sync(0).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(
            e,
            CycleError::DeviceRefConflict { .. }
        )),
        "{error}"
    );
    assert_eq!(repo.rev_parse("refs/heads/main").unwrap(), Some(main));
    world.check_idle(0);
}

#[test]
fn a_folder_trashed_on_origin_spares_a_notebook_written_in_during_the_sync() {
    let (base, lib) = two_tablets();
    // B trashes the folder holding the notes; while A's sync has the UI
    // up, the user writes in those notes on A.
    base.trash(1, &lib.folder);
    base.sync(1).unwrap();
    let page = base.pages(0, &lib.notes)[0].clone();
    let stage = op_index(&base, 0, OpKind::Stage, 2, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    world.inks.lock().unwrap().expect("late-ink", &lib.notes);
    let path = format!("{}/{page}.rm", lib.notes);
    world.tablet(0).inject(Fault::EditBefore(
        stage + 1,
        Box::new(move |dir: &Path| {
            let file = dir.join(&path);
            let mut bytes = std::fs::read(&file).unwrap();
            bytes.extend_from_slice(b"stroke late-ink\n");
            std::fs::write(&file, bytes).unwrap();
        }),
    ));
    let report = world.sync(0).unwrap();
    // Moving the folder into the trash would take the edited notes along.
    assert!(report.skipped.contains(&lib.folder), "{report:?}");
    assert_ne!(
        world.device(0).metadata(&lib.folder).unwrap()["parent"],
        "trash"
    );
    world.converge();
    // The edit pulls the folder holding the notes back out of the trash
    // everywhere ("an edit pulls a document, or its trashed ancestor
    // folder, out of the trash").
    let notes = world.device(1).metadata(&lib.notes).unwrap();
    assert_eq!(notes["parent"], lib.folder.as_str());
    let folder = world.device(1).metadata(&lib.folder).unwrap();
    assert_eq!(folder["parent"], "");
}

// ---- regressions found in review ------------------------------------------------

/// A run staged everything (complete sentinel) for a merge bringing B's
/// rename of the notes, then died at its apply (nothing moved). The user
/// then inks the notes on A. B's rename must still arrive: the notes'
/// base must not become the merge while their files are at the snapshot.
#[test]
fn an_edit_after_a_staged_but_unapplied_run_keeps_the_other_change() {
    let (base, lib) = two_tablets();
    base.rename(1, &lib.notes, "Notes (B)");
    base.sync(1).unwrap();
    let apply = op_index(&base, 0, OpKind::Apply, 1, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    world.tablet(0).inject(Fault::DisconnectAtOp(apply));
    assert!(world.sync(0).is_err());
    world.tablet(0).reconnect();
    world.tablet(0).fire_watchdog();
    let page = world.pages(0, &lib.notes)[0].clone();
    world.ink(0, &lib.notes, &page);
    world.sync(0).unwrap();
    world.converge();
    assert_eq!(
        world.device(0).metadata(&lib.notes).unwrap()["visibleName"],
        "Notes (B)"
    );
    assert!(world.conflicts().is_empty(), "{:?}", world.conflicts());
}

/// After a reattach that leaves an orphan origin has (files without a
/// `.metadata`, left alone), a sync with no changes pushes nothing: the
/// orphan keeps its previous base and does not count against "already
/// agrees with origin".
#[test]
fn an_orphan_origin_has_does_not_push_on_every_sync() {
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.sketch)[0].clone();
    world.ink(0, &lib.sketch, &page);
    world.lose_metadata(0, &lib.sketch);
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    world.reattach(0).unwrap();
    let first = world.sync(0).unwrap();
    let second = world.sync(0).unwrap();
    let third = world.sync(1).unwrap();
    let fourth = world.sync(0).unwrap();
    assert!(
        second.update == MainUpdate::UpToDate && fourth.update == MainUpdate::UpToDate,
        "{:?} / {:?} / {:?} / {:?}",
        first.update,
        second.update,
        third.update,
        fourth.update
    );
}

/// The shared-cloud-library clone of B (base inference, a fork), crashed
/// at every operation and recovered.
fn clone_template() -> World {
    let world = World::new(2);
    let lib = Library::create(&world, 0);
    let a = world.device(0).docs();
    a.iter().for_each(|(doc, files)| {
        files.iter().for_each(|(file, bytes)| {
            world
                .device(1)
                .tablet
                .write_file(&file.xochitl_path(doc), bytes)
                .unwrap();
        });
    });
    world.read(1, &lib.notes);
    let page = world.pages(1, &lib.diary)[0].clone();
    world.ink(1, &lib.diary, &page);
    let own = world.create(1, Kind::Notebook { pages: 1 }, "B only", "");
    let _ = own;
    world.clone_tablet(0).unwrap();
    // A moves on meanwhile.
    world.rename(0, &lib.paper, "Paper (A)");
    world.sync(0).unwrap();
    world
}

fn recover_clone(world: &World, what: &str) {
    let tablet = world.tablet(1);
    tablet.reconnect();
    let armed = tablet.watchdog_armed().is_some();
    assert!(
        tablet.is_running() || armed,
        "{what}: xochitl down, no watchdog"
    );
    if armed {
        tablet.fire_watchdog();
    }
    assert!(tablet.is_running(), "{what}: xochitl did not come back");
    let attached = world.device(1).state().is_some();
    let run = |break_lock: bool| -> libremarkable_utils::error::Result<()> {
        let options = HubOptions {
            break_lock,
            ..world.options.clone()
        };
        let hub = world.hub_with(1, options);
        if attached {
            hub.sync(world.tablet(1)).map(|_| ())
        } else {
            let name = libremarkable_utils::hub::model::DeviceName::new("Tablet B").unwrap();
            hub.clone_tablet(world.tablet(1), &world.origin, Some(name), false)
                .map(|_| ())
        }
    };
    match run(false) {
        Ok(()) => {}
        Err(error) if is_locked(&error) => {
            assert!(!armed, "{what}: a lock with a fired watchdog must be stale");
            run(true).unwrap_or_else(|e| panic!("{what}: after --break-lock: {e}"));
        }
        Err(error) => panic!("{what}: recovery failed (attached={attached}): {error}"),
    }
}

#[test]
fn crash_at_every_op_of_a_clone() {
    let template = clone_template();
    let probe = template.duplicate();
    let before = probe.tablet(1).ops().len();
    probe.clone_tablet(1).unwrap();
    let ops = probe.tablet(1).ops()[before..].to_vec();
    let mutations = ops.iter().filter(|op| op.is_mutating()).count();
    probe.converge();
    let reference = probe.conflicts();
    assert_eq!(reference.len(), 1, "{reference:?}");
    (1..=ops.len()).for_each(|k| {
        let world = template.duplicate();
        world.tablet(1).inject(Fault::DisconnectAtOp(k));
        let result = world.clone_tablet(1);
        world::no_contract_violation(&result);
        let what = format!("clone: disconnect at op {k} ({:?})", ops[k - 1]);
        recover_clone(&world, &what);
        world.converge();
        assert_eq!(world.conflicts(), reference, "{what}");
    });
    (1..=mutations).for_each(|m| {
        let world = template.duplicate();
        world.tablet(1).inject(Fault::LoseAck(m));
        let result = world.clone_tablet(1);
        world::no_contract_violation(&result);
        let what = format!("clone: lost ack of mutation {m}");
        recover_clone(&world, &what);
        world.converge();
        assert_eq!(world.conflicts(), reference, "{what}");
    });
}

/// B deletes an empty folder; while A's sync has the UI up, A's user
/// writes a new notebook into that folder. Applying the deletion would
/// orphan the notebook on the live tablet: step 14 must skip it.
#[test]
fn a_folder_deleted_on_origin_that_gained_a_child_during_the_sync_is_kept() {
    let (base, lib) = two_tablets();
    let boxf = base.create(0, Kind::Folder, "Box", "");
    base.converge();
    base.delete(1, &boxf);
    base.sync(1).unwrap();
    let stage = op_index(&base, 0, OpKind::Stage, 1, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    let child = world::uuid(9_999);
    let page = world::page_id(9_998);
    world.inks.lock().unwrap().expect("box-ink", &child);
    let template = world
        .device(0)
        .tablet
        .read_file(&format!("{}.content", lib.sketch))
        .unwrap();
    let mut content: serde_json::Value = serde_json::from_slice(&template).unwrap();
    content["cPages"]["pages"] = serde_json::json!([{
        "id": page,
        "idx": { "timestamp": "1:2", "value": "ba" },
        "template": { "timestamp": "1:3", "value": "Blank" },
    }]);
    content["pageCount"] = serde_json::json!(1);
    let metadata = world::metadata("In the box", boxf.as_str(), "DocumentType");
    let child_c = child.clone();
    world.tablet(0).inject(Fault::EditBefore(
        stage + 1,
        Box::new(move |dir: &Path| {
            std::fs::write(
                dir.join(format!("{child_c}.metadata")),
                serde_json::to_vec_pretty(&metadata).unwrap(),
            )
            .unwrap();
            std::fs::write(
                dir.join(format!("{child_c}.content")),
                serde_json::to_vec_pretty(&content).unwrap(),
            )
            .unwrap();
            std::fs::create_dir_all(dir.join(child_c.as_str())).unwrap();
            std::fs::write(
                dir.join(format!("{child_c}/{page}.rm")),
                b"reMarkable .lines file, version=6          \nstroke box-ink\n",
            )
            .unwrap();
        }),
    ));
    let report = world.sync(0).unwrap();
    assert!(report.skipped.contains(&boxf), "{report:?}");
    assert!(
        world.device(0).metadata(&boxf).is_some(),
        "the folder holding the new notebook was deleted under it"
    );
    world.converge();
}

/// The crash matrix again, but after every crash A's user writes in a
/// document only A changed (and in B's new notebook, if A has it yet)
/// before the next run: a base that is too old forks A's own change, a
/// base that is too new reverts B's.
#[test]
fn crash_before_every_operation_then_edit() {
    let template = crash_template();
    let probe = template.duplicate();
    let before = probe.tablet(0).ops().len();
    probe.sync(0).unwrap();
    let ops = probe.tablet(0).ops()[before..].to_vec();
    let edit = |world: &World| {
        let device = world.device(0);
        let sketch = device
            .names()
            .into_iter()
            .find(|(_, name)| name == "Sketch")
            .map(|(doc, _)| doc)
            .expect("the sketch");
        let page = world.pages(0, &sketch)[0].clone();
        world.ink(0, &sketch, &page);
        device
            .names()
            .into_iter()
            .filter(|(_, name)| name == "From B")
            .for_each(|(doc, _)| {
                let page = world.pages(0, &doc)[0].clone();
                world.ink(0, &doc, &page);
            });
    };
    // Clean: sync, edit, settle.
    edit(&probe);
    let reference = settle(&probe);
    (1..=ops.len()).for_each(|k| {
        let world = template.duplicate();
        world.tablet(0).inject(Fault::DisconnectAtOp(k));
        let _ = world.sync(0);
        let tablet = world.tablet(0);
        tablet.reconnect();
        let armed = tablet.watchdog_armed().is_some();
        if armed {
            tablet.fire_watchdog();
        }
        edit(&world);
        let what = format!("disconnect at op {k} ({:?}), then an edit", ops[k - 1]);
        match world.sync(0) {
            Ok(_) => {}
            Err(error) if is_locked(&error) => {
                world
                    .sync_with(
                        0,
                        HubOptions {
                            break_lock: true,
                            ..world.options.clone()
                        },
                    )
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
            }
            Err(error) => panic!("{what}: {error}"),
        }
        assert_eq!(settle(&world), reference, "{what}");
    });
}

/// A run that fails while the connection still works brings xochitl back
/// itself (the error path), without waiting for the watchdog.
#[test]
fn a_failed_run_restarts_xochitl_itself() {
    let template = crash_template();
    let clean = template.duplicate();
    let before = clean.tablet(0).ops().len();
    clean.sync(0).unwrap();
    let ops = clean.tablet(0).ops()[before..].to_vec();
    let failures: Vec<String> = (1..=ops.len())
        .filter_map(|k| {
            let world = template.duplicate();
            world.tablet(0).inject(Fault::FailOp(k));
            let result = world.sync(0);
            (result.is_err() && !world.tablet(0).is_running())
                .then(|| format!("failure of op {k} ({:?})", ops[k - 1]))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "xochitl left to the watchdog: {failures:#?}"
    );
}

/// A's apply of merge M is killed half-way; then origin is rewound to
/// just before M (an admin reset). A's journal names a merge origin no
/// longer has, but its staging sentinel proves M had landed: dropping the
/// journal as "never landed" reads the half-applied files as A's edits.
#[test]
fn a_rewind_after_the_journaled_merge_landed() {
    let template = crash_template();
    let repo = template.git.open(Path::new(&template.origin)).unwrap();
    let before = repo.rev_parse("refs/heads/main").unwrap().unwrap();
    let (kill, finished) = kill_leaving_a_half_applied_document(&template);
    let world = crashed_by(&template, Fault::KillApplyAfter(kill));
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    let after = repo.rev_parse("refs/heads/main").unwrap().unwrap();
    assert_ne!(before, after);
    repo.update_ref("refs/heads/main", &before, None).unwrap();
    // Without the staging evidence: Ok, `reconciled.dropped == true`, and
    // the half-applied notes fork a second time.
    let result = world.sync(0);
    assert!(
        matches!(&result, Err(error) if is_cycle(error, |e| matches!(e, CycleError::OriginRewound { .. }))),
        "half-applied {finished:?}; after the rewind: {:?}",
        result
            .as_ref()
            .map(|r| (&r.update, &r.reconciled, &r.forks))
            .map_err(|e| e.to_string())
    );
}

// ---- base inference, device refs, pins ----------------------------------------------

/// Copy tablet 0's documents onto tablet 1 (one cloud library, two
/// tablets).
fn share(world: &World) {
    world.device(0).docs().iter().for_each(|(doc, files)| {
        files.iter().for_each(|(file, bytes)| {
            world
                .device(1)
                .tablet
                .write_file(&file.xochitl_path(doc), bytes)
                .unwrap();
        });
    });
}

/// Base inference matches with the merge's own rules: B merely opened
/// the PDF (a first-open materialisation), so its clone fast-forwards it
/// to A's inked version instead of forking.
#[test]
fn a_shared_library_clone_of_an_opened_pdf_does_not_fork() {
    let world = World::new(2);
    let paper = world.create(0, Kind::Pdf, "Paper", "");
    share(&world);
    world.open_pdf(1, &paper, 3);
    world.clone_tablet(0).unwrap();
    world.open_pdf(0, &paper, 3);
    let page = world.pages(0, &paper)[0].clone();
    world.ink(0, &paper, &page);
    world.sync(0).unwrap();
    let report = world.clone_tablet(1).unwrap();
    assert!(report.inferred.contains_key(&paper), "{report:?}");
    world.converge();
    assert!(world.conflicts().is_empty(), "{:?}", world.conflicts());
}

#[test]
fn reattach_after_opening_a_pdf_does_not_fork() {
    let world = World::new(2);
    let paper = world.create(0, Kind::Pdf, "Paper", "");
    world.clone_tablet(0).unwrap();
    world.clone_tablet(1).unwrap();
    world.converge();
    world.open_pdf(1, &paper, 3);
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    world.open_pdf(0, &paper, 3);
    let page = world.pages(0, &paper)[0].clone();
    world.ink(0, &paper, &page);
    world.sync(0).unwrap();
    world.reattach(1).unwrap();
    world.converge();
    assert!(world.conflicts().is_empty(), "{:?}", world.conflicts());
}

/// Two tablets with two notebooks each, in step.
fn two_notebooks() -> (
    World,
    libremarkable_utils::hub::model::DocId,
    libremarkable_utils::hub::model::DocId,
) {
    let world = World::new(2);
    let notes = world.create(0, Kind::Notebook { pages: 1 }, "Notes", "");
    let other = world.create(0, Kind::Notebook { pages: 1 }, "Other", "");
    world.clone_tablet(0).unwrap();
    world.clone_tablet(1).unwrap();
    world.converge();
    (world, notes, other)
}

fn origin_main(world: &World) -> libremarkable_utils::git::Oid {
    world
        .git
        .open(Path::new(&world.origin))
        .unwrap()
        .rev_parse("refs/heads/main")
        .unwrap()
        .unwrap()
}

/// Origin inked the notes after A's version: inference finds A's version
/// further back and the reattach fast-forwards (without inference, the
/// empty base forks it).
#[test]
fn reattach_after_origin_inked_does_not_fork() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(1, &notes)[0].clone();
    world.ink(1, &notes, &page);
    world.sync(1).unwrap();
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    let report = world.reattach(0).unwrap();
    assert!(report.forks.is_empty(), "{report:?}");
    assert_ne!(report.inferred.get(&notes), Some(&report.base));
    world.converge();
    assert!(world.conflicts().is_empty());
}

/// Origin deleted a document after A's version: it stays deleted (without
/// inference, the reattached tablet brings it back).
#[test]
fn reattach_after_origin_deleted_keeps_it_deleted() {
    let (world, _, other) = two_notebooks();
    world.delete(1, &other);
    world.sync(1).unwrap();
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    world.reattach(0).unwrap();
    world.converge();
    assert!(!world.origin_docs().contains_key(&other));
}

/// Of the commits holding the tablet's version, the newest is the base.
#[test]
fn inference_takes_the_newest_matching_commit() {
    let (world, notes, other) = two_notebooks();
    let page = world.pages(1, &other)[0].clone();
    world.ink(1, &other, &page);
    world.sync(1).unwrap();
    let main = origin_main(&world);
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    let report = world.reattach(0).unwrap();
    assert_eq!(report.inferred.get(&notes), Some(&main), "{report:?}");
}

/// The device-ref push is a compare-and-swap that could rewind: every
/// snapshot takes the ref's head as a parent, so earlier backups stay on
/// it.
#[test]
fn the_device_ref_keeps_every_snapshot() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    let first = world.backup(0).unwrap().snapshot;
    world.ink(0, &notes, &page);
    world.backup(0).unwrap();
    world.sync(0).unwrap();
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    let id = world.device(0).state().unwrap().device_id;
    let head = repo
        .rev_parse(&format!("refs/rmu/devices/{id}"))
        .unwrap()
        .unwrap();
    assert!(repo.is_ancestor(&first, &head).unwrap());
}

/// A base override origin's `main` does not contain stops the run like a
/// rewound default base does.
#[test]
fn a_rewind_is_detected_through_a_base_override() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    // A snapshot only the device ref holds: no ancestor of `main`.
    let snapshot = world.backup(0).unwrap().snapshot;
    let device = world.device(0);
    let mut state = device.state().unwrap();
    state.base_overrides.insert(notes.clone(), Some(snapshot));
    device.set_state(&state);
    let main = origin_main(&world);
    let result = world.sync(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::OriginRewound { .. }))),
        "{result:?}"
    );
    assert_eq!(origin_main(&world), main);
    world.check_idle(0);
}

/// `.rmu/hub.json` changing on origin mid-run (the snapshot was
/// classified under the old rules) stops the run before it pushes.
#[test]
fn the_repo_config_is_pinned_for_the_run() {
    let (base, notes, _) = two_notebooks();
    let page = base.pages(0, &notes)[0].clone();
    base.ink(0, &notes, &page);
    let stop = op_index(&base, 0, OpKind::StopXochitl, 1, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    let (git, origin) = (world.git.clone(), world.origin.clone());
    let repo_id = world.device(0).state().unwrap().repo_id;
    let mut upgraded = libremarkable_utils::hub::model::RepoConfig::new(repo_id);
    upgraded
        .extra
        .insert("upgradedBy".to_string(), serde_json::json!("another tool"));
    let upgraded = upgraded.to_json().into_bytes();
    world.tablet(0).inject(Fault::EditBefore(
        stop,
        Box::new(move |_: &Path| {
            world::commit_repo_config(&git, &origin, &upgraded);
        }),
    ));
    let result = world.sync(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::RepoConfigChanged))),
        "{result:?}"
    );
    // Only the upgrade landed on `main`.
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    let main = origin_main(&world);
    let commit = repo.cat_file().unwrap().read(&main).unwrap().unwrap().1;
    assert!(String::from_utf8_lossy(&commit).contains("edit .rmu/hub.json"));
    world.check_idle(0);
}

#[test]
fn a_tablet_with_changes_on_an_unmoved_origin_fast_forwards() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    let report = world.sync(0).unwrap();
    assert_eq!(
        report.update,
        MainUpdate::FastForward(report.snapshot.clone()),
        "{report:?}"
    );
    assert_eq!(origin_main(&world), report.snapshot);
}

#[test]
fn a_pull_only_sync_pushes_no_merge_commit() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(1, &notes)[0].clone();
    world.ink(1, &notes, &page);
    world.sync(1).unwrap();
    let main = origin_main(&world);
    let report = world.sync(0).unwrap();
    assert_eq!(report.update, MainUpdate::Pulled, "{report:?}");
    assert_eq!(report.base, main);
    assert_eq!(report.applied, vec![notes.clone()]);
    assert_eq!(origin_main(&world), main);
    let again = world.sync(0).unwrap();
    assert_eq!(again.update, MainUpdate::UpToDate, "{again:?}");
    world.check();
}

#[test]
fn a_push_losing_every_attempt_stops_the_run() {
    let (base, lib) = two_tablets();
    let page = base.pages(0, &lib.notes)[1].clone();
    base.ink(0, &lib.notes, &page);
    base.rename(1, &lib.archive, "Attic");
    let journal = op_index(&base, 0, OpKind::WriteState, 2, |w| {
        w.sync(0).unwrap();
    });
    let world = base.duplicate();
    world.tablet(0).inject(Fault::EditBefore(
        journal,
        other_computer(&world, 1, None, |hub, tablet, _| {
            hub.sync(tablet).unwrap();
        }),
    ));
    let result = world.sync_with(
        0,
        HubOptions {
            push_attempts: 1,
            ..world.options.clone()
        },
    );
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::PushRetriesExhausted(1)))),
        "{result:?}"
    );
    // The journaled merge never landed: the next run drops it.
    let report = world.sync(0).unwrap();
    assert!(
        report.reconciled.as_ref().is_some_and(|r| r.dropped),
        "{report:?}"
    );
    world.converge();
}

/// The merge forks the notes; B's user writes in them while the sync has
/// the UI up. The notes are skipped, and their conflict copy (a fork
/// partner, known from the merge's trailers) with them.
#[test]
fn a_skipped_original_skips_its_conflict_copy() {
    let (base, lib) = same_page_conflict();
    let page = base.pages(1, &lib.notes)[1].clone();
    let stage = op_index(&base, 1, OpKind::Stage, 1, |w| {
        w.sync(1).unwrap();
    });
    let world = base.duplicate();
    world.inks.lock().unwrap().expect("window-ink", &lib.notes);
    let page_path = format!("{}/{page}.rm", lib.notes);
    world.tablet(1).inject(Fault::EditBefore(
        stage + 1,
        Box::new(move |dir: &Path| {
            let path = dir.join(&page_path);
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(b"stroke window-ink\n");
            std::fs::write(&path, bytes).unwrap();
        }),
    ));
    let report = world.sync(1).unwrap();
    assert_eq!(report.forks.len(), 1, "{report:?}");
    let copy = report.forks[0].copy.clone();
    assert!(
        report.skipped.contains(&lib.notes) && report.skipped.contains(&copy),
        "{report:?}"
    );
    world.converge();
}

#[test]
fn reattach_refuses_a_pending_journal_of_the_same_repository() {
    let template = crash_template();
    let (kill, _) = kill_leaving_a_half_applied_document(&template);
    let world = crashed_by(&template, Fault::KillApplyAfter(kill));
    assert!(world.device(0).state().unwrap().journal.is_some());
    let before = world.tablet(0).ops().len();
    let result = world.reattach(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::JournalPending))),
        "{result:?}"
    );
    assert!(result.unwrap_err().to_string().contains("detach --force"));
    // Refused read-only, before any pre-read.
    let ops = world.tablet(0).ops()[before..].to_vec();
    assert!(
        !ops.iter()
            .any(|op| matches!(op, OpKind::StatAll | OpKind::Download)),
        "{ops:?}"
    );
    world.check_idle(0);
    // Once `sync` has reconciled it, the reattach goes ahead.
    world.sync(0).unwrap();
    world.reattach(0).unwrap();
    world.check_idle(0);
}

#[test]
fn errors_before_the_tablet_is_touched() {
    let (world, _, _) = two_notebooks();
    // Another run on this computer holds the cache.
    let state = world.device(0).state().unwrap();
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(
            world
                .device(0)
                .cache
                .join(format!("{}.lock", state.repo_id)),
        )
        .unwrap();
    lock.try_lock().unwrap();
    let result = world.sync(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::CacheBusy(_)))),
        "{result:?}"
    );
    drop(lock);
    world.check_idle(0);
    // The state changed between the first read and the lock.
    let lock_op = op_index(&world, 0, OpKind::AcquireLock, 1, |w| {
        w.sync(0).unwrap();
    });
    let changed = world.duplicate();
    let state_file = changed.device(0).tablet.paths().state_file();
    changed.tablet(0).inject(Fault::EditBefore(
        lock_op,
        Box::new(move |_: &Path| {
            let mut state = libremarkable_utils::hub::model::TabletState::parse(
                &std::fs::read(&state_file).unwrap(),
            )
            .unwrap();
            state.origin = "/elsewhere/origin.git".to_string();
            std::fs::write(&state_file, state.to_json()).unwrap();
        }),
    ));
    let result = changed.sync(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::StateChanged))),
        "{result:?}"
    );
    // An origin that was never initialised.
    let empty = world.root.join("empty.git");
    world
        .git
        .init_bare(&empty, libremarkable_utils::git::ObjectFormat::Sha1)
        .unwrap();
    let fresh = World::new(1);
    fresh.create(0, Kind::Notebook { pages: 1 }, "Notes", "");
    let result = fresh
        .hub(0)
        .clone_tablet(fresh.tablet(0), empty.to_str().unwrap(), None, false);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::NotInitialised { .. }))),
        "{result:?}"
    );
    // A `.rmu/hub.json` this rmu cannot read.
    world::commit_repo_config(&fresh.git, &fresh.origin, b"not json");
    let result = fresh.clone_tablet(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::InvalidRepoConfig(_)))),
        "{result:?}"
    );
    // An origin emptied (replaced by an empty repository).
    std::fs::remove_dir_all(&world.origin).unwrap();
    world
        .git
        .init_bare(
            Path::new(&world.origin),
            libremarkable_utils::git::ObjectFormat::Sha1,
        )
        .unwrap();
    let result = world.sync(0);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::OriginEmpty { .. }))),
        "{result:?}"
    );
    world.check_idle(0);
}

/// A backup crashed at every operation (and with every mutation's reply
/// lost): xochitl comes back, and the next backup leaves the device ref
/// at the tree a clean backup gives.
#[test]
fn crash_at_every_op_of_a_backup() {
    let (template, notes, _) = two_notebooks();
    let page = template.pages(0, &notes)[0].clone();
    template.ink(0, &notes, &page);
    let clean = template.duplicate();
    let before = clean.tablet(0).ops().len();
    let reference = clean.backup(0).unwrap();
    let ops = clean.tablet(0).ops()[before..].to_vec();
    let mutations = ops.iter().filter(|op| op.is_mutating()).count();
    let tree = |world: &World, commit: &libremarkable_utils::git::Oid| {
        world
            .git
            .open(Path::new(&world.origin))
            .unwrap()
            .rev_parse(&format!("{commit}^{{tree}}"))
            .unwrap()
            .unwrap()
    };
    let expected = tree(&clean, &reference.snapshot);
    let faults = (1..=ops.len())
        .map(|k| {
            (
                Fault::DisconnectAtOp(k),
                format!("disconnect at op {k} ({:?})", ops[k - 1]),
            )
        })
        .chain((1..=mutations).map(|m| (Fault::LoseAck(m), format!("lost ack of mutation {m}"))));
    faults.for_each(|(fault, what)| {
        let world = template.duplicate();
        world.tablet(0).inject(fault);
        let result = world.backup(0);
        world::no_contract_violation(&result);
        let tablet = world.tablet(0);
        tablet.reconnect();
        let armed = tablet.watchdog_armed().is_some();
        assert!(
            tablet.is_running() || armed,
            "{what}: xochitl down, no watchdog"
        );
        if armed {
            tablet.fire_watchdog();
        }
        assert!(tablet.is_running(), "{what}: xochitl did not come back");
        let report = match world.backup(0) {
            Ok(report) => report,
            Err(error) if is_locked(&error) => {
                assert!(!armed, "{what}: a lock with a fired watchdog must be stale");
                world
                    .hub_with(
                        0,
                        HubOptions {
                            break_lock: true,
                            ..world.options.clone()
                        },
                    )
                    .backup(world.tablet(0))
                    .unwrap_or_else(|e| panic!("{what}: after --break-lock: {e}"))
            }
            Err(error) => panic!("{what}: the next backup failed: {error}"),
        };
        assert_eq!(tree(&world, &report.snapshot), expected, "{what}");
        let id = world.device(0).state().unwrap().device_id;
        let head = world
            .git
            .open(Path::new(&world.origin))
            .unwrap()
            .rev_parse(&format!("refs/rmu/devices/{id}"))
            .unwrap();
        assert_eq!(head, Some(report.snapshot), "{what}");
        world.check_idle(0);
    });
}

// ---- remote, detach, init, cache hygiene ---------------------------------------------

/// A hub on tablet `i`'s computer that uses the origin stored on the
/// tablet (no `--origin`).
fn hub_without_origin(world: &World, i: usize) -> libremarkable_utils::hub::cycle::Hub<'static> {
    static PROGRESS: libremarkable_utils::progress::NoProgress =
        libremarkable_utils::progress::NoProgress;
    libremarkable_utils::hub::cycle::Hub::new(
        world.git.clone(),
        world.device(i).cache.clone(),
        libremarkable_utils::hub::device::LockRequest::for_this_process("test-host"),
        &PROGRESS,
    )
    .with_options(world.options.clone())
}

#[test]
fn remote_shows_and_set_url_changes_the_stored_origin() {
    let (world, notes, _) = two_notebooks();
    let hub = hub_without_origin(&world, 0);
    let shown = hub.remote(world.tablet(0)).unwrap();
    assert_eq!(shown.origin, world.origin);
    assert_eq!(shown.effective, world.origin);
    assert_eq!(shown.device_name.as_str(), "Tablet A");
    let elsewhere = "/elsewhere/origin.git".to_string();
    let overridden = world.hub(0).remote(world.tablet(0)).unwrap();
    assert_eq!(overridden.effective, world.origin);
    // Origin moved: point the tablet at its new place, then sync.
    let moved = world.root.join("moved.git");
    std::fs::rename(&world.origin, &moved).unwrap();
    let moved = moved.to_str().unwrap().to_string();
    let report = hub.set_remote_url(world.tablet(0), &moved).unwrap();
    assert_eq!(report.previous, world.origin);
    assert_eq!(report.remote.origin, moved);
    assert_eq!(world.device(0).state().unwrap().origin, moved);
    world.check_idle(0);
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    assert_ne!(
        hub.sync(world.tablet(0)).unwrap().update,
        MainUpdate::UpToDate
    );
    // Refused: another repository, an origin without `main`, a missing
    // one, credentials in the URL. The stored URL stays.
    let other = World::new(1);
    let refused = |url: &str, expected: fn(&CycleError) -> bool| {
        let result = hub.set_remote_url(world.tablet(0), url);
        assert!(
            matches!(&result, Err(e) if is_cycle(e, expected)),
            "{url}: {result:?}"
        );
        assert_eq!(world.device(0).state().unwrap().origin, moved);
        world.check_idle(0);
    };
    refused(&other.origin, |e| {
        matches!(e, CycleError::RepoMismatch { .. })
    });
    let empty = world.root.join("empty.git");
    world
        .git
        .init_bare(&empty, libremarkable_utils::git::ObjectFormat::Sha1)
        .unwrap();
    refused(empty.to_str().unwrap(), |e| {
        matches!(e, CycleError::OriginEmpty { .. })
    });
    refused("https://user:secret@example.invalid/x.git", |e| {
        matches!(e, CycleError::CredentialsInUrl)
    });
    assert!(hub.set_remote_url(world.tablet(0), &elsewhere).is_err());
    assert_eq!(world.device(0).state().unwrap().origin, moved);
    // Not attached.
    let detached = World::new(1);
    assert!(matches!(
        hub_without_origin(&detached, 0).remote(detached.tablet(0)),
        Err(e) if is_cycle(&e, |e| matches!(e, CycleError::NotAttached))
    ));
}

#[test]
fn detach_forgets_the_hub_state_and_can_prune_the_device_ref() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    world.sync(0).unwrap();
    let state = world.device(0).state().unwrap();
    let device_ref = format!("refs/rmu/devices/{}", state.device_id);
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    assert!(repo.rev_parse(&device_ref).unwrap().is_some());
    let docs = world.device(0).docs();
    let report = world
        .hub(0)
        .detach(
            world.tablet(0),
            DetachOptions {
                prune_device_ref: true,
                ..DetachOptions::default()
            },
        )
        .unwrap();
    assert_eq!(report.pruned.as_deref(), Some(device_ref.as_str()));
    assert_eq!(report.repo_id, state.repo_id);
    assert!(report.warnings.is_empty(), "{report:?}");
    assert!(world.device(0).state().is_none());
    assert_eq!(world.device(0).docs(), docs);
    assert_eq!(repo.rev_parse(&device_ref).unwrap(), None);
    world.check_idle(0);
    assert!(matches!(
        world.hub(0).detach(world.tablet(0), DetachOptions::default()),
        Err(e) if is_cycle(&e, |e| matches!(e, CycleError::NotAttached))
    ));
    // Attached again: a new device id, and nothing forks.
    let report = world.clone_tablet(0).unwrap();
    assert!(report.forks.is_empty(), "{report:?}");
    assert_ne!(world.device(0).state().unwrap().device_id, state.device_id);
    world.converge();
    assert!(world.conflicts().is_empty());
}

/// With an interrupted sync's journal pending, detach refuses (the
/// interrupted apply may have left documents half-updated; sync finishes
/// them) and changes nothing; with `force` it discards the journal, with
/// a warning.
#[test]
fn detach_refuses_a_pending_journal_unless_forced() {
    let template = crash_template();
    let (kill, _) = kill_leaving_a_half_applied_document(&template);
    let world = crashed_by(&template, Fault::KillApplyAfter(kill));
    let tablet = world.tablet(0);
    tablet.reconnect();
    if tablet.watchdog_armed().is_some() {
        tablet.fire_watchdog();
    }
    let state = world.device(0).state().unwrap();
    assert!(state.journal.is_some());
    let result = world.hub(0).detach(
        tablet,
        DetachOptions {
            prune_device_ref: true,
            ..DetachOptions::default()
        },
    );
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::DetachJournalPending))),
        "{result:?}"
    );
    assert!(result.unwrap_err().to_string().contains("rmu hub sync"));
    assert_eq!(world.device(0).state(), Some(state.clone()));
    world.check_idle(0);
    let report = world
        .hub(0)
        .detach(
            tablet,
            DetachOptions {
                force: true,
                ..DetachOptions::default()
            },
        )
        .unwrap();
    assert_eq!(report.pruned, None);
    assert_eq!(report.warnings, vec![Warning::JournalDiscarded]);
    assert!(world.device(0).state().is_none());
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    assert!(
        repo.rev_parse(&format!("refs/rmu/devices/{}", state.device_id))
            .unwrap()
            .is_some()
    );
    world.check_idle(0);
}

/// Records progress calls.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

impl libremarkable_utils::progress::Progress for Recorder {
    fn step(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_string());
    }
    fn bytes(&self, _: u64, _: Option<u64>) {}
    fn bytes_done(&self) {}
    fn finished(&self) {
        self.0.lock().unwrap().push("finished".to_string());
    }
}

#[test]
fn init_creates_a_local_origin_and_cleans_up_after_a_failure() {
    let world = World::new(1);
    let recorder: &'static Recorder = Box::leak(Box::default());
    let cache = world.root.join("init-cache");
    let hub = |cache: std::path::PathBuf| {
        libremarkable_utils::hub::cycle::Hub::new(
            world.git.clone(),
            cache,
            libremarkable_utils::hub::device::LockRequest::for_this_process("test-host"),
            recorder,
        )
    };
    let entries = |dir: &Path| -> std::collections::BTreeSet<String> {
        std::fs::read_dir(dir)
            .map(|read| {
                read.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    // A new local path becomes a bare repository with `main`.
    let fresh = world.root.join("fresh.git");
    let report = hub(cache.clone()).init(fresh.to_str().unwrap()).unwrap();
    let repo = world.git.open(&fresh).unwrap();
    assert_eq!(
        repo.rev_parse("refs/heads/main").unwrap(),
        Some(report.commit)
    );
    assert_eq!(
        recorder.0.lock().unwrap().last().map(String::as_str),
        Some("finished")
    );
    // Already initialised: refused, and the cache it set up is gone.
    let before = entries(&cache);
    recorder.0.lock().unwrap().clear();
    let result = hub(cache.clone()).init(fresh.to_str().unwrap());
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::AlreadyInitialised { .. }))),
        "{result:?}"
    );
    assert_eq!(entries(&cache), before);
    assert_eq!(
        *recorder.0.lock().unwrap(),
        vec!["Initialising origin".to_string(), "finished".to_string()]
    );
    // A failure after creating a new local path removes it again.
    let blocked = world.root.join("not-a-directory");
    std::fs::write(&blocked, b"").unwrap();
    let doomed = world.root.join("doomed.git");
    assert!(hub(blocked).init(doomed.to_str().unwrap()).is_err());
    assert!(!doomed.exists());
}

/// A clone killed mid-fetch leaves its temporary repository; the next
/// clone on that computer removes it, but not one a running clone holds.
#[test]
fn stale_incoming_repositories_are_swept() {
    let world = World::new(1);
    world.create(0, Kind::Notebook { pages: 1 }, "Notes", "");
    let cache = world.device(0).cache.clone();
    std::fs::create_dir_all(cache.join("incoming-dead.git/objects")).unwrap();
    std::fs::create_dir_all(cache.join("incoming-live.git/objects")).unwrap();
    let live = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join("incoming-live.lock"))
        .unwrap();
    live.try_lock().unwrap();
    world.clone_tablet(0).unwrap();
    assert!(!cache.join("incoming-dead.git").exists());
    assert!(cache.join("incoming-live.git").exists());
    drop(live);
    world.check();
}

/// A pull-only run (A has no changes of its own; B inked, renamed, added
/// and deleted) crashed before every operation, and with its apply killed
/// after every move. Its journal names a snapshot `main` does not contain:
/// reconciliation keeps unfinished documents on their previous bases, the
/// recovering run pushes nothing either, and the tablet ends in step.
#[test]
fn a_pull_only_sync_crashed_at_every_step_recovers() {
    let (template, lib) = two_tablets();
    let page = template.pages(1, &lib.notes)[0].clone();
    template.ink(1, &lib.notes, &page);
    template.rename(1, &lib.paper, "Paper (B)");
    template.create(
        1,
        Kind::Notebook { pages: 1 },
        "From B",
        lib.folder.as_str(),
    );
    template.delete(1, &lib.diary);
    template.sync(1).unwrap();
    let main = origin_main(&template);
    let clean = template.duplicate();
    let before = clean.tablet(0).ops().len();
    let report = clean.sync(0).unwrap();
    assert_eq!(report.update, MainUpdate::Pulled, "{report:?}");
    assert!(report.applied.len() >= 4, "{report:?}");
    let ops = clean.tablet(0).ops()[before..].to_vec();
    let faults = (1..=ops.len())
        .map(|k| {
            (
                Fault::DisconnectAtOp(k),
                format!("disconnect at op {k} ({:?})", ops[k - 1]),
            )
        })
        .chain((0..12).map(|k| (Fault::KillApplyAfter(k), format!("apply killed after {k}"))));
    faults.for_each(|(fault, what)| {
        let world = template.duplicate();
        world.tablet(0).inject(fault);
        let result = world.sync(0);
        world::no_contract_violation(&result);
        let tablet = world.tablet(0);
        tablet.reconnect();
        let armed = tablet.watchdog_armed().is_some();
        assert!(
            tablet.is_running() || armed,
            "{what}: xochitl down, no watchdog"
        );
        if armed {
            tablet.fire_watchdog();
        }
        match world.sync(0) {
            Ok(_) => {}
            Err(error) if is_locked(&error) => {
                assert!(!armed, "{what}: a lock with a fired watchdog must be stale");
                world
                    .sync_with(
                        0,
                        HubOptions {
                            break_lock: true,
                            ..world.options.clone()
                        },
                    )
                    .unwrap_or_else(|e| panic!("{what}: after --break-lock: {e}"));
            }
            Err(error) => panic!("{what}: the next run failed: {error}"),
        }
        let quiet = world.sync(0).unwrap();
        assert!(
            quiet.update == MainUpdate::UpToDate && quiet.applied.is_empty(),
            "{what}: not quiet: {quiet:?}"
        );
        assert_eq!(origin_main(&world), main, "{what}: pushed to main");
        world.converge();
        assert!(
            world.conflicts().is_empty(),
            "{what}: {:?}",
            world.conflicts()
        );
    });
}

/// Every report serialises (for the CLI's `--json`), with tagged enums.
#[test]
fn reports_serialise_to_json() {
    let (world, lib) = same_page_conflict();
    let status = serde_json::to_value(world.status(1).unwrap()).unwrap();
    assert_eq!(status["lock"]["state"], "free");
    assert!(status["read"]["downloaded"]["files"].is_u64(), "{status}");
    let dry = serde_json::to_value(world.dry_run(1).unwrap()).unwrap();
    assert_eq!(dry["would_push"], true);
    let report = world.sync(1).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["update"]["kind"], "merge", "{json}");
    assert_eq!(json["update"]["commit"], report.base.as_str());
    assert_eq!(json["forks"][0]["original"], lib.notes.as_str());
    assert!(
        json["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["kind"] == "forked" && note["doc"] == lib.notes.as_str()),
        "{json}"
    );
    assert!(json["names"].is_object(), "{json}");
    let quiet = serde_json::to_value(world.sync(1).unwrap()).unwrap();
    assert_eq!(quiet["update"], serde_json::json!({ "kind": "up_to_date" }));
    let backup = serde_json::to_value(world.backup(0).unwrap()).unwrap();
    assert!(backup["snapshot"].is_string(), "{backup}");
    let warning = serde_json::to_value(Warning::MixedModel {
        seen: "a".to_string(),
        model: "b".to_string(),
    })
    .unwrap();
    assert_eq!(warning["kind"], "mixed_model");
}

// ---- scale ----------------------------------------------------------------------------

/// `docs` notebooks cloned from tablet 0, then `rounds` merges on origin
/// each renaming one of them (round-robin), then the tablet's state lost.
fn long_history(
    docs: usize,
    rounds: usize,
) -> (World, Vec<libremarkable_utils::hub::model::DocId>) {
    let world = World::new(1);
    let ids: Vec<_> = (0..docs)
        .map(|k| world.create(0, Kind::Notebook { pages: 1 }, &format!("Doc {k}"), ""))
        .collect();
    world.clone_tablet(0).unwrap();
    let metadata: Vec<serde_json::Value> = ids
        .iter()
        .map(|doc| world.device(0).metadata(doc).unwrap())
        .collect();
    world::append_history(&world.root, &world.origin, rounds, |round| {
        let k = round % docs;
        let mut renamed = metadata[k].clone();
        renamed["visibleName"] = serde_json::json!(format!("Doc {k} (round {round})"));
        vec![(
            format!("docs/{}/metadata", ids[k]),
            serde_json::to_vec_pretty(&renamed).unwrap(),
        )]
    });
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    (world, ids)
}

/// Reattach after a long history: every document's version is one origin
/// had long ago (`renamed = false`), or none (the tablet renamed every
/// document after losing its state). Returns the reattach's wall time and
/// peak RSS growth.
fn reattach_at_scale(
    template: &World,
    ids: &[libremarkable_utils::hub::model::DocId],
    renamed: bool,
) -> (std::time::Duration, Option<u64>) {
    let world = template.duplicate();
    if renamed {
        ids.iter()
            .enumerate()
            .for_each(|(k, doc)| world.rename(0, doc, &format!("Doc {k} (tablet)")));
    }
    let before = world::peak_rss_kib(true);
    let started = std::time::Instant::now();
    let report = world.reattach(0).unwrap();
    let took = started.elapsed();
    let grew = world::peak_rss_kib(false)
        .zip(before)
        .map(|(after, before)| after.saturating_sub(before));
    let expected = if renamed { 0 } else { ids.len() };
    assert_eq!(report.inferred.len(), expected, "{:?}", report.update);
    (took, grew)
}

fn scale(docs: usize, rounds: usize, scenarios: &[bool]) {
    let (template, ids) = long_history(docs, rounds);
    scenarios.iter().for_each(|&renamed| {
        let (took, grew) = reattach_at_scale(&template, &ids, renamed);
        eprintln!(
            "reattach, {docs} docs, {rounds} merges ({} commits), {}: {took:.2?}, peak RSS +{} KiB",
            2 * rounds,
            if renamed {
                "nothing matches"
            } else {
                "old versions match"
            },
            grew.map_or("?".to_string(), |kib| kib.to_string()),
        );
    });
}

#[test]
fn reattach_after_a_long_history_infers_every_base() {
    scale(100, 150, &[false, true]);
}

// The measurements quoted in the PR, one process each (peak RSS is per
// process): `cargo test --release -p libremarkable-utils --test hub_e2e
// -- --ignored --nocapture reattach_scale_<scenario>`.

#[test]
#[ignore = "scale measurement; run in release with --ignored --nocapture"]
fn reattach_scale_old_versions_match() {
    scale(1000, 1200, &[false]);
}

#[test]
#[ignore = "scale measurement; run in release with --ignored --nocapture"]
fn reattach_scale_nothing_matches() {
    scale(1000, 1200, &[true]);
}

/// The same library with a one-merge history: what a reattach costs
/// without history (reading the tablet, the snapshot, the merge).
#[test]
#[ignore = "scale measurement; run in release with --ignored --nocapture"]
fn reattach_scale_short_history() {
    scale(1000, 1, &[false]);
}

// ---- resilient pre-read ----------------------------------------------------------------

/// A tablet with `docs` notebooks of two pages, its mirrored file count,
/// and options that checkpoint the pre-read after every file.
fn library_to_read(docs: usize) -> (World, usize, HubOptions) {
    let world = World::new(1);
    (0..docs).for_each(|k| {
        world.create(0, Kind::Notebook { pages: 2 }, &format!("Doc {k}"), "");
    });
    let files = world
        .device(0)
        .docs()
        .values()
        .map(|files| files.len())
        .sum();
    let options = HubOptions {
        pre_read_checkpoint: 1,
        ..world.options.clone()
    };
    (world, files, options)
}

fn clone_with(
    world: &World,
    options: HubOptions,
) -> libremarkable_utils::error::Result<libremarkable_utils::hub::cycle::SyncReport> {
    let name = libremarkable_utils::hub::model::DeviceName::new("Tablet A").unwrap();
    world
        .hub_with(0, options)
        .clone_tablet(world.tablet(0), &world.origin, Some(name), false)
}

/// The pre-read of a first clone is cut inside its seventh file (a page,
/// so the cut ends the `fast-import` it streams into): the six before
/// were checkpointed, so the stopped read downloads only the rest.
/// Without checkpoints the cut loses the whole pre-read.
#[test]
fn a_pre_read_cut_mid_file_keeps_its_checkpointed_files() {
    let (template, files, options) = library_to_read(4);
    let world = template.duplicate();
    world.tablet(0).inject(Fault::CutDownloadInFile(6));
    let report = clone_with(&world, options).unwrap();
    assert_eq!(report.read.downloaded.files, files - 6, "{report:?}");
    world.check();
    // The default (64 MiB) checkpoints nothing in so small a library.
    let world = template.duplicate();
    world.tablet(0).inject(Fault::CutDownloadInFile(6));
    let report = world.clone_tablet(0).unwrap();
    assert_eq!(report.read.downloaded.files, files, "{report:?}");
}

/// The run whose pre-read was cut fails before its stopped read (the
/// connection is gone); the next run reuses what that pre-read saved,
/// whether its own pre-read gets nothing (its download fails) or fails
/// outright (its stat pass), which must not wipe what was saved. (Each
/// scenario cuts its own copy of the world: a copy's files have new
/// stats.)
#[test]
fn the_next_run_reuses_a_cut_pre_read() {
    let (template, files, options) = library_to_read(4);
    let lock = op_index(&template, 0, OpKind::AcquireLock, 1, |w| {
        w.clone_tablet(0).unwrap();
    });
    let stat = op_index(&template, 0, OpKind::StatAll, 1, |w| {
        w.clone_tablet(0).unwrap();
    });
    [
        (Fault::FailDownloadAfter(0), "its download fails"),
        (Fault::FailOp(stat), "its stat pass fails"),
    ]
    .into_iter()
    .for_each(|(fault, what)| {
        let world = template.duplicate();
        world.tablet(0).inject(Fault::CutDownloadInFile(6));
        world.tablet(0).inject(Fault::DisconnectAtOp(lock));
        assert!(clone_with(&world, options.clone()).is_err());
        world.tablet(0).reconnect();
        world.check_idle(0);
        // Saved for this tablet (by where it is reached: it has no device
        // id yet), not for every tablet cloned from this computer.
        let saved: Vec<String> = std::fs::read_dir(&world.device(0).cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".pre-read-"))
            .collect();
        assert!(
            saved.len() == 1 && saved[0].contains(".pre-read-at-"),
            "{saved:?}"
        );
        world.tablet(0).inject(fault);
        let report = clone_with(&world, options.clone()).unwrap();
        assert_eq!(
            report.read.downloaded.files,
            files - 6,
            "{what}: {report:?}"
        );
        world.check();
    });
}

/// A first clone is cut during its stopped read, after a complete
/// pre-read: the lock stays. Whether the next run finds it stale through
/// the fired watchdog (it pre-reads again) or through a reboot (no
/// pre-read: the boot id is not read before the lock), it reuses what the
/// first pre-read saved and downloads nothing while xochitl is stopped.
#[test]
fn a_crashed_first_clone_reuses_its_pre_read() {
    let (template, _, options) = library_to_read(3);
    let stopped_read = op_index(&template, 0, OpKind::StatAll, 2, |w| {
        w.clone_tablet(0).unwrap();
    });
    [true, false].into_iter().for_each(|watchdog| {
        let world = template.duplicate();
        let tablet = world.tablet(0);
        tablet.inject(Fault::DisconnectAtOp(stopped_read));
        assert!(clone_with(&world, options.clone()).is_err());
        tablet.reconnect();
        if watchdog {
            assert!(tablet.fire_watchdog());
        } else {
            tablet.reboot();
        }
        let before = tablet.ops().len();
        let report = clone_with(&world, options.clone()).unwrap();
        let stat_passes = tablet.ops()[before..]
            .iter()
            .filter(|op| **op == OpKind::StatAll)
            .count();
        assert_eq!(
            stat_passes,
            if watchdog { 2 } else { 1 },
            "watchdog {watchdog}"
        );
        assert_eq!(
            report.read.downloaded.files, 0,
            "watchdog {watchdog}: {report:?}"
        );
        assert!(report.took_over.is_some(), "{report:?}");
        world.check();
    });
}

/// A tablet another run holds is not read at all before the lock refuses
/// this run.
#[test]
fn a_locked_tablet_gets_no_pre_read() {
    let (world, _, _) = two_notebooks();
    let other = libremarkable_utils::hub::device::LockRequest::for_this_process("other-host");
    let tablet = world.tablet(0);
    let held =
        libremarkable_utils::hub::device::Tablet::acquire_lock(tablet, &other, false).unwrap();
    let before = tablet.ops().len();
    let result = world.sync(0);
    assert!(matches!(&result, Err(e) if is_locked(e)), "{result:?}");
    let ops = tablet.ops()[before..].to_vec();
    assert!(
        !ops.iter()
            .any(|op| matches!(op, OpKind::StatAll | OpKind::Download)),
        "{ops:?}"
    );
    // Breaking it, the run pre-reads (two stat passes) and goes ahead.
    let before = tablet.ops().len();
    let report = world
        .sync_with(
            0,
            HubOptions {
                break_lock: true,
                ..world.options.clone()
            },
        )
        .unwrap();
    assert!(report.took_over.is_some(), "{report:?}");
    let stat_passes = tablet.ops()[before..]
        .iter()
        .filter(|op| **op == OpKind::StatAll)
        .count();
    assert_eq!(stat_passes, 2);
    assert!(libremarkable_utils::hub::device::Tablet::release_lock(tablet, &held).is_err());
}

/// A tablet paired with the cloud after it was attached: `sync` refuses
/// before reading anything.
#[test]
fn a_paired_tablet_gets_no_pre_read_on_sync() {
    let (world, _, _) = two_notebooks();
    let paired = libremarkable_utils::hub::device::FakeTablet::builder(&world.device(0).dir)
        .cloud_paired(true)
        .build()
        .unwrap();
    let result = world.hub(0).sync(&paired);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::CloudPaired))),
        "{result:?}"
    );
    assert!(
        !paired
            .ops()
            .iter()
            .any(|op| matches!(op, OpKind::StatAll | OpKind::Download)),
        "{:?}",
        paired.ops()
    );
}

/// While a journal is pending, `set-url` requires the new origin to have
/// its merge (only there can `sync` finish it): a copy of origin rewound
/// to before the merge is refused, before the lock and (a journal that
/// appears meanwhile) under it; an intact copy is accepted.
#[test]
fn set_url_requires_a_pending_journals_merge() {
    let template = crash_template();
    let before_merge = world::git_cli(
        &template.root,
        &[
            &format!("--git-dir={}", template.origin),
            "rev-parse",
            "refs/heads/main",
        ],
    );
    let (kill, _) = kill_leaving_a_half_applied_document(&template);
    let world = crashed_by(&template, Fault::KillApplyAfter(kill));
    let tablet = world.tablet(0);
    tablet.reconnect();
    if tablet.watchdog_armed().is_some() {
        tablet.fire_watchdog();
    }
    let journal = world.device(0).state().unwrap().journal.unwrap();
    let copy = |name: &str, main: Option<&str>| -> String {
        let path = world.root.join(name).to_str().unwrap().to_string();
        world::git_cli(
            &world.root,
            &["clone", "-q", "--mirror", &world.origin, &path],
        );
        if let Some(main) = main {
            world::git_cli(
                &world.root,
                &[
                    &format!("--git-dir={path}"),
                    "update-ref",
                    "refs/heads/main",
                    main,
                ],
            );
        }
        path
    };
    let hub = hub_without_origin(&world, 0);
    let stored = world.device(0).state().unwrap().origin;
    let rewound = copy("rewound.git", Some(&before_merge));
    let ops_before = tablet.ops().len();
    let result = hub.set_remote_url(tablet, &rewound);
    // Refused before the lock is even taken.
    assert!(
        !tablet.ops()[ops_before..].contains(&OpKind::AcquireLock),
        "{:?}",
        tablet.ops()
    );
    assert!(
        matches!(
            &result,
            Err(libremarkable_utils::error::Error::HubCycle(CycleError::JournalMergeMissing { merge }))
                if *merge == journal.merge
        ),
        "{result:?}"
    );
    assert_eq!(world.device(0).state().unwrap().origin, stored);
    world.check_idle(0);
    // Under the lock: the journal is only there once the lock is taken.
    let mut state = world.device(0).state().unwrap();
    state.journal = None;
    world.device(0).set_state(&state);
    let lock = op_index(&world, 0, OpKind::AcquireLock, 1, |w| {
        hub_without_origin(w, 0)
            .set_remote_url(w.tablet(0), &rewound)
            .unwrap();
    });
    let state_file = world.device(0).tablet.paths().state_file();
    let pending = journal.clone();
    tablet.inject(Fault::EditBefore(
        lock,
        Box::new(move |_: &Path| {
            let mut state = libremarkable_utils::hub::model::TabletState::parse(
                &std::fs::read(&state_file).unwrap(),
            )
            .unwrap();
            state.journal = Some(pending);
            std::fs::write(&state_file, state.to_json()).unwrap();
        }),
    ));
    let result = hub.set_remote_url(tablet, &rewound);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::JournalMergeMissing { .. }))),
        "{result:?}"
    );
    assert_eq!(world.device(0).state().unwrap().origin, stored);
    world.check_idle(0);
    // A copy that has the merge is fine.
    let intact = copy("intact.git", None);
    hub.set_remote_url(tablet, &intact).unwrap();
    assert_eq!(world.device(0).state().unwrap().origin, intact);
}

/// A deletion the merge does not carry to origin (another tablet changed
/// the document, so it is kept) is not in the guard's list: `status`
/// still names it, from its base.
#[test]
fn status_names_a_deletion_kept_over_by_origin() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(1, &notes)[0].clone();
    world.ink(1, &notes, &page);
    world.sync(1).unwrap();
    world.delete(0, &notes);
    let status = world.status(0).unwrap();
    assert_eq!(
        status.changes.get(&notes),
        Some(&libremarkable_utils::hub::cycle::DocChange::Deleted)
    );
    assert!(!status.guard.from_origin.contains(&notes), "{status:?}");
    assert_eq!(status.names.get(&notes).map(String::as_str), Some("Notes"));
}

/// A journal is pending and origin is rewound past its merge: `sync`
/// stops (`OriginRewound`, naming reattach), and the reattach it names
/// goes ahead, discarding the journal nothing can reconcile any more,
/// instead of sending the user back to `sync`.
#[test]
fn reattach_after_a_rewind_discards_a_journal_origin_lost() {
    let template = crash_template();
    let repo = template.git.open(Path::new(&template.origin)).unwrap();
    let before = repo.rev_parse("refs/heads/main").unwrap().unwrap();
    let (kill, _) = kill_leaving_a_half_applied_document(&template);
    let world = crashed_by(&template, Fault::KillApplyAfter(kill));
    let repo = world.git.open(Path::new(&world.origin)).unwrap();
    repo.update_ref("refs/heads/main", &before, None).unwrap();
    let tablet = world.tablet(0);
    tablet.reconnect();
    if tablet.watchdog_armed().is_some() {
        tablet.fire_watchdog();
    }
    let error = world.sync(0).unwrap_err();
    assert!(
        is_cycle(&error, |e| matches!(e, CycleError::OriginRewound { .. })),
        "{error}"
    );
    assert!(error.to_string().contains("--reattach"), "{error}");
    assert!(error.to_string().contains("rmu hub detach"), "{error}");
    let report = world.reattach(0).unwrap();
    assert!(
        report.warnings.contains(&Warning::JournalDiscarded),
        "{report:?}"
    );
    assert!(world.device(0).state().unwrap().journal.is_none());
    // The documents that lost their `.metadata` are in no base after a
    // reattach (left alone); repaired, everything converges, ink intact.
    world.repair(0);
    world.converge();
}

/// Origin moved with `git clone --bare`, which copies no `refs/rmu/*`:
/// `set-url` refuses it with a message that says so (the next sync would
/// otherwise stop on the missing device ref), and accepts it once the
/// device refs are copied too.
#[test]
fn set_url_refuses_an_origin_moved_without_its_device_refs() {
    let (world, notes, _) = two_notebooks();
    let hub = hub_without_origin(&world, 0);
    let moved = world.root.join("moved.git");
    let moved_url = moved.to_str().unwrap().to_string();
    world::git_cli(
        &world.root,
        &["clone", "-q", "--bare", &world.origin, &moved_url],
    );
    let result = hub.set_remote_url(world.tablet(0), &moved_url);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::DeviceRefMissing { .. }))),
        "{result:?}"
    );
    assert!(result.unwrap_err().to_string().contains("refs/rmu/*"));
    assert_eq!(world.device(0).state().unwrap().origin, world.origin);
    world.check_idle(0);
    world::git_cli(
        &world.root,
        &[
            &format!("--git-dir={}", world.origin),
            "push",
            "-q",
            &moved_url,
            "refs/rmu/*:refs/rmu/*",
        ],
    );
    hub.set_remote_url(world.tablet(0), &moved_url).unwrap();
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    hub.sync(world.tablet(0)).unwrap();
}

/// Between `set-url`'s first check and its lock, another computer syncs
/// the tablet to the old origin: its new base is not in the new URL's
/// history. Checked again under the lock, the URL is refused rather than
/// stored for the next sync to stop on.
#[test]
fn set_url_checks_the_bases_again_under_the_lock() {
    let (base, notes, _) = two_notebooks();
    let moved = |world: &World| world.root.join("moved.git").to_str().unwrap().to_string();
    world::git_cli(
        &base.root,
        &["clone", "-q", "--mirror", &base.origin, &moved(&base)],
    );
    let page = base.pages(0, &notes)[0].clone();
    base.ink(0, &notes, &page);
    let lock = op_index(&base, 0, OpKind::AcquireLock, 1, |w| {
        hub_without_origin(w, 0)
            .set_remote_url(w.tablet(0), &moved(w))
            .unwrap();
    });
    let world = base.duplicate();
    let elsewhere = world.root.join("computer-x").join("cache");
    world.tablet(0).inject(Fault::EditBefore(
        lock,
        other_computer(&world, 0, Some(elsewhere), |hub, tablet, _| {
            let report = hub.sync(tablet).unwrap();
            assert_ne!(report.update, MainUpdate::UpToDate);
        }),
    ));
    // (A copied world's tablet still stores the template's origin URL.)
    let stored = world.device(0).state().unwrap().origin;
    let result = hub_without_origin(&world, 0).set_remote_url(world.tablet(0), &moved(&world));
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::OriginRewound { .. }))),
        "{result:?}"
    );
    assert_eq!(world.device(0).state().unwrap().origin, stored);
    world.check_idle(0);
}

// ---- half-applied documents edited before the next run ----------------------------

/// A pull-only run (B renamed the notes, inked the diary, renamed the
/// paper, added a notebook) crashed at every operation and apply move;
/// before the next run A's user inks the notes and renames the diary.
/// Both sides' changes must survive with no conflict copy: a base that
/// is too new would revert B's rename of the notes (A's files still have
/// the old name), one that is too old would fork.
#[test]
fn a_pull_only_run_crashed_then_edited() {
    crashed_then_edited(false);
}

#[test]
fn a_merge_run_crashed_then_edited() {
    crashed_then_edited(true);
}

fn crashed_then_edited(own_change: bool) {
    let (template, lib) = two_tablets();
    if own_change {
        template.rename(0, &lib.sketch, "Sketch (A)");
    }
    let page = template.pages(1, &lib.diary)[0].clone();
    template.ink(1, &lib.diary, &page);
    template.rename(1, &lib.notes, "Notes (B)");
    template.rename(1, &lib.paper, "Paper (B)");
    template.create(
        1,
        Kind::Notebook { pages: 1 },
        "From B",
        lib.folder.as_str(),
    );
    template.sync(1).unwrap();
    let clean = template.duplicate();
    let before = clean.tablet(0).ops().len();
    let update = clean.sync(0).unwrap().update;
    assert_eq!(update == MainUpdate::Pulled, !own_change, "{update:?}");
    let ops = clean.tablet(0).ops()[before..].to_vec();
    let faults = (1..=ops.len())
        .map(|k| {
            (
                Fault::DisconnectAtOp(k),
                format!("disconnect at op {k} ({:?})", ops[k - 1]),
            )
        })
        .chain((0..14).map(|k| (Fault::KillApplyAfter(k), format!("apply killed after {k}"))));
    let mut failures = Vec::new();
    faults.for_each(|(fault, what)| {
        let world = template.duplicate();
        world.tablet(0).inject(fault);
        let _ = world.sync(0);
        let tablet = world.tablet(0);
        tablet.reconnect();
        if tablet.watchdog_armed().is_some() {
            tablet.fire_watchdog();
        }
        let notes_page = world.pages(0, &lib.notes)[0].clone();
        world.ink(0, &lib.notes, &notes_page);
        world.rename(0, &lib.diary, "Diary (A)");
        let next = world.sync(0);
        if let Err(error) = &next {
            if is_locked(error) {
                world
                    .sync_with(
                        0,
                        HubOptions {
                            break_lock: true,
                            ..world.options.clone()
                        },
                    )
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
            } else {
                panic!("{what}: {error}");
            }
        }
        world.converge();
        let name = |doc: &libremarkable_utils::hub::model::DocId| {
            world.device(1).metadata(doc).unwrap()["visibleName"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let got = (name(&lib.notes), name(&lib.diary), name(&lib.paper));
        let expected = (
            "Notes (B)".to_string(),
            "Diary (A)".to_string(),
            "Paper (B)".to_string(),
        );
        if got != expected || !world.conflicts().is_empty() {
            let reconciled = format!("{:?}", next.as_ref().ok().map(|r| r.reconciled.clone()));
            failures.push(format!(
                "{what}: {got:?}, conflicts {:?}, reconciled {reconciled}",
                world.conflicts()
            ));
        }
    });
    assert!(failures.is_empty(), "{failures:#?}");
}

// ---- small gaps -------------------------------------------------------------------------

/// A's own version of the notes exists only in its merged snapshot (its
/// ink merged with B's rename, the merge's second parent): its apply is
/// killed, its state lost, and B inks on top. Inference over every
/// ancestor of `main` finds that snapshot; over first parents only it
/// would find nothing and fork.
#[test]
fn reattach_matches_a_version_only_in_a_merged_snapshot() {
    let (world, notes, _) = two_notebooks();
    let page = world.pages(0, &notes)[0].clone();
    world.ink(0, &notes, &page);
    world.rename(1, &notes, "Notes (B)");
    world.sync(1).unwrap();
    world.tablet(0).inject(Fault::KillApplyAfter(0));
    assert!(world.sync(0).is_err());
    let snapshot = world.device(0).state().unwrap().journal.unwrap().snapshot;
    world.tablet(0).reconnect();
    world.tablet(0).fire_watchdog();
    std::fs::remove_dir_all(world.device(0).tablet.state_dir()).unwrap();
    world.sync(1).unwrap();
    world.ink(1, &notes, &page);
    world.sync(1).unwrap();
    let report = world.reattach(0).unwrap();
    assert_eq!(report.inferred.get(&notes), Some(&snapshot), "{report:?}");
    assert!(report.forks.is_empty(), "{report:?}");
    world.converge();
    assert!(world.conflicts().is_empty());
}

/// A fork recorded by two commits on `main` (its trailer repeated) is one
/// open conflict copy.
#[test]
fn an_open_conflict_recorded_twice_is_listed_once() {
    let (world, lib) = same_page_conflict();
    let report = world.sync(1).unwrap();
    let fork = report.forks[0].clone();
    world.converge();
    let git_dir = format!("--git-dir={}", world.origin);
    let main = world::git_cli(&world.root, &[&git_dir, "rev-parse", "refs/heads/main"]);
    let again = world::git_cli(
        &world.root,
        &[
            &git_dir,
            "commit-tree",
            &format!("{main}^{{tree}}"),
            "-p",
            &main,
            "-m",
            &format!("re-recorded\n\nRmu-Fork: {} {}", lib.notes, fork.copy),
        ],
    );
    world::git_cli(
        &world.root,
        &[&git_dir, "update-ref", "refs/heads/main", &again],
    );
    let status = world.status(1).unwrap();
    assert_eq!(
        status
            .open_conflicts
            .iter()
            .map(|conflict| &conflict.copy)
            .collect::<Vec<_>>(),
        vec![&fork.copy],
        "{status:?}"
    );
}

/// B inks both pages of the diary and adds one; A's apply stops after
/// every move; A's user then inks the diary's second page. Where that
/// page's write was still pending, keeping the user's page with base =
/// merge would drop B's ink on it: it must fork instead.
#[test]
fn a_pending_page_the_user_inked_is_never_kept_over_the_merge() {
    let (template, lib) = two_tablets();
    template
        .pages(1, &lib.diary)
        .iter()
        .for_each(|page| template.ink(1, &lib.diary, page));
    template.insert_page(1, &lib.diary);
    template.sync(1).unwrap();
    (0..14).for_each(|k| {
        let world = template.duplicate();
        world.tablet(0).inject(Fault::KillApplyAfter(k));
        let _ = world.sync(0);
        world.tablet(0).reconnect();
        if world.tablet(0).watchdog_armed().is_some() {
            world.tablet(0).fire_watchdog();
        }
        let page = world.pages(0, &lib.diary)[1].clone();
        world.ink(0, &lib.diary, &page);
        world
            .sync(0)
            .unwrap_or_else(|e| panic!("apply killed after {k}: {e}"));
        world.converge();
    });
}

// ---- transfer ----------------------------------------------------------------------------

/// A clone (or reattach) of tablet `i` from its computer, counting the
/// pack bytes it fetches (`GIT_TRACE_PACKFILE`).
fn fetched_by_clone(world: &World, i: usize, reattach: bool) -> u64 {
    fetched_by_clone_from(world, i, reattach, &world.origin)
}

/// [`fetched_by_clone`] from origin at `url`.
fn fetched_by_clone_from(world: &World, i: usize, reattach: bool, url: &str) -> u64 {
    static PROGRESS: libremarkable_utils::progress::NoProgress =
        libremarkable_utils::progress::NoProgress;
    static TRACES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = TRACES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let trace = world.root.join(format!("packs-{n}"));
    let _ = std::fs::remove_file(&trace);
    let hub = libremarkable_utils::hub::cycle::Hub::new(
        world.git.clone().env("GIT_TRACE_PACKFILE", &trace),
        world.device(i).cache.clone(),
        libremarkable_utils::hub::device::LockRequest::for_this_process("test-host"),
        &PROGRESS,
    )
    .with_options(world.options.clone());
    let name =
        (!reattach).then(|| libremarkable_utils::hub::model::DeviceName::new("Tablet B").unwrap());
    hub.clone_tablet(world.tablet(i), url, name, reattach)
        .unwrap();
    std::fs::metadata(&trace).map_or(0, |meta| meta.len())
}

/// An incompressible payload of `kib` KiB, so pack bytes show what moved.
fn payload(kib: usize) -> Vec<u8> {
    std::iter::successors(Some(0x2545_f491_4f6c_dd1du64), |x| {
        let x = x ^ (x << 13);
        let x = x ^ (x >> 7);
        Some(x ^ (x << 17))
    })
    .take(kib * 128)
    .flat_map(u64::to_le_bytes)
    .collect()
}

/// Another repository at the URL a cache was recorded for: the first
/// clone afterwards fetches the new library once (into the stale cache,
/// which it then takes from locally), and that cache no longer records
/// the URL, so it is not tried again.
#[test]
fn a_replaced_origin_is_fetched_once_and_its_old_cache_forgets_the_url() {
    let (world, _, _) = two_notebooks();
    let old_id = world.device(1).state().unwrap().repo_id;
    let old_cache = world.device(1).cache.join(format!("{old_id}.git"));
    let other = World::new(1);
    let paper = other.create(0, Kind::Pdf, "Paper", "");
    let library = payload(2048);
    other
        .device(0)
        .tablet
        .write_file(&format!("{paper}.pdf"), &library)
        .unwrap();
    other.clone_tablet(0).unwrap();
    std::fs::remove_dir_all(&world.origin).unwrap();
    world::copy_dir(Path::new(&other.origin), Path::new(&world.origin));
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    let size = library.len() as u64;
    let fetched = fetched_by_clone(&world, 1, true);
    eprintln!("first clone after the replacement: {fetched} pack bytes for {size}");
    assert!(
        fetched >= size && fetched < size + size / 2,
        "{fetched} for {size}"
    );
    let recorded = std::fs::read_to_string(old_cache.join("rmu-origin")).unwrap_or_default();
    assert!(
        !recorded.lines().any(|url| url == world.origin),
        "{recorded:?}"
    );
    let refs = world::git_cli(
        &world.root,
        &[
            &format!("--git-dir={}", old_cache.display()),
            "for-each-ref",
        ],
    );
    assert!(!refs.contains("refs/rmu/bootstrap"), "{refs}");
    // The new cache stands alone: no alternates, and it syncs with the
    // stale cache gone.
    let new_id = world.device(1).state().unwrap().repo_id;
    let new_cache = world.device(1).cache.join(format!("{new_id}.git"));
    assert!(!new_cache.join("objects/info/alternates").exists());
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    assert_eq!(fetched_by_clone(&world, 1, true), 0);
    std::fs::remove_dir_all(&old_cache).unwrap();
    world.sync(1).unwrap();
    world::git_cli(
        &world.root,
        &[
            &format!("--git-dir={}", new_cache.display()),
            "fsck",
            "--no-dangling",
        ],
    );
}

/// Two caches on one computer record the same URL (one from a clone, one
/// from a sync that reached its own origin through that URL), and then a
/// third repository answers there: the first stale cache fetches it, the
/// second only forgets the URL, so the library crosses the network once.
#[test]
fn several_stale_caches_fetch_the_new_library_once() {
    let (world, _, _) = two_notebooks();
    let url = world.origin.clone();
    let x_id = world.device(1).state().unwrap().repo_id;
    let x_cache = world.device(1).cache.join(format!("{x_id}.git"));
    let x_main = world::git_cli(
        &world.root,
        &[
            &format!("--git-dir={}", x_cache.display()),
            "rev-parse",
            "refs/remotes/origin/main",
        ],
    );
    // Repository Y answers at the URL; a tablet attached to Y syncs from
    // tablet B's computer through it, so B's cache directory records the
    // URL for Y's cache too.
    let y = World::new(1);
    y.create(0, Kind::Notebook { pages: 1 }, "Why", "");
    y.clone_tablet(0).unwrap();
    std::fs::remove_dir_all(&url).unwrap();
    world::copy_dir(Path::new(&y.origin), Path::new(&url));
    static PROGRESS: libremarkable_utils::progress::NoProgress =
        libremarkable_utils::progress::NoProgress;
    libremarkable_utils::hub::cycle::Hub::new(
        world.git.clone(),
        world.device(1).cache.clone(),
        libremarkable_utils::hub::device::LockRequest::for_this_process("test-host"),
        &PROGRESS,
    )
    .with_options(HubOptions {
        origin: Some(url.clone()),
        ..world.options.clone()
    })
    .sync(y.tablet(0))
    .unwrap();
    let y_id = y.device(0).state().unwrap().repo_id;
    let y_cache = world.device(1).cache.join(format!("{y_id}.git"));
    // Repository Z replaces it.
    let z = World::new(1);
    let paper = z.create(0, Kind::Pdf, "Paper", "");
    let library = payload(2048);
    z.device(0)
        .tablet
        .write_file(&format!("{paper}.pdf"), &library)
        .unwrap();
    z.clone_tablet(0).unwrap();
    std::fs::remove_dir_all(&url).unwrap();
    world::copy_dir(Path::new(&z.origin), Path::new(&url));
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    let size = library.len() as u64;
    let fetched = fetched_by_clone(&world, 1, true);
    assert!(
        fetched >= size && fetched < size + size / 2,
        "{fetched} for {size}"
    );
    [&x_cache, &y_cache].into_iter().for_each(|cache| {
        let recorded = std::fs::read_to_string(cache.join("rmu-origin")).unwrap_or_default();
        assert!(!recorded.lines().any(|known| known == url), "{recorded:?}");
    });
    assert_eq!(
        world::git_cli(
            &world.root,
            &[
                &format!("--git-dir={}", x_cache.display()),
                "rev-parse",
                "refs/remotes/origin/main"
            ],
        ),
        x_main
    );
}

/// A mistyped `remote set-url` to an unrelated repository of the other
/// object format is refused, and the tablet's cache (whose `main` may be
/// the last copy of its history) is kept as it was.
#[test]
fn set_url_to_another_format_keeps_the_cache() {
    let (world, _, _) = two_notebooks();
    let id = world.device(0).state().unwrap().repo_id;
    let cache = world.device(0).cache.join(format!("{id}.git"));
    let git_dir = format!("--git-dir={}", cache.display());
    let main = world::git_cli(
        &world.root,
        &[&git_dir, "rev-parse", "refs/remotes/origin/main"],
    );
    let other = world.root.join("other");
    let other_path = other.to_str().unwrap().to_string();
    world::git_cli(
        &world.root,
        &[
            "init",
            "-q",
            "--object-format=sha256",
            "-b",
            "main",
            &other_path,
        ],
    );
    std::fs::write(other.join("file"), b"unrelated").unwrap();
    world::git_cli(&world.root, &["-C", &other_path, "add", "file"]);
    world::git_cli(
        &world.root,
        &["-C", &other_path, "commit", "-q", "-m", "unrelated"],
    );
    let result = hub_without_origin(&world, 0).set_remote_url(world.tablet(0), &other_path);
    assert!(
        matches!(&result, Err(e) if is_cycle(e, |e| matches!(e, CycleError::CacheFormatMismatch { .. }))),
        "{result:?}"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains(&cache.display().to_string())
    );
    assert_eq!(
        world::git_cli(
            &world.root,
            &[&git_dir, "rev-parse", "refs/remotes/origin/main"]
        ),
        main
    );
    world.check_idle(0);
    world.sync(0).unwrap();
}

/// One origin reached through two URLs: each is recorded, so going back
/// to the first after a clone through the second fetches nothing again.
#[test]
fn a_cache_remembers_several_origin_urls() {
    let world = World::new(2);
    let paper = world.create(0, Kind::Pdf, "Paper", "");
    let library = payload(1024);
    world
        .device(0)
        .tablet
        .write_file(&format!("{paper}.pdf"), &library)
        .unwrap();
    world.clone_tablet(0).unwrap();
    let alias = world.root.join("alias.git");
    std::os::unix::fs::symlink(&world.origin, &alias).unwrap();
    let alias = alias.to_str().unwrap().to_string();
    let size = library.len() as u64;
    assert!(fetched_by_clone(&world, 1, false) >= size);
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    // Through the alias: a URL this cache has not seen, fetched once.
    assert!(fetched_by_clone_from(&world, 1, true, &alias) >= size);
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    let back = fetched_by_clone_from(&world, 1, true, &world.origin);
    assert!(back < size / 10, "{back} for {size}");
}

/// The library crosses the network once: a first clone on a computer with
/// no cache fetches it once (not into a probe and then again into the
/// cache), and a reattach to an up-to-date cache fetches next to nothing.
#[test]
fn a_clone_fetches_the_library_once() {
    let world = World::new(2);
    let paper = world.create(0, Kind::Pdf, "Paper", "");
    let payload = payload(2048);
    world
        .device(0)
        .tablet
        .write_file(&format!("{paper}.pdf"), &payload)
        .unwrap();
    world.clone_tablet(0).unwrap();
    let size = payload.len() as u64;
    let first = fetched_by_clone(&world, 1, false);
    eprintln!("first clone, no cache: {first} pack bytes fetched for a {size}-byte library");
    assert!(
        first >= size && first < size + size / 2,
        "{first} for {size}"
    );
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    let again = fetched_by_clone(&world, 1, true);
    eprintln!("reattach, cache up to date: {again} pack bytes fetched");
    assert!(again < size / 10, "{again} for {size}");
    world.converge();
}

/// Another repository now answers at the URL a cache was recorded for: a
/// reattach does not take that cache (whose repo id no longer matches),
/// leaves it as it was, and attaches to the new repository.
#[test]
fn a_clone_from_a_replaced_origin_ignores_the_old_cache() {
    let (world, _, _) = two_notebooks();
    let old_id = world.device(1).state().unwrap().repo_id;
    let old_cache = world.device(1).cache.join(format!("{old_id}.git"));
    let git_dir = format!("--git-dir={}", old_cache.display());
    let old_main = world::git_cli(
        &world.root,
        &[&git_dir, "rev-parse", "refs/remotes/origin/main"],
    );
    let other = World::new(1);
    other.create(0, Kind::Notebook { pages: 1 }, "Elsewhere", "");
    other.clone_tablet(0).unwrap();
    std::fs::remove_dir_all(&world.origin).unwrap();
    world::copy_dir(Path::new(&other.origin), Path::new(&world.origin));
    std::fs::remove_dir_all(world.device(1).tablet.state_dir()).unwrap();
    let report = world.reattach(1).unwrap();
    let new_id = world.device(1).state().unwrap().repo_id;
    assert_eq!(
        new_id,
        other.device(0).state().unwrap().repo_id,
        "{report:?}"
    );
    assert_eq!(
        world::git_cli(
            &world.root,
            &[&git_dir, "rev-parse", "refs/remotes/origin/main"]
        ),
        old_main
    );
}

/// A `.keep` a killed `fast-import` left in the cache is removed the next
/// time the cache is opened (git never repacks a kept pack).
#[test]
fn a_killed_fast_imports_keep_files_are_removed_on_open() {
    let (world, _, _) = two_notebooks();
    let id = world.device(0).state().unwrap().repo_id;
    let pack_dir = world.device(0).cache.join(format!("{id}.git/objects/pack"));
    std::fs::create_dir_all(&pack_dir).unwrap();
    std::fs::write(pack_dir.join("pack-killed.keep"), b"fast-import\n").unwrap();
    world.sync(0).unwrap();
    assert!(!pack_dir.join("pack-killed.keep").exists());
}
