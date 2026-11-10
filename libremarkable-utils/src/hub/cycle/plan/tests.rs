//! Tests of the cycle's pure decisions, over fabricated documents whose
//! blobs live in an in-memory map.

use super::*;
use crate::hub::model::PageId;
use serde_json::json;
use sha2::{Digest, Sha256};

fn doc(n: u32) -> DocId {
    DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
}

fn page(n: u32) -> DocFile {
    DocFile::Page(PageId::new(format!("p{n}")).unwrap())
}

fn oid(n: u32) -> Oid {
    format!("{n:040x}").parse().unwrap()
}

fn rules() -> &'static MirrorRules {
    MirrorRules::latest()
}

/// Blobs: JSON by content, other files by a fabricated id.
#[derive(Default)]
struct Blobs(HashMap<Oid, Value>);

impl Blobs {
    fn json(&mut self, value: Value) -> Oid {
        let id: Oid = format!("{:x}", Sha256::digest(value.to_string()))[..40]
            .parse()
            .unwrap();
        self.0.insert(id.clone(), value);
        id
    }
}

fn metadata(name: &str, parent: &str, kind: &str) -> Value {
    json!({
        "lastModified": "1",
        "lastOpened": "0",
        "parent": parent,
        "type": kind,
        "visibleName": name,
    })
}

fn content(pages: &[&str]) -> Value {
    json!({
        "formatVersion": 2,
        "fileType": "notebook",
        "cPages": { "pages": pages.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>() },
        "zoomMode": "bestFit",
        "customZoomScale": 1,
    })
}

fn version(files: Vec<(DocFile, Oid)>) -> DocVersion {
    DocVersion {
        files: files
            .into_iter()
            .map(|(file, id)| (file, ListingEntry::regular(id)))
            .collect(),
        other: BTreeMap::new(),
    }
}

