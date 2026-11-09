//! End-to-end tests of the `rmu hub` sync cycle (`hub::cycle`): two or
//! three fake tablets, a bare origin and one local cache per tablet in a
//! scratch directory. No network, no real tablet, hermetic git (see
//! `hub_e2e/world.rs`). Every scenario ends with the same oracles
//! (`World::check`): the tablets converge on origin, no ink is lost,
//! xochitl runs, no lock or journal is left.
#![cfg(unix)]

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
    let (world, lib) = two_tablets();
    let page = world.pages(0, &lib.notes)[1].clone();
    world.ink(0, &lib.notes, &page);
    world.rename(1, &lib.archive, "Attic");
    // Right before A writes its journal, B syncs: A's push is rejected
    // and A goes back to fetch and merge.
    let journal = op_index(&world, 0, OpKind::WriteState, 2, |w| {
        w.sync(0).unwrap();
    });
    let (git, origin, options) = (
        world.git.clone(),
        world.origin.clone(),
        world.options.clone(),
    );
    let (other, cache) = (
        Arc::clone(&world.device(1).tablet),
        world.device(1).cache.clone(),
    );
    world.tablet(0).inject(Fault::EditBefore(
        journal,
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
                origin: Some(origin),
                ..options
            });
            let report = hub.sync(&*other).unwrap();
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
/// notebook, a deletion, ink on a page A also inks), A has its own
/// (that ink, an inserted page, a rename, a new folder with a move, a
/// trashed document). A's sync therefore reads, merges with a fork,
/// pushes, stages, applies writes and deletions, and records.
fn crash_template() -> World {
    let (world, lib) = two_tablets();
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
    world.sync(1).unwrap();
    world.ink(0, &lib.notes, &page);
    world.insert_page(0, &lib.sketch);
    world.rename(0, &lib.archive, "Attic");
    let inbox = world.create(0, Kind::Folder, "Inbox", "");
    world.move_to(0, &lib.sketch, inbox.as_str());
    world.trash(0, &lib.paper);
    world
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
    world.converge();
    Reference {
        ops,
        mutations,
        conflicts: world.conflicts(),
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
    world.converge();
    assert_eq!(
        world.conflicts(),
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

#[test]
fn download_cut_after_every_file() {
    let template = crash_template();
    let reference = reference(&template);
    (0..12).for_each(|k| {
        crash_with(
            &template,
            &reference,
            Fault::FailDownloadAfter(k),
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
        match rng.below(12) {
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
            _ => {
                world
                    .sync(i)
                    .unwrap_or_else(|e| panic!("seed {seed} step {step}: sync {i}: {e}"));
            }
        }
    });
    world.converge();
    if std::env::var_os("RMU_E2E_DEBUG").is_some() {
        eprintln!(
            "seed {seed}: {} documents, {} conflict copies, {} ink marks",
            world.origin_docs().len(),
            world.conflicts().len(),
            world.inks.lock().unwrap().expected.len()
        );
    }
}

#[test]
fn interleaved_edits_and_syncs_across_three_tablets() {
    [1, 7, 42, 1234, 99_991].into_iter().for_each(interleaving);
}
