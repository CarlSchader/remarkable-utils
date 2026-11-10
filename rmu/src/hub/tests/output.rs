//! Human and plain rendering, error details and `finish`.

use super::*;

fn contains_all(human: &str, expected: &[&str]) {
    expected.iter().for_each(|expected| {
        assert!(
            human.contains(expected),
            "{expected:?} missing from:\n{human}"
        )
    });
}

#[test]
fn renders_sync_reports() {
    let report = Report::Sync(sync_report());
    let human = text(&render::human(&report, &names()));
    contains_all(
        &human,
        &[
            "Synced: merged with origin and pushed cccccccccccc (after 2 push attempts)",
            "The tablet is at cccccccccccc (its backup: bbbbbbbbbbbb)",
            "Updated on the tablet (5):\n  \"Paper one\" (8c1f0b9e-0000-4000-8000-000000000001)\n  8c1f0b9e-0000-4000-8000-000000000002",
            "7 files (2.00 KiB) read while the tablet's UI was down",
            "3 files stay on the tablet but not mirrored (thumbnails, caches, unknown files)",
            "Documents: 2 changed on the tablet, 3 changed on origin, 1 forked",
            "Deletions: 1 from origin, 2 from the tablet",
            "Conflict copies (1):\n  \"Lecture notes (conflict Desk tablet 2026-11-09)\" (8c1f0b9e-0000-4000-8000-00000000000a), the tablet's version of \"Lecture notes\" (8c1f0b9e-0000-4000-8000-000000000009)\nCompare each copy with its original; rename or delete the copy to close it",
            "Skipped: changed on the tablet during the sync, or would break its folder tree (the next sync picks them up) (2):\n  \"Edited meanwhile\" (8c1f0b9e-0000-4000-8000-000000000004)",
            "Quarantined, malformed on the tablet (left untouched until it parses again) (1):\n  8c1f0b9e-0000-4000-8000-000000000005: unparsable .metadata",
            "Files without a .metadata",
            "  \"Kept book\" (8c1f0b9e-0000-4000-8000-000000000007): kept over a deletion: deleted on origin but changed on the other side",
            "  8c1f0b9e-0000-4000-8000-000000000008: restored from the trash to the root, so an edit is not lost",
            "Deleted from origin (1):\n  \"Removed on this tablet\" (8c1f0b9e-0000-4000-8000-000000000014)\n",
            "Deleted from the tablet (1):\n  \"Old draft\" (8c1f0b9e-0000-4000-8000-000000000015)\n",
            "  \"Cleaned up elsewhere\" (8c1f0b9e-0000-4000-8000-000000000017)",
            "1 document also deleted from the tablet: origin deleted them with recorded consent",
            "These deletions are allowed by --allow-deletions",
            "Reconciled an interrupted sync: finished 1 document, 1 left for the next sync",
            "Took over the tablet's lock (pid 4242 on laptop, started ",
            " days ago): the tablet rebooted since it was taken",
            "warning: this repository has only seen reMarkable 2 tablets",
        ],
    );
    assert!(!human.contains("Read "), "{human}");
    // History's names win over the tablet's (a stale `.metadata`).
    let stale = BTreeMap::from([(doc(21), "Stale tablet name".to_string())]);
    let human = text(&render::human(&report, &stale));
    assert!(
        human.contains("\"Old draft\"") && !human.contains("Stale"),
        "{human}"
    );

    let Report::Sync(sync) = &report else {
        unreachable!()
    };
    let clone = text(&render::human(
        &Report::Clone(sync.clone()),
        &BTreeMap::new(),
    ));
    assert!(clone.starts_with("Cloned: merged with origin"), "{clone}");

    let up_to_date = SyncReport {
        update: MainUpdate::UpToDate,
        applied: Vec::new(),
        push_attempts: 0,
        read: ReadSummary::default(),
        ..sync.clone()
    };
    let human = text(&render::human(&Report::Sync(up_to_date), &BTreeMap::new()));
    assert!(human.contains("Synced: already in sync with origin; nothing pushed"));
    assert!(human.contains("Nothing to change on the tablet"));
    assert!(human.contains("Deletions: 1 from origin, 0 from the tablet"));
    assert!(
        !human.contains("Deleted from the tablet"),
        "nothing was applied"
    );
    assert!(!human.contains("read while"), "nothing was read: {human}");

    let many = SyncReport {
        applied: (40..40 + render::NAMED_UPDATES as u64 + 1)
            .map(doc)
            .collect(),
        ..sync.clone()
    };
    let human = text(&render::human(
        &Report::Sync(many.clone()),
        &BTreeMap::new(),
    ));
    assert!(
        human.contains("Updated 11 documents on the tablet"),
        "{human}"
    );
    assert!(!render::named_docs(&Report::Sync(many)).contains(&doc(40)));

    assert_eq!(render::plain(&report), vec!["c".repeat(40)]);
    let warnings = render::warnings(&report);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with("warning: this repository has only seen reMarkable 2 tablets"),
        "{warnings:?}"
    );
    // Names are looked up only for the listed documents.
    let named = render::named_docs(&report);
    [doc(1), doc(4), doc(9), doc(21)]
        .iter()
        .for_each(|doc| assert!(named.contains(doc), "{doc}"));
}