/// A notebook version: metadata, content, and pages by blob.
fn notebook(blobs: &mut Blobs, name: &str, parent: &str, pages: &[(u32, u32)]) -> DocVersion {
    let ids: Vec<String> = pages.iter().map(|(n, _)| format!("p{n}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut files = vec![
        (
            DocFile::Metadata,
            blobs.json(metadata(name, parent, "DocumentType")),
        ),
        (DocFile::Content, blobs.json(content(&ids))),
    ];
    files.extend(pages.iter().map(|(n, ink)| (page(*n), oid(*ink))));
    version(files)
}

fn with_json(version: &DocVersion, blobs: &mut Blobs, file: DocFile, value: Value) -> DocVersion {
    let mut changed = version.clone();
    changed
        .files
        .insert(file, ListingEntry::regular(blobs.json(value)));
    changed
}

#[test]
fn view_state_and_derived_fields_never_make_versions_differ() {
    let mut blobs = Blobs::default();
    let a = notebook(&mut blobs, "Notes", "", &[(1, 10)]);
    let mut read = metadata("Notes", "", "DocumentType");
    read["lastOpened"] = json!("99");
    read["lastModified"] = json!("5");
    let b = with_json(&a, &mut blobs, DocFile::Metadata, read);
    assert!(versions_equivalent(rules(), Some(&a), Some(&b), &blobs.0).unwrap());
    let renamed = with_json(
        &a,
        &mut blobs,
        DocFile::Metadata,
        metadata("Renamed", "", "DocumentType"),
    );
    assert!(!versions_equivalent(rules(), Some(&a), Some(&renamed), &blobs.0).unwrap());
    let inked = notebook(&mut blobs, "Notes", "", &[(1, 11)]);
    assert!(!versions_equivalent(rules(), Some(&a), Some(&inked), &blobs.0).unwrap());
    // Absent, and present without metadata, are the same; present is not.
    let leftovers = version(vec![(page(1), oid(10))]);
    assert!(versions_equivalent(rules(), None, Some(&leftovers), &blobs.0).unwrap());
    assert!(!versions_equivalent(rules(), None, Some(&a), &blobs.0).unwrap());
    // Unparsable JSON equals only the same blob.
    let broken = version(vec![(DocFile::Metadata, oid(1))]);
    let other = version(vec![(DocFile::Metadata, oid(2))]);
    assert!(versions_equivalent(rules(), Some(&broken), Some(&broken), &blobs.0).unwrap());
    assert!(!versions_equivalent(rules(), Some(&broken), Some(&other), &blobs.0).unwrap());
}

#[test]
fn plans_write_only_what_differs_and_keep_the_tablets_view_state() {
    let mut blobs = Blobs::default();
    let tablet = notebook(&mut blobs, "Notes", "", &[(1, 10), (2, 20)]);
    let local: LocalJson = [(JsonFile::Content, {
        let mut value = content(&["p1", "p2"]);
        value["zoomMode"] = json!("fitWidth");
        value
    })]
    .into();
    // Equivalent: nothing to do.
    let same = with_json(&tablet, &mut blobs, DocFile::Content, {
        let mut value = content(&["p1", "p2"]);
        value["zoomMode"] = json!("other");
        value
    });
    assert_eq!(
        plan_doc(
            rules(),
            &doc(1),
            Some(&same),
            Some(&tablet),
            &local,
            &blobs.0
        )
        .unwrap(),
        None
    );
    // A new page and a removed one: the page files, and the content with
    // the tablet's zoom mode overlaid (bytes, not origin's blob).
    let mut target = notebook(&mut blobs, "Notes", "", &[(1, 10), (3, 30)]);
    let planned = plan_doc(
        rules(),
        &doc(1),
        Some(&target),
        Some(&tablet),
        &local,
        &blobs.0,
    )
    .unwrap()
    .unwrap();
    let Change::Write { writes, removes } = &planned.change else {
        panic!("{planned:?}")
    };
    assert_eq!(removes, &vec![page(2)]);
    let files: Vec<&DocFile> = writes.iter().map(|write| &write.file).collect();
    assert_eq!(files, vec![&DocFile::Content, &page(3)]);
    let Content::Bytes(bytes) = &writes[0].content else {
        panic!("{:?}", writes[0])
    };
    let written: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(written["zoomMode"], "fitWidth");
    let ids: Vec<&str> = written["cPages"]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| page["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["p1", "p3"]);
    assert_eq!(writes[1].content, Content::Blob(oid(30)));
    // A document new to the tablet loses model-specific zoom fields.
    let planned = plan_doc(rules(), &doc(1), Some(&target), None, &local, &blobs.0)
        .unwrap()
        .unwrap();
    let Change::Write { writes, removes } = &planned.change else {
        panic!("{planned:?}")
    };
    assert!(removes.is_empty());
    let content_write = writes
        .iter()
        .find(|write| write.file == DocFile::Content)
        .unwrap();
    let Content::Bytes(bytes) = &content_write.content else {
        panic!("{content_write:?}")
    };
    let written: Value = serde_json::from_slice(bytes).unwrap();
    assert!(written.get("customZoomScale").is_none());
    assert_eq!(written["zoomMode"], "bestFit");
    // Leftovers without metadata: removed once the document arrives,
    // left alone when it does not.
    let leftovers = version(vec![(page(9), oid(90))]);
    let planned = plan_doc(
        rules(),
        &doc(1),
        Some(&target),
        Some(&leftovers),
        &local,
        &blobs.0,
    )
    .unwrap()
    .unwrap();
    assert!(matches!(&planned.change, Change::Write { removes, .. } if removes == &vec![page(9)]));
    assert_eq!(
        plan_doc(rules(), &doc(1), None, Some(&leftovers), &local, &blobs.0).unwrap(),
        None
    );
    // Absent from the merge: deleted.
    assert_eq!(
        plan_doc(rules(), &doc(1), None, Some(&tablet), &local, &blobs.0)
            .unwrap()
            .unwrap()
            .change,
        Change::Delete
    );
    // An executable payload keeps its mode.
    target.files.insert(
        DocFile::PayloadPdf,
        ListingEntry {
            mode: FileMode::Executable,
            id: oid(5),
        },
    );
    let planned = plan_doc(
        rules(),
        &doc(1),
        Some(&target),
        Some(&tablet),
        &local,
        &blobs.0,
    )
    .unwrap()
    .unwrap();
    let Change::Write { writes, .. } = &planned.change else {
        panic!()
    };
    assert!(
        writes
            .iter()
            .any(|w| w.file == DocFile::PayloadPdf && w.mode == FileMode::Executable)
    );
}

#[test]
fn files_without_metadata_are_overwritten_only_if_they_are_rmus_leftovers() {
    let mut blobs = Blobs::default();
    let target = notebook(&mut blobs, "A", "", &[(1, 10), (2, 20)]);
    let merged = RepoTree {
        docs: [(doc(1), target)].into(),
        extra: BTreeMap::new(),
    };
    let local = BTreeMap::new();
    let planned = |leftovers: DocVersion| {
        let tablet: BTreeMap<DocId, DocVersion> = [(doc(1), leftovers)].into();
        plan(&PlanInput {
            rules: rules(),
            merged: &merged,
            tablet: &tablet,
            protect: &BTreeSet::new(),
            local: &local,
            json: &blobs.0,
        })
        .unwrap()
    };
    // An interrupted write of this very version: finished.
    let output = planned(version(vec![(page(1), oid(10))]));
    assert_eq!(output.planned.len(), 1);
    assert!(output.left_alone.is_empty());
    // Ink of its own (a page origin lacks, or another page blob): a
    // document that lost its `.metadata`, never overwritten.
    [
        version(vec![(page(1), oid(11))]),
        version(vec![(page(3), oid(30))]),
    ]
    .into_iter()
    .for_each(|leftovers| {
        let output = planned(leftovers);
        assert!(output.planned.is_empty());
        assert_eq!(output.left_alone, [doc(1)].into());
    });
}

#[test]
fn plan_skips_protected_documents() {
    let mut blobs = Blobs::default();
    let a = notebook(&mut blobs, "A", "", &[(1, 1)]);
    let merged = RepoTree {
        docs: [(doc(2), a.clone())].into(),
        extra: BTreeMap::new(),
    };
    let tablet: BTreeMap<DocId, DocVersion> = [(doc(1), a)].into();
    let local = BTreeMap::new();
    let input = |protect: &BTreeSet<DocId>| {
        plan(&PlanInput {
            rules: rules(),
            merged: &merged,
            tablet: &tablet,
            protect,
            local: &local,
            json: &blobs.0,
        })
        .unwrap()
        .planned
    };
    let all = input(&BTreeSet::new());
    assert_eq!(
        all.iter().map(|p| p.doc.clone()).collect::<Vec<_>>(),
        [doc(1), doc(2)]
    );
    assert_eq!(all[0].change, Change::Delete);
    let protected = input(&[doc(1)].into());
    assert_eq!(protected.len(), 1);
    assert_eq!(protected[0].doc, doc(2));
}

fn item(parent: &str, folder: bool) -> TreeItem {
    TreeItem {
        parent: parent.to_string(),
        kind: Some(if folder {
            crate::xochitl::ItemKind::Folder
        } else {
            crate::xochitl::ItemKind::Document
        }),
    }
}

fn write(n: u32) -> Planned {
    Planned {
        doc: doc(n),
        change: Change::Write {
            writes: Vec::new(),
            removes: Vec::new(),
        },
    }
}

fn delete(n: u32) -> Planned {
    Planned {
        doc: doc(n),
        change: Change::Delete,
    }
}

#[test]
fn apply_order_is_parents_first_then_deletions_children_first() {
    // 1 (folder) > 2 (folder) > 3; copy 4 at the root; 5 > 6 deleted.
    let merged: BTreeMap<DocId, TreeItem> = [
        (doc(1), item("", true)),
        (doc(2), item(doc(1).as_str(), true)),
        (doc(3), item(doc(2).as_str(), false)),
        (doc(4), item("", false)),
        (doc(7), item("", false)),
    ]
    .into();
    let current: BTreeMap<DocId, TreeItem> = [
        (doc(5), item("", true)),
        (doc(6), item(doc(5).as_str(), false)),
    ]
    .into();
    let ordered = order(
        vec![
            delete(5),
            write(3),
            delete(6),
            write(2),
            write(7),
            write(4),
            write(1),
        ],
        &merged,
        &current,
        &[doc(4)].into(),
    );
    let ids: Vec<DocId> = ordered.into_iter().map(|p| p.doc).collect();
    assert_eq!(
        ids,
        [doc(4), doc(1), doc(7), doc(2), doc(3), doc(6), doc(5)]
    );
}

#[test]
fn verification_ignores_reading_but_not_editing() {
    let mut blobs = Blobs::default();
    let read_metadata = blobs.json(metadata("Notes", "", "DocumentType"));
    let before: BTreeMap<DocFile, Oid> =
        [(DocFile::Metadata, read_metadata), (page(1), oid(10))].into();
    let files: BTreeSet<DocFile> = before.keys().cloned().collect();
    let sha: HashMap<Oid, String> = [(oid(10), "abc".to_string())].into();
    let check = |files: &BTreeSet<DocFile>, moved: BTreeMap<DocFile, Reread>| {
        verified(rules(), &before, files, &moved, &blobs.0, &sha).unwrap()
    };
    assert!(check(&files, BTreeMap::new()));
    let mut opened = metadata("Notes", "", "DocumentType");
    opened["lastOpened"] = json!("123");
    assert!(check(
        &files,
        [(DocFile::Metadata, Reread::Json(Some(opened)))].into()
    ));
    assert!(!check(
        &files,
        [(
            DocFile::Metadata,
            Reread::Json(Some(metadata("Renamed", "", "DocumentType")))
        )]
        .into()
    ));
    assert!(!check(
        &files,
        [(DocFile::Metadata, Reread::Json(None))].into()
    ));
    assert!(check(
        &files,
        [(page(1), Reread::Sha256("abc".into()))].into()
    ));
    assert!(!check(
        &files,
        [(page(1), Reread::Sha256("def".into()))].into()
    ));
    // A page added or removed since the read.
    let mut more = files.clone();
    more.insert(page(2));
    assert!(!check(&more, BTreeMap::new()));
    assert!(!check(&[DocFile::Metadata].into(), BTreeMap::new()));
}

#[test]
fn tree_checks_skip_only_the_changes_that_break_the_live_tree() {
    // Folder 1 holds 2; the user put a new document 3 into folder 4,
    // which the merge deletes; the merge also moves 2 under 5.
    let current: BTreeMap<DocId, TreeItem> = [
        (doc(1), item("", true)),
        (doc(2), item(doc(1).as_str(), false)),
        (doc(3), item(doc(4).as_str(), false)),
        (doc(4), item("", true)),
        (doc(5), item("", true)),
        // Already broken on the tablet: never blamed on the plan.
        (doc(9), item(doc(8).as_str(), false)),
    ]
    .into();
    let planned: BTreeMap<DocId, Option<TreeItem>> =
        [(doc(4), None), (doc(2), Some(item(doc(5).as_str(), false)))].into();
    assert_eq!(
        tree_skips(&current, &planned, &Partners::new()),
        [doc(4)].into()
    );
    // A move that would close a cycle, and its fork partner with it.
    let cycle: BTreeMap<DocId, Option<TreeItem>> = [
        (doc(1), Some(item(doc(5).as_str(), true))),
        (doc(5), Some(item(doc(1).as_str(), true))),
        (doc(2), Some(item("", false))),
    ]
    .into();
    let partners = super::partners([(doc(5), doc(2))]);
    assert_eq!(
        tree_skips(&current, &cycle, &partners),
        [doc(1), doc(2), doc(5)].into()
    );
    assert!(tree_skips(&current, &BTreeMap::new(), &partners).is_empty());
}

#[test]
fn moving_a_touched_document_into_the_trash_is_skipped() {
    let current: BTreeMap<DocId, TreeItem> = [
        (doc(1), item("", true)),
        (doc(2), item(doc(1).as_str(), false)),
        (doc(3), item("", false)),
        (doc(4), item("", true)),
    ]
    .into();
    // Origin trashed folder 1 and document 3, and moved folder 4 under 1.
    let merged: BTreeMap<DocId, TreeItem> = [
        (doc(1), item("trash", true)),
        (doc(3), item("trash", false)),
        (doc(4), item(doc(1).as_str(), true)),
    ]
    .into();
    let planned = vec![write(1), write(3), write(4)];
    let blobs = Blobs::default();
    let check = |touched: BTreeSet<DocId>| {
        skipped(&Checks {
            rules: rules(),
            planned: &planned,
            before: &BTreeMap::new(),
            files_now: &BTreeMap::new(),
            moved: &BTreeMap::new(),
            sha256: &HashMap::new(),
            json: &blobs.0,
            current: &current,
            merged: &merged,
            touched: &touched,
            partners: &Partners::new(),
        })
        .unwrap()
    };
    // The user wrote in 2, inside folder 1, after the snapshot.
    assert_eq!(check([doc(2)].into()), [doc(1)].into());
    assert!(check(BTreeSet::new()).is_empty());
    // Written in inside folder 4: moving 4 under the trashed 1 would take
    // it along too, and so would trashing 1.
    let mut nested = current.clone();
    nested.insert(doc(5), item(doc(4).as_str(), false));
    let skipped_nested = skipped(&Checks {
        rules: rules(),
        planned: &planned,
        before: &BTreeMap::new(),
        files_now: &BTreeMap::new(),
        moved: &BTreeMap::new(),
        sha256: &HashMap::new(),
        json: &blobs.0,
        current: &nested,
        merged: &merged,
        touched: &[doc(5)].into(),
        partners: &Partners::new(),
    })
    .unwrap();
    assert!(skipped_nested.contains(&doc(4)), "{skipped_nested:?}");
}

#[test]
fn partners_are_transitive() {
    let partners = partners([(doc(1), doc(2)), (doc(2), doc(3))]);
    assert_eq!(
        with_partners([doc(3)].into(), &partners),
        [doc(1), doc(2), doc(3)].into()
    );
    assert_eq!(with_partners([doc(9)].into(), &partners), [doc(9)].into());
}

#[test]
fn reconciliation_verdicts() {
    let mut blobs = Blobs::default();
    let snapshot = notebook(&mut blobs, "Notes", "", &[(1, 10)]);
    let merge = notebook(&mut blobs, "Notes", "", &[(1, 10), (2, 20)]);
    let files = |version: &DocVersion| -> BTreeMap<DocFile, Oid> {
        version
            .files
            .iter()
            .map(|(file, entry)| (file.clone(), entry.id.clone()))
            .collect()
    };
    let judge = |tablet: &BTreeMap<DocFile, Oid>| {
        verdict(rules(), Some(&merge), Some(&snapshot), tablet, &blobs.0).unwrap()
    };
    assert_eq!(judge(&files(&merge)), Verdict::AtMerge);
    assert_eq!(judge(&files(&snapshot)), Verdict::AtSnapshot);
    // The new page moved in, the content not yet.
    let mut half = files(&snapshot);
    half.insert(page(2), oid(20));
    assert_eq!(judge(&half), Verdict::Mixed);
    // The user inked page 1 since.
    let mut edited = files(&merge);
    edited.insert(page(1), oid(11));
    assert_eq!(judge(&edited), Verdict::Other);
    // Read only (view state): still at the merge.
    let mut opened = files(&merge);
    let mut read = metadata("Notes", "", "DocumentType");
    read["lastOpened"] = json!("7");
    opened.insert(DocFile::Metadata, blobs.json(read));
    let judge = |tablet: &BTreeMap<DocFile, Oid>| {
        verdict(rules(), Some(&merge), Some(&snapshot), tablet, &blobs.0).unwrap()
    };
    assert_eq!(judge(&opened), Verdict::AtMerge);
    // A deletion interrupted after `.metadata`: a mix.
    let leftover: BTreeMap<DocFile, Oid> = files(&snapshot)
        .into_iter()
        .filter(|(file, _)| *file != DocFile::Metadata)
        .collect();
    assert_eq!(
        verdict(rules(), None, Some(&snapshot), &leftover, &blobs.0).unwrap(),
        Verdict::Mixed
    );
}

#[test]
fn an_apply_finished_a_document_once_nothing_it_needs_is_still_staged() {
    let mut blobs = Blobs::default();
    let snapshot = notebook(&mut blobs, "Notes", "", &[(1, 10), (3, 30)]);
    let merge = notebook(&mut blobs, "Notes", "", &[(1, 10), (2, 20)]);
    let mut tablet: BTreeMap<DocFile, Oid> = merge
        .files
        .iter()
        .map(|(file, entry)| (file.clone(), entry.id.clone()))
        .collect();
    tablet.insert(page(1), oid(11));
    let finished = |pending: &BTreeSet<String>, tablet: &BTreeMap<DocFile, Oid>| {
        apply_finished(
            rules(),
            &doc(1),
            Some(&merge),
            Some(&snapshot),
            tablet,
            pending,
            &blobs.0,
        )
        .unwrap()
    };
    assert!(finished(&BTreeSet::new(), &tablet));
    // The new page is still staged: not finished.
    let pending: BTreeSet<String> = [page(2).xochitl_path(&doc(1))].into();
    assert!(!finished(&pending, &tablet));
    // An unchanged file being staged does not matter.
    let pending: BTreeSet<String> = [page(1).xochitl_path(&doc(1))].into();
    assert!(finished(&pending, &tablet));
    // Page 3 was to be removed and is still there.
    let mut kept = tablet.clone();
    kept.insert(page(3), oid(30));
    assert!(!finished(&BTreeSet::new(), &kept));
}

#[test]
fn the_manifest_leaves_out_racy_moved_removed_and_unknown_entries() {
    let second = 1_000_000_000i64;
    let stat = |mtime_ns: i64, inode: u64| StatSignature {
        size: 1,
        mtime_ns,
        ctime_ns: mtime_ns,
        inode,
    };
    let path = |name: &str| XochitlPath::new(name).unwrap();
    let read: BTreeMap<XochitlPath, (StatSignature, Oid)> = [
        (path("fine"), (stat(5 * second + 7, 1), oid(1))),
        (path("coarse-old"), (stat(4 * second, 2), oid(2))),
        (path("coarse-racy"), (stat(10 * second, 3), oid(3))),
        (path("moved"), (stat(5 * second + 1, 4), oid(4))),
        (path("removed"), (stat(5 * second + 2, 5), oid(5))),
        (path("rewritten"), (stat(5 * second + 3, 6), oid(6))),
    ]
    .into();
    let before: BTreeMap<XochitlPath, StatSignature> = read
        .iter()
        .map(|(path, (stat, _))| (path.clone(), *stat))
        .filter(|(path, _)| path.as_str() != "moved")
        .collect();
    let written: BTreeMap<XochitlPath, (StatSignature, Option<Oid>)> = [
        (path("rewritten"), (stat(20 * second + 1, 7), Some(oid(7)))),
        (path("overlaid"), (stat(20 * second + 2, 8), None)),
        (path("racy-write"), (stat(30 * second, 9), Some(oid(9)))),
    ]
    .into();
    let manifest = manifest(&ManifestInput {
        read: &read,
        read_reference_ns: 10 * second + 500,
        before_apply: Some(&before),
        written: &written,
        removed: &[path("removed")].into(),
        written_reference_ns: 30 * second + 5,
    });
    let kept: BTreeMap<&str, &Oid> = manifest
        .iter()
        .map(|(path, entry)| (path.as_str(), &entry.blob))
        .collect();
    assert_eq!(
        kept,
        BTreeMap::from([
            ("coarse-old", &oid(2)),
            ("fine", &oid(1)),
            ("rewritten", &oid(7)),
        ])
    );
}

#[test]
fn staged_runs_read_complete_sentinels_only() {
    let merge = oid(7);
    let sentinel = sentinel_files(&merge, [doc(1), doc(2)]);
    assert_eq!(
        sentinel.last().unwrap(),
        &format!("{STAGED_SENTINEL}{merge}/done")
    );
    let listing: Vec<String> = sentinel
        .iter()
        .map(|path| format!("run1/{path}"))
        .chain([
            format!("run1/{}", page(1).xochitl_path(&doc(1))),
            // Incomplete: no `done`.
            format!("run2/{STAGED_SENTINEL}{merge}/{}", doc(3)),
            "run2/x.metadata".to_string(),
            // Another merge's sentinel.
            format!("run3/{STAGED_SENTINEL}{}/done", oid(8)),
            format!("run3/{STAGED_SENTINEL}{}/{}", oid(8), doc(4)),
            "run3/y.metadata".to_string(),
        ])
        .collect();
    let runs = staged_runs(listing.iter().map(String::as_str), &merge);
    assert_eq!(
        runs,
        vec![StagedRun {
            planned: [doc(1), doc(2)].into(),
            pending: [page(1).xochitl_path(&doc(1))].into(),
        }]
    );
    assert!(staged_runs(listing.iter().map(String::as_str), &oid(9)).is_empty());
}

#[test]
fn reconciliation_only_finishes_or_trusts_documents_a_run_planned() {
    let mut blobs = Blobs::default();
    let snapshot_doc = notebook(&mut blobs, "Notes", "", &[(1, 10)]);
    let merge_doc = notebook(&mut blobs, "Notes", "", &[(1, 10), (2, 20)]);
    let merge = RepoTree {
        docs: [(doc(1), merge_doc.clone())].into(),
        extra: BTreeMap::new(),
    };
    let snapshot = RepoTree {
        docs: [(doc(1), snapshot_doc.clone())].into(),
        extra: BTreeMap::new(),
    };
    let files = |version: &DocVersion| -> BTreeMap<DocFile, Oid> {
        version
            .files
            .iter()
            .map(|(file, entry)| (file.clone(), entry.id.clone()))
            .collect()
    };
    // Half applied (the new page moved in, the content not).
    let mut half = files(&snapshot_doc);
    half.insert(page(2), oid(20));
    // Applied, then inked.
    let mut edited = files(&merge_doc);
    edited.insert(page(1), oid(11));
    let judge = |tablet_files: BTreeMap<DocFile, Oid>, staged: &[StagedRun]| {
        let tablet = [(doc(1), tablet_files)].into();
        verdicts(&Journaled {
            rules: rules(),
            merge: &merge,
            snapshot: &snapshot,
            tablet: &tablet,
            staged,
            forks: &[],
            json: &blobs.0,
        })
        .unwrap()[&doc(1)]
    };
    let planned = [StagedRun {
        planned: [doc(1)].into(),
        pending: BTreeSet::new(),
    }];
    let unplanned = [StagedRun {
        planned: [doc(2)].into(),
        pending: BTreeSet::new(),
    }];
    assert_eq!(judge(half.clone(), &planned), Verdict::Mixed);
    // Never planned (e.g. quarantined): it only looks half applied.
    assert_eq!(judge(half, &unplanned), Verdict::Other);
    assert_eq!(judge(edited.clone(), &[]), Verdict::Other);
    assert_eq!(judge(edited.clone(), &planned), Verdict::AtMerge);
    assert_eq!(judge(edited.clone(), &unplanned), Verdict::Other);
    let pending = [StagedRun {
        planned: [doc(1)].into(),
        pending: [page(2).xochitl_path(&doc(1))].into(),
    }];
    assert_eq!(judge(edited.clone(), &pending), Verdict::Other);
    // Either of two runs (an interrupted one and the reconciliation that
    // finished it) is evidence.
    assert_eq!(
        judge(edited, &[pending[0].clone(), planned[0].clone()]),
        Verdict::AtMerge
    );
}

#[test]
fn step_14_skips_changed_documents_and_their_partners() {
    let mut blobs = Blobs::default();
    let metadata_blob = blobs.json(metadata("A", "", "DocumentType"));
    let before: BTreeMap<DocId, BTreeMap<DocFile, Oid>> = [
        (doc(1), [(DocFile::Metadata, metadata_blob.clone())].into()),
        (doc(2), [(DocFile::Metadata, metadata_blob.clone())].into()),
        (doc(3), [(DocFile::Metadata, metadata_blob)].into()),
    ]
    .into();
    let files_now: BTreeMap<DocId, BTreeSet<DocFile>> = before
        .iter()
        .map(|(doc, files)| (doc.clone(), files.keys().cloned().collect()))
        .collect();
    // 1 was renamed by the user since the read; 2 is its conflict copy.
    let moved: BTreeMap<DocId, BTreeMap<DocFile, Reread>> = [(
        doc(1),
        [(
            DocFile::Metadata,
            Reread::Json(Some(metadata("B", "", "DocumentType"))),
        )]
        .into(),
    )]
    .into();
    let current: BTreeMap<DocId, TreeItem> = [
        (doc(1), item("", false)),
        (doc(2), item("", false)),
        (doc(3), item("", false)),
    ]
    .into();
    let planned = vec![write(1), write(2), write(3)];
    let skipped = skipped(&Checks {
        rules: rules(),
        planned: &planned,
        before: &before,
        files_now: &files_now,
        moved: &moved,
        sha256: &HashMap::new(),
        json: &blobs.0,
        current: &current,
        merged: &current,
        touched: &[doc(1)].into(),
        partners: &partners([(doc(1), doc(2))]),
    })
    .unwrap();
    assert_eq!(skipped, [doc(1), doc(2)].into());
}

#[test]
fn restage_contents_and_removals() {
    let entry = |n: u32, content: Content| Planned {
        doc: doc(n),
        change: Change::Write {
            writes: vec![Write {
                file: DocFile::Content,
                mode: FileMode::Regular,
                content,
            }],
            removes: vec![page(9)],
        },
    };
    let staged = contents(&[
        entry(1, Content::Bytes(b"old".to_vec())),
        entry(2, Content::Blob(oid(1))),
    ]);
    assert_eq!(staged.len(), 2);
    let again = restage(
        &[
            entry(1, Content::Bytes(b"new".to_vec())),
            entry(2, Content::Blob(oid(1))),
        ],
        &staged,
    );
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].doc, doc(1));
    assert!(matches!(&again[0].change, Change::Write { removes, .. } if removes.is_empty()));
    let read: BTreeMap<DocId, BTreeMap<DocFile, Oid>> = [(
        doc(3),
        [(DocFile::Metadata, oid(1)), (page(1), oid(2))].into(),
    )]
    .into();
    let removed = removed(&[entry(1, Content::Blob(oid(1))), delete(3)], &read);
    assert_eq!(
        removed,
        [
            page(9).xochitl_path(&doc(1)),
            DocFile::Metadata.xochitl_path(&doc(3)),
            page(1).xochitl_path(&doc(3)),
        ]
        .into()
    );
}

