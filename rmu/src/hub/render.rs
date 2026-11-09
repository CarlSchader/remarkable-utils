//! Human and plain output of `rmu hub`. Pure: reports (and the names
//! looked up for their documents) in, lines out.
//!
//! The human summary is concise: counts for routine work, and lists
//! (capped at [`LIST_LIMIT`] entries) only for what needs attention:
//! conflict copies, skipped and quarantined documents, notes, deletions.

use std::collections::{BTreeMap, BTreeSet};

use indicatif::HumanBytes;
use libremarkable_utils::Error;
use libremarkable_utils::git::Oid;
use libremarkable_utils::hub::cycle::{
    BackupReport, CycleError, DetachReport, DocChange, DryRunReport, MainUpdate, ReadSummary,
    Reconciled, RemoteReport, SetUrlReport, StatusReport, SyncReport, TakeOver, Warning,
};
use libremarkable_utils::hub::device::{LockOwner, LockState, StaleReason};
use libremarkable_utils::hub::merge::{DeletionGuard, Fork, Note, NoteKind, Summary};
use libremarkable_utils::hub::model::DocId;

use super::Report;

/// Visible names of documents, as far as they could be looked up.
pub type Names = BTreeMap<DocId, String>;

/// Entries shown per list; the rest are counted.
pub const LIST_LIMIT: usize = 20;

/// A document for humans: `"Name" (id)`, or the id alone.
fn label(names: &Names, doc: &DocId) -> String {
    names
        .get(doc)
        .map_or_else(|| doc.to_string(), |name| format!("\"{name}\" ({doc})"))
}

/// A commit for humans.
fn short(commit: &Oid) -> &str {
    &commit.as_str()[..12.min(commit.as_str().len())]
}

/// A titled list: `Title (n):` and indented entries, capped.
fn list(lines: &mut Vec<String>, title: &str, entries: Vec<String>) {
    if entries.is_empty() {
        return;
    }
    lines.push(format!("{title} ({}):", entries.len()));
    let shown = entries.len().min(LIST_LIMIT);
    lines.extend(entries[..shown].iter().map(|entry| format!("  {entry}")));
    if entries.len() > shown {
        lines.push(format!("  ... and {} more", entries.len() - shown));
    }
}

/// The documents of a list the summary shows (named first).
fn shown<'a>(docs: impl IntoIterator<Item = &'a DocId>) -> impl Iterator<Item = &'a DocId> {
    docs.into_iter().take(LIST_LIMIT)
}

/// The documents whose names the human summary shows.
pub fn named_docs(report: &Report) -> Vec<DocId> {
    let docs: Vec<&DocId> = match report {
        Report::Init { .. } | Report::Remote(_) | Report::SetUrl(_) | Report::Detach(_) => {
            Vec::new()
        }
        Report::Clone(report) | Report::Sync(report) => {
            shown(report.forks.iter().map(|f| &f.original))
                .chain(shown(&report.skipped))
                .chain(shown(report.quarantined.keys()))
                .chain(shown(report.notes.iter().map(|note| &note.doc)))
                .chain(shown(&report.deletions.from_tablet))
                .chain(shown(&report.deletions.from_origin))
                .collect()
        }
        Report::Backup(report) => shown(report.quarantined.keys()).collect(),
        Report::Status(report) => status_docs(report).collect(),
        Report::DryRun(report) => status_docs(&report.status)
            .chain(shown(&report.writes))
            .chain(shown(&report.deletes))
            .collect(),
    };
    docs.into_iter().cloned().collect()
}

fn status_docs(report: &StatusReport) -> impl Iterator<Item = &DocId> {
    shown(report.changes.keys())
        .chain(shown(report.open_conflicts.iter().map(|c| &c.original)))
        .chain(shown(report.quarantined.keys()))
        .chain(shown(report.notes.iter().map(|note| &note.doc)))
        .chain(shown(&report.guard.from_tablet))
        .chain(shown(&report.guard.from_origin))
}

/// The human summary of a report.
pub fn human(report: &Report, names: &Names) -> Vec<String> {
    match report {
        Report::Init { origin, report } => vec![
            format!("Initialised origin {origin}"),
            format!(
                "Repository id {} (mirror rules version {}), first commit {}",
                report.config.repo_id,
                report.config.mirror_rules,
                short(&report.commit)
            ),
            format!("Attach a tablet with: rmu hub clone {origin} --name <name>"),
        ],
        Report::Clone(report) => sync(report, names, "Cloned"),
        Report::Sync(report) => sync(report, names, "Synced"),
        Report::DryRun(report) => dry_run(report, names),
        Report::Backup(report) => backup(report, names),
        Report::Status(report) => status(report, names),
        Report::Remote(report) => remote(report),
        Report::SetUrl(report) => set_url(report),
        Report::Detach(report) => detach(report),
    }
}