#[test]
fn renders_status_and_dry_run_reports() {
    let names = BTreeMap::from([(doc(1), "New notebook".to_string())]);
    let report = Report::Status(status_report());
    let human = text(&render::human(&report, &names));
    contains_all(
        &human,
        &[
            "Tablet \"Desk tablet\" (0f0e0d0c-0000-4000-8000-0000000000c1)",
            "Origin ssh://git@example.invalid/library.git (repository 0f0e0d0c-0000-4000-8000-0000000000a1)",
            "Lock: held by pid 4242 on laptop, started ",
            "if that run is gone (its computer crashed or was closed), `rmu hub sync --break-lock` takes the lock over",
            "An interrupted sync left a journal; the next sync reconciles it",
            "3 changes on the tablet since its last sync",
            "  added    \"New notebook\" (8c1f0b9e-0000-4000-8000-000000000001)",
            "  modified 8c1f0b9e-0000-4000-8000-000000000002",
            "  deleted  \"Gone notebook\" (8c1f0b9e-0000-4000-8000-000000000003)",
            "Would delete from origin (1):\n  \"Removed on this tablet\"",
            "Origin has 4 commits the tablet has not seen",
            "Open conflict copies (rename or delete a copy to close it) (1):",
            "Quarantined, malformed on the tablet",
            "A sync would merge: Documents: 1 added",
            "\"visibleName\" changed on both sides; kept origin's value",
            "Would delete from origin (1):",
            "Would delete from the tablet (2):",
            "1 document also would be deleted from the tablet",
            "A sync would refuse these deletions without --allow-deletions",
            "(read while xochitl runs: best effort)",
        ],
    );
    assert_eq!(
        render::plain(&report),
        vec![
            format!("A {}", doc(1)),
            format!("M {}", doc(2)),
            format!("D {}", doc(3)),
        ]
    );

    let clean = StatusReport {
        lock: LockState::Free,
        journal_pending: false,
        quarantined: BTreeMap::new(),
        changes: BTreeMap::new(),
        behind: 0,
        open_conflicts: Vec::new(),
        guard: DeletionGuard::default(),
        summary: Summary::default(),
        notes: Vec::new(),
        ..status_report()
    };
    let human = text(&render::human(
        &Report::Status(clean.clone()),
        &BTreeMap::new(),
    ));
    assert!(human.contains("Lock: free"));
    assert!(human.contains("No changes on the tablet since its last sync"));
    assert!(human.contains("The tablet has everything on origin"));
    assert!(!human.contains("Would delete"));
    assert!(!human.contains("--break-lock"));

    let unknown_owner = StatusReport {
        lock: LockState::Held(None),
        ..clean.clone()
    };
    assert!(
        text(&render::human(
            &Report::Status(unknown_owner),
            &BTreeMap::new()
        ))
        .contains("Lock: held by an unknown owner;")
    );

    let dry_run = Report::DryRun(DryRunReport {
        status: clean,
        ..dry_run_report()
    });
    let human = text(&render::human(&dry_run, &BTreeMap::new()));
    assert!(human.contains("A sync would write 2 documents to the tablet and delete 1 from it"));
    assert!(human.contains(&format!("  write   {}", doc(30))));
    assert!(human.contains(&format!("  delete  {}", doc(32))));
    assert!(human.contains("It would push to origin"));
    assert!(
        human.ends_with("(dry run: nothing was changed; read while xochitl runs: best effort)")
    );
    assert!(!human.contains("\n(read while xochitl runs"));
    assert_eq!(
        render::plain(&dry_run),
        vec![
            format!("write {}", doc(30)),
            format!("write {}", doc(31)),
            format!("delete {}", doc(32)),
        ]
    );
}