#[test]
fn step_14_counts_moved_new_gone_and_racy_files() {
    let second = 1_000_000_000i64;
    let stat = |mtime_ns: i64, inode: u64| StatSignature {
        size: 1,
        mtime_ns,
        ctime_ns: mtime_ns,
        inode,
    };
    let path = |name: &str| XochitlPath::new(name).unwrap();
    let read: BTreeMap<XochitlPath, StatSignature> = [
        (path("fine"), stat(5 * second + 7, 1)),
        (path("coarse-old"), stat(4 * second, 2)),
        // A coarse mtime in the second of the step-6 start: xochitl may
        // have changed it within that second without moving the stat.
        (path("coarse-racy"), stat(10 * second, 3)),
        (path("changed"), stat(5 * second + 1, 4)),
        (path("gone"), stat(5 * second + 2, 5)),
    ]
    .into();
    let now: BTreeMap<XochitlPath, StatSignature> = [
        (path("fine"), stat(5 * second + 7, 1)),
        (path("coarse-old"), stat(4 * second, 2)),
        (path("coarse-racy"), stat(10 * second, 3)),
        (path("changed"), stat(6 * second + 1, 4)),
        (path("new"), stat(6 * second + 2, 6)),
    ]
    .into();
    let started_at = 10 * second + 500_000_000;
    assert_eq!(
        moved_since_read(&read, &now, started_at),
        [
            path("changed"),
            path("coarse-racy"),
            path("gone"),
            path("new")
        ]
        .into()
    );
    // Started a second later, the coarse entry is settled too.
    assert_eq!(
        moved_since_read(&read, &now, 11 * second),
        [path("changed"), path("gone"), path("new")].into()
    );
}