/// Plain stdout when it is not a terminal: the ids a script wants.
pub fn plain(report: &Report) -> Vec<String> {
    match report {
        Report::Init { report, .. } => vec![report.config.repo_id.to_string()],
        Report::Clone(report) | Report::Sync(report) => vec![report.base.to_string()],
        Report::Backup(report) => vec![report.snapshot.to_string()],
        Report::Status(report) => changes(report)
            .map(|(doc, change)| format!("{} {doc}", change_letter(change)))
            .collect(),
        Report::DryRun(report) => report
            .writes
            .iter()
            .map(|doc| format!("write {doc}"))
            .chain(report.deletes.iter().map(|doc| format!("delete {doc}")))
            .collect(),
        Report::Remote(report) => vec![report.origin.clone()],
        Report::SetUrl(report) => vec![report.remote.origin.clone()],
        Report::Detach(report) => report.pruned.iter().cloned().collect(),
    }
}

/// Warnings, for stderr when the result is JSON.
pub fn warnings(report: &Report) -> Vec<String> {
    let warnings: &[Warning] = match report {
        Report::Clone(report) | Report::Sync(report) => &report.warnings,
        Report::Backup(report) => &report.warnings,
        Report::SetUrl(report) => &report.warnings,
        Report::Detach(report) => &report.warnings,
        Report::Init { .. } | Report::DryRun(_) | Report::Status(_) | Report::Remote(_) => &[],
    };
    warnings
        .iter()
        .map(|warning| format!("warning: {warning}"))
        .collect()
}

fn changes(report: &StatusReport) -> impl Iterator<Item = (&DocId, DocChange)> {
    report.changes.iter().map(|(doc, change)| (doc, *change))
}

fn change_letter(change: DocChange) -> char {
    match change {
        DocChange::Added => 'A',
        DocChange::Modified => 'M',
        DocChange::Deleted => 'D',
    }
}