#[test]
fn renders_backup_and_init_reports() {
    let report = Report::Backup(backup_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    assert!(human.contains("Backed up the tablet to origin (eeeeeeeeeeee)"));
    assert!(human.contains("1 file (10 B) read while the tablet's UI was down"));
    assert!(!human.contains("not mirrored"));
    assert!(human.contains("Took over the tablet's lock (an unknown owner): --break-lock"));
    assert_eq!(render::plain(&report), vec!["e".repeat(40)]);
    let again = Report::Backup(BackupReport {
        pushed: false,
        took_over: None,
        ..backup_report()
    });
    assert!(
        text(&render::human(&again, &BTreeMap::new())).contains("Origin already had this backup")
    );

    let init = init_report();
    let human = text(&render::human(&init, &BTreeMap::new()));
    assert!(human.contains("Initialised origin /srv/library.git"));
    assert!(human.contains("rmu hub clone /srv/library.git --name <name>"));
    assert_eq!(render::plain(&init), vec![repo_id().to_string()]);
}

#[test]
fn renders_pulled_and_inferred_syncs() {
    let report = SyncReport {
        update: MainUpdate::Pulled,
        inferred: BTreeMap::from([(doc(1), oid('a')), (doc(2), oid('a'))]),
        ..sync_report()
    };
    let human = text(&render::human(&Report::Clone(report), &BTreeMap::new()));
    assert!(
        human.starts_with("Cloned: took origin's changes; the tablet had none of its own to push"),
        "{human}"
    );
    assert!(human.contains("2 documents matched a version in origin's history"));
    let human = text(&render::human(
        &Report::Sync(sync_report()),
        &BTreeMap::new(),
    ));
    assert!(!human.contains("matched a version"), "{human}");
}

#[test]
fn renders_remote_set_url_and_detach_reports() {
    let report = Report::Remote(remote_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    assert!(human.contains("Tablet \"Desk tablet\" (0f0e0d0c-0000-4000-8000-0000000000c1)"));
    assert!(human.contains("Origin ssh://git@example.invalid/library.git"));
    assert!(!human.contains("This run uses"));
    assert_eq!(
        render::plain(&report),
        vec!["ssh://git@example.invalid/library.git"]
    );
    let overridden = Report::Remote(RemoteReport {
        effective: "laptop:library.git".into(),
        ..remote_report()
    });
    assert!(
        text(&render::human(&overridden, &BTreeMap::new()))
            .contains("This run uses laptop:library.git (--origin)")
    );

    let set_url = Report::SetUrl(set_url_report());
    let human = text(&render::human(&set_url, &BTreeMap::new()));
    assert!(human.contains(
        "Origin of \"Desk tablet\" changed from /old/library.git to ssh://git@example.invalid/library.git"
    ));
    assert!(human.contains("warning: could not release the tablet lock: connection lost"));
    assert_eq!(
        render::plain(&set_url),
        vec!["ssh://git@example.invalid/library.git"]
    );

    let report = Report::Detach(detach_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    assert!(human.contains(
        "Detached the tablet from repository 0f0e0d0c-0000-4000-8000-0000000000a1; its documents are untouched"
    ));
    assert!(human.contains("Deleted its device ref refs/rmu/devices/"));
    assert!(human.contains("`rmu hub clone <origin>` attaches it again"));
    assert!(human.contains("warning: the journal of an interrupted sync was discarded"));
    assert_eq!(
        render::plain(&report),
        vec!["refs/rmu/devices/0f0e0d0c-0000-4000-8000-0000000000c1"]
    );
    let kept = Report::Detach(DetachReport {
        pruned: None,
        warnings: Vec::new(),
        ..detach_report()
    });
    assert!(render::plain(&kept).is_empty());
    assert!(!text(&render::human(&kept, &BTreeMap::new())).contains("device ref"));
}

#[test]
fn long_lists_are_capped() {
    let report = SyncReport {
        skipped: (100..130).map(doc).collect(),
        ..sync_report()
    };
    let human = render::human(&Report::Sync(report.clone()), &BTreeMap::new());
    assert!(human.iter().any(|line| line == "  ... and 10 more"));
    let named = render::named_docs(&Report::Sync(report));
    assert_eq!(
        named.iter().filter(|d| **d >= doc(100)).count(),
        render::LIST_LIMIT
    );
    let status = StatusReport {
        changes: (100..125).map(|n| (doc(n), DocChange::Modified)).collect(),
        ..status_report()
    };
    let human = render::human(&Report::Status(status), &BTreeMap::new());
    assert!(human.iter().any(|line| line == "  ... and 5 more"));
}

#[test]
fn control_characters_are_escaped() {
    let report = SyncReport {
        forks: vec![Fork {
            original: doc(9),
            copy: doc(10),
            name: "Evil\u{1b}[2J copy".into(),
        }],
        ..sync_report()
    };
    let names = BTreeMap::from([(doc(4), "Bell\u{7} and\nnewline".to_string())]);
    let human = render::human(&Report::Sync(report), &names);
    assert!(
        human.iter().all(|line| !line.chars().any(char::is_control)),
        "{human:?}"
    );
    let human = text(&human);
    assert!(human.contains("\"Evil\\u{1b}[2J copy\""), "{human}");
    assert!(human.contains("\"Bell\\u{7} and\\nnewline\""), "{human}");
    assert_eq!(render::escape_controls("plain ünïcode"), "plain ünïcode");
}

#[test]
fn renders_errors() {
    // The documents to delete from origin are gone from the tablet: their
    // names come from history, which the error carries.
    let error = guard_refusal();
    let details = text(&render::error_details(
        &error,
        &render::ErrorContext::default(),
    ));
    assert!(details.contains("The tablet looks empty"));
    assert!(details.contains(&format!(
        "Would delete from origin (2):\n  \"Deleted paper\" ({})\n  \"Deleted notes\" ({})",
        doc(1),
        doc(2)
    )));
    assert!(details.contains("Would delete from the tablet (1):\n  \"Old draft\""));
    // The tablet's names only fill in, never override history's.
    let Error::HubCycle(CycleError::DeletionsNeedConsent {
        from_origin,
        from_tablet,
        ..
    }) = guard_refusal()
    else {
        unreachable!()
    };
    let partial = Error::HubCycle(CycleError::DeletionsNeedConsent {
        from_origin,
        from_tablet,
        empty_snapshot: false,
        names: BTreeMap::from([(doc(1), "Deleted paper".to_string())]),
    });
    let details = text(&render::error_details(
        &partial,
        &render::ErrorContext {
            names: BTreeMap::from([
                (doc(1), "Stale tablet name".to_string()),
                (doc(3), "Still on the tablet".to_string()),
            ]),
            ..Default::default()
        },
    ));
    assert!(details.contains("\"Deleted paper\""), "{details}");
    assert!(!details.contains("Stale tablet name"), "{details}");
    assert!(details.contains("\"Still on the tablet\""), "{details}");
    assert!(
        details.contains(&format!("\n  {}\n", doc(2))),
        "no name anywhere: {details}"
    );
    assert!(details.contains("--allow-deletions"));
    assert_eq!(render::error_docs(&error), vec![doc(3), doc(1), doc(2)]);

    let empty = Error::HubCycle(CycleError::OriginEmpty {
        url: "ssh://git@example.invalid/library.git".into(),
    });
    let details = text(&render::error_details(
        &empty,
        &render::ErrorContext {
            repo_id: Some(repo_id()),
            cache_dir: Some(PathBuf::from("/home/user/.cache/rmu/hub")),
            ..Default::default()
        },
    ));
    contains_all(
        &details,
        &[
            "Origin ssh://git@example.invalid/library.git has no main branch",
            "1. Check the URL",
            "2. Restore origin from a backup, or from the local cache of the computer that synced",
            "git --git-dir /home/user/.cache/rmu/hub/0f0e0d0c-0000-4000-8000-0000000000a1.git push ssh://git@example.invalid/library.git refs/remotes/origin/main:refs/heads/main",
            "git --git-dir /home/user/.cache/rmu/hub/0f0e0d0c-0000-4000-8000-0000000000a1.git push ssh://git@example.invalid/library.git 'refs/remotes/origin/devices/*:refs/rmu/devices/*'",
            "takes 'refs/remotes/origin/devices/*:refs/heads/rmu/devices/*' instead.",
            "`rejected (non-fast-forward)` lines are harmless",
            "A tablet whose backups are not restored needs `rmu hub clone <origin> --reattach`",
            "3. Only as a last resort",
        ],
    );
    // Paths are quoted for the shell, as `ssh::shell_quote` does.
    let odd = Error::HubCycle(CycleError::OriginEmpty {
        url: "/srv/it's here.git".into(),
    });
    let details = text(&render::error_details(
        &odd,
        &render::ErrorContext {
            repo_id: Some(repo_id()),
            cache_dir: Some(PathBuf::from("/home/o'neil/cache dir")),
            ..Default::default()
        },
    ));
    assert!(
        details.contains(
            "git --git-dir '/home/o'\\''neil/cache dir/0f0e0d0c-0000-4000-8000-0000000000a1.git' push '/srv/it'\\''s here.git' refs/remotes/origin/main:refs/heads/main"
        ),
        "{details}"
    );
    let details = text(&render::error_details(
        &empty,
        &render::ErrorContext::default(),
    ));
    assert!(
        details.contains("$XDG_CACHE_HOME/rmu/hub/<repo-id>.git"),
        "{details}"
    );

    let conflict = Error::HubCycle(CycleError::DeviceRefConflict {
        reference: "refs/rmu/devices/x".into(),
        recorded: "a".into(),
        remote: "b".into(),
    });
    let details = text(&render::error_details(
        &conflict,
        &render::ErrorContext::default(),
    ));
    assert!(
        details.contains("`rmu hub clone <origin> --reattach`"),
        "{details}"
    );

    let locked = Error::HubDevice(DeviceError::Locked("pid 1 on laptop".into()));
    assert!(render::error_details(&locked, &render::ErrorContext::default()).is_empty());
    assert!(locked.to_string().contains("--break-lock"));
}

/// A stdout whose reader went away (`rmu hub sync | head -0`).
struct ClosedPipe;

impl Write for ClosedPipe {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn context(output: Output, quiet: bool) -> OutputContext<'static> {
    OutputContext {
        tablet: None,
        cache_dir: None,
        output,
        quiet,
    }
}

/// `finish` on a report: stdout, stderr.
fn finished(report: Report, output: Output, quiet: bool) -> (String, String) {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    finish(
        Ok(report),
        &context(output, quiet),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();
    (
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

#[test]
fn warnings_in_every_output_mode() {
    let warning = "warning: this repository has only seen reMarkable 2 tablets";
    let report = || Report::Sync(sync_report());

    let (stdout, stderr) = finished(report(), Output::Json, false);
    assert!(stdout.starts_with('{') && !stdout.contains("warning: "));
    assert!(stderr.starts_with(warning), "{stderr}");
    let (_, stderr) = finished(report(), Output::Json, true);
    assert!(stderr.is_empty(), "--quiet: {stderr}");

    let (stdout, stderr) = finished(report(), Output::Human, false);
    assert!(stdout.contains(warning) && stderr.is_empty());
    let (stdout, _) = finished(report(), Output::Human, true);
    assert!(stdout.contains(warning), "the summary is the result");

    let (stdout, stderr) = finished(report(), Output::Plain, false);
    assert_eq!(stdout, format!("{}\n", "c".repeat(40)));
    assert!(stderr.contains(warning), "{stderr}");
    let (_, stderr) = finished(report(), Output::Plain, true);
    assert!(stderr.is_empty(), "--quiet: {stderr}");
}

#[test]
fn a_closed_stdout_after_success_is_not_an_error() {
    [Output::Json, Output::Human, Output::Plain]
        .into_iter()
        .for_each(|output| {
            let result = finish(
                Ok(Report::Sync(sync_report())),
                &context(output, true),
                &mut ClosedPipe,
                &mut Vec::new(),
            );
            assert!(result.is_ok(), "{output:?}: {result:?}");
        });
    // An error keeps its exit status even when nothing can be written.
    let result = finish(
        Err(guard_refusal()),
        &context(Output::Json, false),
        &mut ClosedPipe,
        &mut ClosedPipe,
    );
    assert_eq!(exit_code(&result.unwrap_err()), 3);
}

#[test]
fn a_document_back_after_its_deletion_is_noted() {
    let report = SyncReport {
        notes: vec![Note {
            doc: doc(40),
            kind: NoteKind::ReturnedAfterDeletion,
        }],
        ..sync_report()
    };
    let names = BTreeMap::from([(doc(40), "Field notes".to_string())]);
    let human = text(&render::human(&Report::Sync(report), &names));
    assert!(
        human.contains(&format!(
            "  \"Field notes\" ({}): came back: it was deleted earlier, but another tablet changed or restored it since",
            doc(40)
        )),
        "{human}"
    );
}

#[test]
fn renders_the_library_history() {
    let report = Report::Log(log_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    contains_all(
        &human,
        &[
            "History of origin ssh://git@example.invalid/library.git (repository 0f0e0d0c-0000-4000-8000-0000000000a1), newest first",
            "Tablet \"Desk tablet\" has the library as of cccccccccccc (2 documents at a base of their own)",
            "dddddddddddd  2026-11-09 14:03  restore  rmu\n    1 added\n    added: \"Old (restored 2026-11-09)\" (8c1f0b9e-0000-4000-8000-000000000004)\n    restored \"Old\" (8c1f0b9e-0000-4000-8000-000000000003) as of bbbbbbbbbbbb as \"Old (restored 2026-11-09)\"",
            "cccccccccccc  2026-11-09 14:03  sync     Desk tablet (reMarkable Fake, 3.28.0)  <- the tablet's base",
            "    1 added, 1 changed, 1 deleted",
            "    deleted: \"Old\" (8c1f0b9e-0000-4000-8000-000000000003)",
            "    conflict copy \"Notes (conflict Desk tablet 2026-11-09)\" (8c1f0b9e-0000-4000-8000-000000000002) of \"Notes\" (8c1f0b9e-0000-4000-8000-000000000001)",
            "    1 document deleted with --allow-deletions",
            "    note: 8c1f0b9e-0000-4000-8000-000000000001: \"visibleName\" changed on both sides",
            "aaaaaaaaaaaa  2026-11-09 14:03  init     rmu",
            "... and 5 older entries (-n sets how many are shown)",
        ],
    );
    assert_eq!(
        render::plain(&report),
        vec![
            oid('d').to_string(),
            oid('c').to_string(),
            oid('a').to_string()
        ]
    );
    assert!(render::warnings(&report).is_empty());
    assert!(render::named_docs(&report).is_empty());
}

#[test]
fn renders_a_documents_history() {
    let report = Report::DocLog(doc_log_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    contains_all(
        &human,
        &[
            "\"Notes\" (8c1f0b9e-0000-4000-8000-000000000001), in /Projects",
            "dddddddddddd  2026-11-09 14:03  Desk tablet: changed; conflict copy 8c1f0b9e-0000-4000-8000-000000000002 made from it",
            "cccccccccccc  2026-11-09 14:03  Desk tablet: renamed from \"Draft\" to \"Notes\"; moved from the root to /Projects",
            "bbbbbbbbbbbb  2026-11-09 14:03  Desk tablet: its version as of aaaaaaaaaaaa restored as 8c1f0b9e-0000-4000-8000-000000000004",
            "aaaaaaaaaaaa  2026-11-09 14:03  Desk tablet: added as \"Draft\" in the root",
            "Tablet \"Desk tablet\" has it as of cccccccccccc",
            "To bring a version back as a new document: rmu hub restore 8c1f0b9e-0000-4000-8000-000000000001 --at <commit>",
        ],
    );
    let deleted = DocLogReport {
        place: None,
        tablet: Some(TabletBase {
            base: None,
            ..doc_log_report().tablet.unwrap()
        }),
        ..doc_log_report()
    };
    let human = text(&render::human(&Report::DocLog(deleted), &BTreeMap::new()));
    contains_all(
        &human,
        &[
            "\"Notes\" (8c1f0b9e-0000-4000-8000-000000000001), deleted on origin",
            "Tablet \"Desk tablet\" has no base for it yet",
        ],
    );
    assert_eq!(render::plain(&report).len(), 4);
}

#[test]
fn renders_restore_and_add() {
    let report = Report::Restore(restore_report());
    let human = text(&render::human(&report, &BTreeMap::new()));
    contains_all(
        &human,
        &[
            "Restored \"Old\" (8c1f0b9e-0000-4000-8000-000000000003) as of bbbbbbbbbbbb, just before its deletion (asked for dddddddddddd) as \"Old (restored 2026-11-09)\" (8c1f0b9e-0000-4000-8000-000000000004) in /Projects",
            "Pushed to origin (eeeeeeeeeeee); every tablet gets it at its next sync",
            "(after 2 push attempts: origin moved meanwhile)",
        ],
    );
    assert_eq!(render::plain(&report), vec![doc(4).to_string()]);
    let report = Report::Add(add_report(Some(3)));
    let human = text(&render::human(&report, &BTreeMap::new()));
    contains_all(
        &human,
        &[
            "Added \"Report\" (8c1f0b9e-0000-4000-8000-000000000005) to the root: pdf, 2.00 KiB, 3 pages (page list written)",
            "Pushed to origin (ffffffffffff)",
        ],
    );
    assert!(!human.contains("push attempts"));
    assert_eq!(render::plain(&report), vec![doc(5).to_string()]);
    let human = text(&render::human(
        &Report::Add(add_report(None)),
        &BTreeMap::new(),
    ));
    assert!(
        human.contains("pdf, 2.00 KiB, page list left to the tablet's first open"),
        "{human}"
    );
}

#[test]
fn times_are_utc() {
    assert_eq!(render::when(0), "1970-01-01 00:00");
    assert_eq!(render::when(TIME), "2026-11-09 14:03");
    assert_eq!(render::when(-60), "1969-12-31 23:59");
}