#[test]
fn reconciliation_finishes_mixed_documents_the_live_tree_allows() {
    let mut blobs = Blobs::default();
    let files = |version: &DocVersion| -> BTreeMap<DocFile, Oid> {
        version
            .files
            .iter()
            .map(|(file, entry)| (file.clone(), entry.id.clone()))
            .collect()
    };
    // 1: the merge moves it into folder 4, which the tablet deleted.
    // 2: a folder the merge deletes; the tablet put document 3 into it.
    // 5: a plain write. 6: still at the snapshot (not finished).
    let one_merged = notebook(&mut blobs, "One", doc(4).as_str(), &[(1, 11)]);
    let one_now = notebook(&mut blobs, "One", "", &[(1, 10)]);
    let five_merged = notebook(&mut blobs, "Five", "", &[(1, 51)]);
    let five_now = notebook(&mut blobs, "Five", "", &[(1, 50)]);
    let merge = RepoTree {
        docs: [(doc(1), one_merged), (doc(5), five_merged)].into(),
        extra: Default::default(),
    };
    let tablet: BTreeMap<DocId, BTreeMap<DocFile, Oid>> =
        [(doc(1), files(&one_now)), (doc(5), files(&five_now))].into();
    let verdicts: BTreeMap<DocId, Verdict> = [
        (doc(1), Verdict::Mixed),
        (doc(2), Verdict::Mixed),
        (doc(5), Verdict::Mixed),
        (doc(6), Verdict::AtSnapshot),
    ]
    .into();
    let current: BTreeMap<DocId, TreeItem> = [
        (doc(1), item("", false)),
        (doc(2), item("", true)),
        (doc(3), item(doc(2).as_str(), false)),
        (doc(5), item("", false)),
    ]
    .into();
    let merged: BTreeMap<DocId, TreeItem> = [
        (doc(1), item(doc(4).as_str(), false)),
        (doc(5), item("", false)),
    ]
    .into();
    let snapshot = RepoTree::default();
    let run = |forks: &[(DocId, DocId)]| {
        let journaled = Journaled {
            rules: rules(),
            merge: &merge,
            snapshot: &snapshot,
            tablet: &tablet,
            staged: &[],
            forks,
            json: &blobs.0,
        };
        let (planned, skipped) = finish(&Finish {
            journaled: &journaled,
            verdicts: &verdicts,
            local: &BTreeMap::new(),
            current: &current,
            merged: &merged,
        })
        .unwrap();
        let planned: Vec<DocId> = planned.into_iter().map(|entry| entry.doc).collect();
        (planned, skipped)
    };
    assert_eq!(run(&[]), (vec![doc(5)], [doc(1), doc(2)].into()));
    // A fork partner of a skipped document is skipped with it.
    assert_eq!(
        run(&[(doc(1), doc(5))]),
        (Vec::new(), [doc(1), doc(2), doc(5)].into())
    );
}