fn change_word(change: DocChange) -> &'static str {
    match change {
        DocChange::Added => "added",
        DocChange::Modified => "modified",
        DocChange::Deleted => "deleted",
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn docs(count: usize) -> String {
    plural(count, "document", "documents")
}

fn read_line(read: &ReadSummary) -> String {
    let mut line = format!(
        "Read {} ({}) from the tablet",
        plural(read.downloaded.files, "file", "files"),
        HumanBytes(read.downloaded.bytes)
    );
    if read.unmirrored > 0 {
        line.push_str(&format!(
            "; {} not mirrored (thumbnails, caches, unknown files)",
            plural(read.unmirrored, "file stays", "files stay")
        ));
    }
    line
}

/// The merge's resolutions, without the unchanged documents.
fn summary_line(summary: &Summary) -> Option<String> {
    let parts: Vec<String> = [
        (summary.changed_ours, "changed on the tablet"),
        (summary.changed_theirs, "changed on origin"),
        (summary.identical, "changed identically on both"),
        (summary.added, "added"),
        (summary.deleted, "deleted"),
        (summary.kept, "kept over a deletion"),
        (summary.merged, "merged"),
        (summary.forked, "forked"),
        (summary.resurrected, "restored from history"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect();
    (!parts.is_empty()).then(|| format!("Documents: {}", parts.join(", ")))
}

fn place(parent: &str) -> &str {
    match parent {
        "" => "the root",
        "trash" => "the trash",
        other => other,
    }
}

/// What a note says, without its document.
pub fn note_text(kind: &NoteKind) -> String {
    match kind {
        NoteKind::MetadataConflict { field } => {
            format!("\"{field}\" changed on both sides; kept origin's value")
        }
        NoteKind::KeptOverDeletion { deleted_by } => {
            format!("kept over a deletion: deleted on {deleted_by} but changed on the other side")
        }
        NoteKind::Forked { copy } => {
            format!("changed on both sides; the tablet's version is now the conflict copy {copy}")
        }
        NoteKind::ForkAlreadyPresent { copy } => {
            format!("its conflict copy {copy} already exists")
        }
        NoteKind::Opaque => "unknown .content format; compared as a whole".to_string(),
        NoteKind::RestoredFromTrash { parent } => format!(
            "restored from the trash to {}, so an edit is not lost",
            place(parent)
        ),
        NoteKind::Resurrected => "restored from history to hold a present item".to_string(),
        NoteKind::OrphanToRoot { missing_parent } => {
            format!("its folder {missing_parent} is gone; moved to the root")
        }
        NoteKind::ParentNotFolder { parent } => {
            format!("its parent {parent} is not a folder; moved to the root")
        }
        NoteKind::CycleBroken { parent } => {
            format!("was on a folder cycle; moved to {}", place(parent))
        }
    }
}

fn notes(lines: &mut Vec<String>, notes: &[Note], names: &Names) {
    list(
        lines,
        "Notes",
        notes
            .iter()
            .map(|note| format!("{}: {}", label(names, &note.doc), note_text(&note.kind)))
            .collect(),
    );
}

fn forks(lines: &mut Vec<String>, title: &str, forks: &[Fork], names: &Names) {
    list(
        lines,
        title,
        forks
            .iter()
            .map(|fork| {
                format!(
                    "\"{}\" ({}), the tablet's version of {}",
                    fork.name,
                    fork.copy,
                    label(names, &fork.original)
                )
            })
            .collect(),
    );
}

fn quarantined(lines: &mut Vec<String>, docs: &BTreeMap<DocId, String>, names: &Names) {
    list(
        lines,
        "Quarantined, malformed on the tablet (left untouched until it parses again)",
        docs.iter()
            .map(|(doc, reason)| format!("{}: {reason}", label(names, doc)))
            .collect(),
    );
}

fn orphans(lines: &mut Vec<String>, docs: &[DocId]) {
    list(
        lines,
        "Files without a .metadata (left alone; often from an interrupted upload)",
        docs.iter().map(ToString::to_string).collect(),
    );
}

/// The deletion guard's lists. `applied`: after a sync, only what was
/// actually deleted counts (a skipped document was not); `None` for a
/// plan.
fn deletions(
    lines: &mut Vec<String>,
    guard: &DeletionGuard,
    names: &Names,
    applied: Option<&BTreeSet<&DocId>>,
) {
    let planned = applied.is_none();
    let done = |doc: &&DocId| applied.is_none_or(|applied| applied.contains(doc));
    let verb = match planned {
        true => "Would delete",
        false => "Deleted",
    };
    list(
        lines,
        &format!("{verb} from origin"),
        guard
            .from_origin
            .iter()
            .map(|doc| label(names, doc))
            .collect(),
    );
    list(
        lines,
        &format!("{verb} from the tablet"),
        guard
            .from_tablet
            .iter()
            .filter(done)
            .map(|doc| label(names, doc))
            .collect(),
    );
    let exempt = guard.exempt.iter().filter(done).count();
    if exempt > 0 {
        lines.push(format!(
            "{} also {} from the tablet: origin deleted them with recorded consent",
            docs(exempt),
            if planned {
                "would be deleted"
            } else {
                "deleted"
            },
        ));
    }
    if guard.needs_consent {
        lines.push(match (guard.allowed, planned) {
            (true, _) => "These deletions are allowed by --allow-deletions".to_string(),
            (false, true) => {
                "A sync would refuse these deletions without --allow-deletions".to_string()
            }
            (false, false) => "These deletions needed --allow-deletions".to_string(),
        });
    }
}

fn owner_text(owner: &Option<LockOwner>) -> String {
    owner
        .as_ref()
        .map_or_else(|| "unknown owner".to_string(), ToString::to_string)
}

fn took_over(lines: &mut Vec<String>, took_over: &Option<TakeOver>) {
    if let Some(TakeOver {
        previous: owner,
        reason,
    }) = took_over
    {
        let why = match reason {
            StaleReason::BootChanged => "the tablet rebooted since it was taken",
            StaleReason::WatchdogFired => "its watchdog had fired",
            StaleReason::Broken => "--break-lock",
        };
        lines.push(format!(
            "Took over the tablet's lock ({}): {why}",
            owner_text(owner)
        ));
    }
}

fn reconciled(lines: &mut Vec<String>, reconciled: &Option<Reconciled>, names: &Names) {
    let Some(reconciled) = reconciled else {
        return;
    };
    lines.push(match reconciled.dropped {
        true => "Reconciled an interrupted sync: its merge never reached origin; dropped".into(),
        false => format!(
            "Reconciled an interrupted sync: finished {}, {} left for the next sync",
            docs(reconciled.finished.len()),
            reconciled.kept.len()
        ),
    });
    list(
        lines,
        "Left for the next sync (changed since the interrupted sync)",
        reconciled
            .kept
            .iter()
            .map(|doc| label(names, doc))
            .collect(),
    );
}

fn warning_lines(lines: &mut Vec<String>, warnings: &[Warning]) {
    lines.extend(warnings.iter().map(|warning| format!("warning: {warning}")));
}

fn sync(report: &SyncReport, names: &Names, verb: &str) -> Vec<String> {
    let attempts = match report.push_attempts {
        0 | 1 => String::new(),
        n => format!(" (after {n} push attempts)"),
    };
    let mut lines = vec![match &report.update {
        MainUpdate::UpToDate => format!("{verb}: already in sync with origin; nothing pushed"),
        MainUpdate::Pulled => {
            format!("{verb}: took origin's changes; the tablet had none of its own to push")
        }
        MainUpdate::FastForward(commit) => format!(
            "{verb}: pushed the tablet's changes to origin ({}){attempts}",
            short(commit)
        ),
        MainUpdate::Merge(commit) => format!(
            "{verb}: merged with origin and pushed {}{attempts}",
            short(commit)
        ),
    }];
    let applied: BTreeSet<&DocId> = report.applied.iter().collect();
    lines.push(format!(
        "The tablet is at {} (its backup: {})",
        short(&report.base),
        short(&report.snapshot)
    ));
    lines.push(match report.applied.len() {
        0 => "Nothing to change on the tablet".to_string(),
        n => format!("Updated {} on the tablet", docs(n)),
    });
    lines.push(read_line(&report.read));
    if !report.inferred.is_empty() {
        lines.push(format!(
            "{} matched a version in origin's history and took it as their base",
            docs(report.inferred.len())
        ));
    }
    lines.extend(summary_line(&report.summary));
    lines.push(format!(
        "Deletions: {} from origin, {} from the tablet",
        report.deletions.from_origin.len(),
        report
            .deletions
            .from_tablet
            .iter()
            .chain(&report.deletions.exempt)
            .filter(|doc| applied.contains(doc))
            .count()
    ));
    forks(&mut lines, "Conflict copies", &report.forks, names);
    list(
        &mut lines,
        "Skipped: changed on the tablet during the sync, or would break its folder tree (the next sync picks them up)",
        report.skipped.iter().map(|doc| label(names, doc)).collect(),
    );
    quarantined(&mut lines, &report.quarantined, names);
    orphans(&mut lines, &report.orphans);
    notes(&mut lines, &report.notes, names);
    deletions(&mut lines, &report.deletions, names, Some(&applied));
    reconciled(&mut lines, &report.reconciled, names);
    took_over(&mut lines, &report.took_over);
    warning_lines(&mut lines, &report.warnings);
    lines
}

fn backup(report: &BackupReport, names: &Names) -> Vec<String> {
    let mut lines = vec![match report.pushed {
        true => format!(
            "Backed up the tablet to origin ({})",
            short(&report.snapshot)
        ),
        false => format!(
            "Origin already had this backup of the tablet ({})",
            short(&report.snapshot)
        ),
    }];
    lines.push(read_line(&report.read));
    quarantined(&mut lines, &report.quarantined, names);
    orphans(&mut lines, &report.orphans);
    took_over(&mut lines, &report.took_over);
    warning_lines(&mut lines, &report.warnings);
    lines
}

fn status(report: &StatusReport, names: &Names) -> Vec<String> {
    let mut lines = vec![
        format!("Tablet \"{}\" ({})", report.device_name, report.device_id),
        format!("Origin {} (repository {})", report.origin, report.repo_id),
        match &report.lock {
            LockState::Free => "Lock: free".to_string(),
            LockState::Held(owner) => format!("Lock: held by {}", owner_text(owner)),
        },
    ];
    if report.journal_pending {
        lines.push("An interrupted sync left a journal; the next sync reconciles it".into());
    }
    lines.push(match report.changes.len() {
        0 => "No changes on the tablet since its last sync".to_string(),
        n => format!(
            "{} on the tablet since its last sync",
            plural(n, "change", "changes")
        ),
    });
    let entries = changes(report)
        .map(|(doc, change)| format!("{:<9}{}", change_word(change), label(names, doc)))
        .collect::<Vec<_>>();
    let shown = entries.len().min(LIST_LIMIT);
    lines.extend(entries[..shown].iter().map(|entry| format!("  {entry}")));
    if entries.len() > shown {
        lines.push(format!("  ... and {} more", entries.len() - shown));
    }
    lines.push(match report.behind {
        0 => "The tablet has everything on origin".to_string(),
        n => format!(
            "Origin has {} the tablet has not seen",
            plural(n, "commit", "commits")
        ),
    });
    list(
        &mut lines,
        "Open conflict copies (rename or delete a copy to close it)",
        report
            .open_conflicts
            .iter()
            .map(|conflict| {
                format!(
                    "\"{}\" ({}), a copy of {}",
                    conflict.name,
                    conflict.copy,
                    label(names, &conflict.original)
                )
            })
            .collect(),
    );
    quarantined(&mut lines, &report.quarantined, names);
    orphans(&mut lines, &report.orphans);
    lines.extend(summary_line(&report.summary).map(|line| format!("A sync would merge: {line}")));
    forks(
        &mut lines,
        "A sync would create conflict copies",
        &report.forks,
        names,
    );
    notes(&mut lines, &report.notes, names);
    deletions(&mut lines, &report.guard, names, None);
    lines.push("(read while xochitl runs: best effort)".to_string());
    lines
}

fn dry_run(report: &DryRunReport, names: &Names) -> Vec<String> {
    let mut lines = status(&report.status, names);
    lines.pop();
    lines.push(format!(
        "A sync would write {} to the tablet and delete {} from it",
        docs(report.writes.len()),
        report.deletes.len()
    ));
    let entries: Vec<String> = report
        .writes
        .iter()
        .map(|doc| format!("write   {}", label(names, doc)))
        .chain(
            report
                .deletes
                .iter()
                .map(|doc| format!("delete  {}", label(names, doc))),
        )
        .collect();
    let shown = entries.len().min(LIST_LIMIT);
    lines.extend(entries[..shown].iter().map(|entry| format!("  {entry}")));
    if entries.len() > shown {
        lines.push(format!("  ... and {} more", entries.len() - shown));
    }
    lines.push(match report.would_push {
        true => "It would push to origin".to_string(),
        false => "It would not push anything to origin".to_string(),
    });
    lines.push("(dry run: nothing was changed; read while xochitl runs: best effort)".into());
    lines
}

fn remote(report: &RemoteReport) -> Vec<String> {
    let mut lines = vec![
        format!("Tablet \"{}\" ({})", report.device_name, report.device_id),
        format!("Repository {}", report.repo_id),
        format!("Origin {}", report.origin),
    ];
    if report.effective != report.origin {
        lines.push(format!("This run uses {} (--origin)", report.effective));
    }
    lines
}

fn set_url(report: &SetUrlReport) -> Vec<String> {
    let mut lines = vec![format!(
        "Origin of \"{}\" changed from {} to {}",
        report.remote.device_name, report.previous, report.remote.origin
    )];
    took_over(&mut lines, &report.took_over);
    warning_lines(&mut lines, &report.warnings);
    lines
}

fn detach(report: &DetachReport) -> Vec<String> {
    let mut lines = vec![format!(
        "Detached the tablet from repository {}; its documents are untouched",
        report.repo_id
    )];
    lines.extend(
        report
            .pruned
            .iter()
            .map(|reference| format!("Deleted its device ref {reference} from origin")),
    );
    lines.push("`rmu hub clone <origin>` attaches it again".to_string());
    took_over(&mut lines, &report.took_over);
    warning_lines(&mut lines, &report.warnings);
    lines
}

/// The documents an error's details name.
pub fn error_docs(error: &Error) -> Vec<DocId> {
    match error {
        Error::HubCycle(CycleError::DeletionsNeedConsent {
            from_origin,
            from_tablet,
            ..
        }) => shown(from_tablet)
            .chain(shown(from_origin))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Details printed to stderr before the error itself (never suppressed
/// by `--quiet`): what the deletion guard refused.
pub fn error_details(error: &Error, names: &Names) -> Vec<String> {
    let mut lines = Vec::new();
    if let Error::HubCycle(CycleError::DeletionsNeedConsent {
        from_origin,
        from_tablet,
        empty_snapshot,
    }) = error
    {
        if *empty_snapshot {
            lines.push(
                "The tablet looks empty, but its last sync was not: a sync would delete the library"
                    .to_string(),
            );
        }
        let labels = |docs: &[DocId]| docs.iter().map(|doc| label(names, doc)).collect();
        list(&mut lines, "Would delete from origin", labels(from_origin));
        list(
            &mut lines,
            "Would delete from the tablet",
            labels(from_tablet),
        );
        lines.push(
            "If these deletions are intended, rerun with --allow-deletions; `rmu hub sync --dry-run` shows the whole plan"
                .to_string(),
        );
    }
    lines
}