/// The diary: the merge brings B's ink on page 1 and a new page 2 (so
/// its `.content` changes too); the apply moved both pages, then died
/// before `.content`; the user then renamed the diary (a `.metadata` the
/// merge leaves alone).
struct HalfApplied {
    blobs: Blobs,
    merge: DocVersion,
    snapshot: DocVersion,
    tablet: BTreeMap<DocFile, Oid>,
    pending: BTreeSet<String>,
}

fn half_applied() -> HalfApplied {
    let mut blobs = Blobs::default();
    let snapshot = notebook(&mut blobs, "Diary", "", &[(1, 10)]);
    let merge = notebook(&mut blobs, "Diary", "", &[(1, 11), (2, 20)]);
    let renamed = blobs.json(metadata("Diary (A)", "", "DocumentType"));
    let tablet: BTreeMap<DocFile, Oid> = [
        (DocFile::Metadata, renamed),
        (
            DocFile::Content,
            snapshot.files[&DocFile::Content].id.clone(),
        ),
        (page(1), oid(11)),
        (page(2), oid(20)),
    ]
    .into();
    let pending = [DocFile::Content.xochitl_path(&doc(1))].into();
    HalfApplied {
        blobs,
        merge,
        snapshot,
        tablet,
        pending,
    }
}

impl HalfApplied {
    fn target(&self) -> Option<DocVersion> {
        partial_target(
            rules(),
            &doc(1),
            Some(&self.merge),
            Some(&self.snapshot),
            &self.tablet,
            &self.pending,
            &self.blobs.0,
        )
        .unwrap()
    }
}

#[test]
fn a_half_applied_document_the_user_changed_is_finished_file_by_file() {
    let case = half_applied();
    let target = case.target().expect("finishable");
    // The pending `.content` from the merge, the user's `.metadata`, the
    // pages as the apply wrote them.
    assert_eq!(
        target.files[&DocFile::Content],
        case.merge.files[&DocFile::Content]
    );
    assert_eq!(
        target.files[&DocFile::Metadata].id,
        case.tablet[&DocFile::Metadata]
    );
    assert_eq!(target.files[&page(1)].id, oid(11));
    assert_eq!(target.files[&page(2)].id, oid(20));
    // The user inked page 1 after the apply wrote it: on top, kept.
    let mut inked = half_applied();
    inked.tablet.insert(page(1), oid(12));
    assert_eq!(inked.target().unwrap().files[&page(1)].id, oid(12));
}

#[test]
fn only_a_started_unconflicted_apply_is_finished_file_by_file() {
    // Nothing written yet: the snapshot base merges the user's rename.
    let mut untouched = half_applied();
    untouched.tablet.insert(page(1), oid(10));
    untouched.tablet.remove(&page(2));
    untouched
        .pending
        .extend([page(1).xochitl_path(&doc(1)), page(2).xochitl_path(&doc(1))]);
    assert!(untouched.target().is_none());
    // The user changed a file whose write is still pending: a conflict.
    let mut conflict = half_applied();
    let edited = conflict.blobs.json(content(&["p1", "p9"]));
    conflict.tablet.insert(DocFile::Content, edited);
    assert!(conflict.target().is_none());
    // Moved (no longer staged), then changed by the user: on top.
    let mut moved = half_applied();
    moved.pending.clear();
    let edited = moved.blobs.json(content(&["p1", "p9"]));
    moved.tablet.insert(DocFile::Content, edited);
    assert!(moved.target().is_none(), "nothing left pending");
    // Nothing pending: `apply_finished` covers it.
    let mut done = half_applied();
    done.tablet.insert(
        DocFile::Content,
        done.merge.files[&DocFile::Content].id.clone(),
    );
    assert!(done.target().is_none());
}

#[test]
fn reconciliation_judges_and_finishes_a_partly_applied_edited_document() {
    let case = half_applied();
    let merge = RepoTree {
        docs: [(doc(1), case.merge.clone())].into(),
        extra: Default::default(),
    };
    let snapshot = RepoTree {
        docs: [(doc(1), case.snapshot.clone())].into(),
        extra: Default::default(),
    };
    let tablet = [(doc(1), case.tablet.clone())].into();
    let staged = [StagedRun {
        planned: [doc(1)].into(),
        pending: case.pending.clone(),
    }];
    let journaled = Journaled {
        rules: rules(),
        merge: &merge,
        snapshot: &snapshot,
        tablet: &tablet,
        staged: &staged,
        forks: &[],
        json: &case.blobs.0,
    };
    let judged = verdicts(&journaled).unwrap();
    assert_eq!(judged[&doc(1)], Verdict::Partial);
    let (planned, skipped) = finish(&Finish {
        journaled: &journaled,
        verdicts: &judged,
        local: &BTreeMap::new(),
        current: &[(doc(1), item("", false))].into(),
        merged: &[(doc(1), item("", false))].into(),
    })
    .unwrap();
    assert!(skipped.is_empty());
    let [entry] = planned.as_slice() else {
        panic!("{planned:?}")
    };
    let Change::Write { writes, removes } = &entry.change else {
        panic!("{entry:?}")
    };
    // Only the pending file: never the user's `.metadata`.
    assert_eq!(
        writes.iter().map(|write| &write.file).collect::<Vec<_>>(),
        vec![&DocFile::Content]
    );
    assert!(removes.is_empty());
    // Without the staging evidence, no run planned it: left as it is.
    let journaled = Journaled {
        staged: &[],
        ..journaled
    };
    assert_eq!(verdicts(&journaled).unwrap()[&doc(1)], Verdict::Other);
}

#[test]
fn a_pending_removal_is_finished_and_the_users_change_kept() {
    // The merge removes page 2 (and so changes `.content`); the apply
    // wrote page 1 and `.content`, then died before removing page 2; the
    // user then renamed the notebook.
    let mut blobs = Blobs::default();
    let snapshot = notebook(&mut blobs, "Diary", "", &[(1, 10), (2, 20)]);
    let merge = notebook(&mut blobs, "Diary", "", &[(1, 11)]);
    let renamed = blobs.json(metadata("Diary (A)", "", "DocumentType"));
    let tablet: BTreeMap<DocFile, Oid> = [
        (DocFile::Metadata, renamed.clone()),
        (DocFile::Content, merge.files[&DocFile::Content].id.clone()),
        (page(1), oid(11)),
        (page(2), oid(20)),
    ]
    .into();
    let target = partial_target(
        rules(),
        &doc(1),
        Some(&merge),
        Some(&snapshot),
        &tablet,
        &BTreeSet::new(),
        &blobs.0,
    )
    .unwrap()
    .expect("finishable");
    assert!(!target.files.contains_key(&page(2)));
    assert_eq!(target.files[&DocFile::Metadata].id, renamed);
    // The user inked page 2 meanwhile: removing it would lose that.
    let mut inked = tablet.clone();
    inked.insert(page(2), oid(21));
    assert!(
        partial_target(
            rules(),
            &doc(1),
            Some(&merge),
            Some(&snapshot),
            &inked,
            &BTreeSet::new(),
            &blobs.0,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn a_partial_finish_the_live_tree_refuses_is_kept() {
    // The merge moves the notebook into folder 4 (its `.metadata`,
    // pending) and changes page 2 (written); the user inked page 1, which
    // the merge leaves alone. Folder 4 is gone from the tablet: finishing
    // would orphan the notebook, so it is skipped (kept at the snapshot).
    let mut blobs = Blobs::default();
    let snapshot = notebook(&mut blobs, "One", "", &[(1, 10), (2, 20)]);
    let merge = notebook(&mut blobs, "One", doc(4).as_str(), &[(1, 10), (2, 21)]);
    let tablet: BTreeMap<DocId, BTreeMap<DocFile, Oid>> = [(
        doc(1),
        [
            (
                DocFile::Metadata,
                snapshot.files[&DocFile::Metadata].id.clone(),
            ),
            (
                DocFile::Content,
                snapshot.files[&DocFile::Content].id.clone(),
            ),
            (page(1), oid(12)),
            (page(2), oid(21)),
        ]
        .into(),
    )]
    .into();
    let merge_tree = RepoTree {
        docs: [(doc(1), merge)].into(),
        extra: Default::default(),
    };
    let snapshot_tree = RepoTree {
        docs: [(doc(1), snapshot)].into(),
        extra: Default::default(),
    };
    let staged = [StagedRun {
        planned: [doc(1)].into(),
        pending: [DocFile::Metadata.xochitl_path(&doc(1))].into(),
    }];
    let journaled = Journaled {
        rules: rules(),
        merge: &merge_tree,
        snapshot: &snapshot_tree,
        tablet: &tablet,
        staged: &staged,
        forks: &[],
        json: &blobs.0,
    };
    let judged = verdicts(&journaled).unwrap();
    assert_eq!(judged[&doc(1)], Verdict::Partial);
    let (planned, skipped) = finish(&Finish {
        journaled: &journaled,
        verdicts: &judged,
        local: &BTreeMap::new(),
        current: &[(doc(1), item("", false))].into(),
        merged: &[(doc(1), item(doc(4).as_str(), false))].into(),
    })
    .unwrap();
    assert!(planned.is_empty(), "{planned:?}");
    assert_eq!(skipped, [doc(1)].into());
}
