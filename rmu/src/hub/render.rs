//! Human and plain output of `rmu hub`. Pure: reports (and the names
//! looked up for their documents) in, lines out.
//!
//! The human summary is concise: counts for routine work, and lists
//! (capped at [`LIST_LIMIT`] entries) only for what needs attention:
//! conflict copies, skipped and quarantined documents, notes, deletions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use indicatif::HumanBytes;
use libremarkable_utils::Error;
use libremarkable_utils::git::Oid;
use libremarkable_utils::hub::cycle::{
    self, AddReport, BackupReport, CommitKind, CycleError, DetachReport, DocChange, DocEvent,
    DocLogReport, DryRunReport, LogEntry, LogReport, MainUpdate, Place, ReadSummary, Reconciled,
    RemoteReport, RestoreReport, SetUrlReport, StatusReport, SyncReport, TabletBase, TakeOver,
    Warning,
};
use libremarkable_utils::hub::device::{LockOwner, LockState, StaleReason};
use libremarkable_utils::hub::merge::{DeletionGuard, Fork, Note, NoteKind, Summary};
use libremarkable_utils::hub::model::{DocId, RepoId};
use libremarkable_utils::ssh::shell_quote;

use super::Report;

/// Visible names of documents, as far as they could be looked up.
pub type Names = BTreeMap<DocId, String>;

/// Entries shown per list; the rest are counted.
pub const LIST_LIMIT: usize = 20;

/// Up to this many documents written to the tablet are listed by name
/// (so a document coming back, say, does not go unnoticed); more are
/// counted.
pub const NAMED_UPDATES: usize = 10;

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

/// Entries indented and capped at [`LIST_LIMIT`], the rest counted.
fn capped(entries: Vec<String>) -> Vec<String> {
    let more = entries.len().saturating_sub(LIST_LIMIT);
    entries
        .into_iter()
        .take(LIST_LIMIT)
        .map(|entry| format!("  {entry}"))
        .chain((more > 0).then(|| format!("  ... and {more} more")))
        .collect()
}

/// A titled list: `Title (n):` and indented entries, capped.
fn list(lines: &mut Vec<String>, title: &str, entries: Vec<String>) {
    if entries.is_empty() {
        return;
    }
    lines.push(format!("{title} ({}):", entries.len()));
    lines.extend(capped(entries));
}

/// Control characters escaped (`\u{1b}`, `\n`, ...): names and reasons
/// arrive from other tablets through origin, and must not drive the
/// terminal.
pub fn escape_controls(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c.is_control() {
            true => c.escape_default().collect::<Vec<_>>(),
            false => vec![c],
        })
        .collect()
}

/// The documents of a list the summary shows (named first).
fn shown<'a>(docs: impl IntoIterator<Item = &'a DocId>) -> impl Iterator<Item = &'a DocId> {
    docs.into_iter().take(LIST_LIMIT)
}

/// The names a report carries from origin's or the tablet's history
/// (for documents the tablet no longer has): preferred over the
/// tablet's own `.metadata`.
pub fn report_names(report: &Report) -> Names {
    match report {
        Report::Clone(report) | Report::Sync(report) => report.names.clone(),
        Report::Status(report) => report.names.clone(),
        Report::DryRun(report) => report.status.names.clone(),
        Report::Init { .. }
        | Report::Backup(_)
        | Report::Remote(_)
        | Report::SetUrl(_)
        | Report::Detach(_)
        | Report::Log(_)
        | Report::DocLog(_)
        | Report::Restore(_)
        | Report::Add(_) => Names::new(),
    }
}

/// The names an error carries (the deletion guard's documents).
pub fn error_names(error: &Error) -> Names {
    match error {
        Error::HubCycle(CycleError::DeletionsNeedConsent { names, .. }) => names.clone(),
        _ => Names::new(),
    }
}

/// The documents whose names the human summary shows.
pub fn named_docs(report: &Report) -> Vec<DocId> {
    let docs: Vec<&DocId> = match report {
        // The history commands carry their own names (from origin).
        Report::Init { .. }
        | Report::Remote(_)
        | Report::SetUrl(_)
        | Report::Detach(_)
        | Report::Log(_)
        | Report::DocLog(_)
        | Report::Restore(_)
        | Report::Add(_) => Vec::new(),
        Report::Clone(report) | Report::Sync(report) => {
            let updates = match report.applied.len() <= NAMED_UPDATES {
                true => report.applied.as_slice(),
                false => &[],
            };
            shown(report.forks.iter().map(|f| &f.original))
                .chain(updates)
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

/// `preferred` names over `fallback` ones.
fn merged(preferred: Names, fallback: &Names) -> Names {
    fallback.clone().into_iter().chain(preferred).collect()
}

/// The human summary of a report. `names` (the tablet's own) fill in for
/// documents the report carries no name for.
pub fn human(report: &Report, names: &Names) -> Vec<String> {
    let names = &merged(report_names(report), names);
    let lines = match report {
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
        Report::Log(report) => log(report),
        Report::DocLog(report) => doc_log(report),
        Report::Restore(report) => restore(report),
        Report::Add(report) => add(report),
    };
    lines.iter().map(|line| escape_controls(line)).collect()
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
        Report::Log(report) => report
            .entries
            .iter()
            .map(|e| e.commit.to_string())
            .collect(),
        Report::DocLog(report) => report
            .entries
            .iter()
            .map(|e| e.commit.to_string())
            .collect(),
        Report::Restore(report) => vec![report.copy.to_string()],
        Report::Add(report) => vec![report.doc.to_string()],
    }
}

/// Warnings, for stderr when the result is JSON.
pub fn warnings(report: &Report) -> Vec<String> {
    let warnings: &[Warning] = match report {
        Report::Clone(report) | Report::Sync(report) => &report.warnings,
        Report::Backup(report) => &report.warnings,
        Report::SetUrl(report) => &report.warnings,
        Report::Detach(report) => &report.warnings,
        Report::Init { .. }
        | Report::DryRun(_)
        | Report::Status(_)
        | Report::Remote(_)
        | Report::Log(_)
        | Report::DocLog(_)
        | Report::Restore(_)
        | Report::Add(_) => &[],
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

/// What reading the tablet took; nothing when there is nothing to say.
fn read_lines(read: &ReadSummary) -> Vec<String> {
    let downloaded = (read.downloaded.files > 0).then(|| {
        format!(
            "{} ({}) read while the tablet's UI was down",
            plural(read.downloaded.files, "file", "files"),
            HumanBytes(read.downloaded.bytes)
        )
    });
    let unmirrored = (read.unmirrored > 0).then(|| {
        format!(
            "{} on the tablet but not mirrored (thumbnails, caches, unknown files)",
            plural(read.unmirrored, "file stays", "files stay")
        )
    });
    downloaded.into_iter().chain(unmirrored).collect()
}

/// The merge's resolutions, without the unchanged documents.
/// `forks`: the conflict copies made (the summary counts each fork's
/// original and copy).
fn summary_line(summary: &Summary, forks: usize) -> Option<String> {
    let parts: Vec<String> = [
        (summary.changed_ours, "changed on the tablet"),
        (summary.changed_theirs, "changed on origin"),
        (summary.identical, "changed identically on both"),
        (summary.added, "added"),
        (summary.deleted, "deleted"),
        (summary.kept, "kept over a deletion"),
        (summary.merged, "merged"),
        (forks, "forked"),
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
        NoteKind::ReturnedAfterDeletion => {
            "came back: it was deleted earlier, but another tablet changed or restored it since"
                .to_string()
        }
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
        .map_or_else(|| "an unknown owner".to_string(), ToString::to_string)
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
    match report.applied.len() {
        0 => lines.push("Nothing to change on the tablet".to_string()),
        n if n <= NAMED_UPDATES => list(
            &mut lines,
            "Updated on the tablet",
            report.applied.iter().map(|doc| label(names, doc)).collect(),
        ),
        n => lines.push(format!("Updated {} on the tablet", docs(n))),
    }
    lines.extend(read_lines(&report.read));
    if !report.inferred.is_empty() {
        lines.push(format!(
            "{} matched a version in origin's history and took it as their base",
            docs(report.inferred.len())
        ));
    }
    lines.extend(summary_line(&report.summary, report.forks.len()));
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
    if !report.forks.is_empty() {
        lines.push(
            "Compare each copy with its original; rename or delete the copy to close it".into(),
        );
    }
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
    lines.extend(read_lines(&report.read));
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
            LockState::Held(owner) => format!(
                "Lock: held by {}; if that run is gone (its computer crashed or was closed), \
                 `rmu hub sync --break-lock` takes the lock over",
                owner_text(owner)
            ),
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
    lines.extend(capped(entries));
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
    lines.extend(
        summary_line(&report.summary, report.forks.len())
            .map(|line| format!("A sync would merge: {line}")),
    );
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
    lines.extend(capped(entries));
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

// ---- log, restore, add ---------------------------------------------------------

/// Names inline in a history entry: at most this many per list.
pub const INLINE_NAMES: usize = 5;

/// A time for humans: `yyyy-mm-dd HH:MM` (UTC).
pub fn when(time: i64) -> String {
    let seconds = time.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}",
        cycle::civil_date(time),
        seconds / 3600,
        seconds % 3600 / 60
    )
}

fn kind_word(kind: CommitKind) -> &'static str {
    match kind {
        CommitKind::Init => "init",
        CommitKind::Sync | CommitKind::Snapshot => "sync",
        CommitKind::Restore => "restore",
        CommitKind::Add => "add",
        CommitKind::Other => "commit",
    }
}

/// A folder for humans: `the root`, or its path.
fn folder(place: &Place) -> String {
    match place.path.as_str() {
        "/" => "the root".to_string(),
        "trash" => "the trash".to_string(),
        path => path.to_string(),
    }
}

/// Up to [`INLINE_NAMES`] documents, labelled, the rest counted.
fn inline(names: &Names, docs: &[DocId]) -> String {
    let shown: Vec<String> = docs
        .iter()
        .take(INLINE_NAMES)
        .map(|doc| label(names, doc))
        .collect();
    match docs.len().saturating_sub(INLINE_NAMES) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

fn tablet_line(tablet: &TabletBase, what: &str) -> String {
    match &tablet.base {
        Some(base) => format!(
            "Tablet \"{}\" has {what} as of {}",
            tablet.device_name,
            short(base)
        ),
        None => format!(
            "Tablet \"{}\" has no base for {what} yet",
            tablet.device_name
        ),
    }
}

fn log_entry(lines: &mut Vec<String>, entry: &LogEntry, tablet: Option<&TabletBase>) {
    let who = match (&entry.device, entry.kind) {
        (_, CommitKind::Other) => format!("{}: {}", entry.author, entry.subject),
        (Some(device), _) => {
            let about: Vec<&str> = [device.model.as_deref(), device.firmware.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            let name = device.name.as_deref().unwrap_or(&entry.author);
            match about.is_empty() {
                true => name.to_string(),
                false => format!("{name} ({})", about.join(", ")),
            }
        }
        (None, _) => entry.author.clone(),
    };
    let here = match tablet.and_then(|tablet| tablet.base.as_ref()) {
        Some(base) if *base == entry.commit => "  <- the tablet's base",
        _ => "",
    };
    lines.push(format!(
        "{}  {}  {:<7}  {who}{here}",
        short(&entry.commit),
        when(entry.time),
        kind_word(entry.kind)
    ));
    let counts: Vec<String> = [
        (entry.added.len(), "added"),
        (entry.changed.len(), "changed"),
        (entry.deleted.len(), "deleted"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect();
    if !counts.is_empty() {
        lines.push(format!("    {}", counts.join(", ")));
    }
    let names = &entry.names;
    if !entry.added.is_empty() && entry.added.len() <= INLINE_NAMES {
        lines.push(format!("    added: {}", inline(names, &entry.added)));
    }
    if !entry.deleted.is_empty() {
        lines.push(format!("    deleted: {}", inline(names, &entry.deleted)));
    }
    lines.extend(entry.forks.iter().map(|fork| {
        format!(
            "    conflict copy {} of {}",
            label(names, &fork.copy),
            label(names, &fork.original)
        )
    }));
    lines.extend(entry.restored.iter().map(|restored| {
        format!(
            "    restored {} as of {} as {}",
            restored.original,
            short(&restored.from),
            label(names, &restored.copy)
        )
    }));
    if !entry.consented.is_empty() {
        lines.push(format!(
            "    {} deleted with --allow-deletions",
            docs(entry.consented.len())
        ));
    }
    let more = entry.notes.len().saturating_sub(LIST_LIMIT);
    lines.extend(
        entry
            .notes
            .iter()
            .take(LIST_LIMIT)
            .map(|note| format!("    note: {note}")),
    );
    if more > 0 {
        lines.push(format!("    ... and {more} more notes"));
    }
}

fn log(report: &LogReport) -> Vec<String> {
    let mut lines = vec![format!(
        "History of origin {} (repository {}), newest first",
        report.origin, report.repo_id
    )];
    if let Some(tablet) = &report.tablet {
        let overrides = match tablet.overrides {
            0 => String::new(),
            n => format!(" ({} at a base of their own)", docs(n)),
        };
        lines.push(format!("{}{overrides}", tablet_line(tablet, "the library")));
    }
    report
        .entries
        .iter()
        .for_each(|entry| log_entry(&mut lines, entry, report.tablet.as_ref()));
    if report.more > 0 {
        lines.push(format!(
            "... and {} older {} (-n sets how many are shown)",
            report.more,
            if report.more == 1 { "entry" } else { "entries" }
        ));
    }
    lines
}

fn event_text(event: &DocEvent) -> String {
    let name = |name: &Option<String>| {
        name.as_deref()
            .map_or_else(|| "(no name)".to_string(), |name| format!("\"{name}\""))
    };
    match event {
        DocEvent::Added { name: named, place } => {
            format!("added as {} in {}", name(named), folder(place))
        }
        DocEvent::Changed => "changed".to_string(),
        DocEvent::Renamed { from, to } => format!("renamed from {} to {}", name(from), name(to)),
        DocEvent::Moved { from, to } => format!("moved from {} to {}", folder(from), folder(to)),
        DocEvent::Deleted => "deleted".to_string(),
        DocEvent::ForkedInto { copy } => format!("conflict copy {copy} made from it"),
        DocEvent::ForkOf { original } => format!("made as a conflict copy of {original}"),
        DocEvent::RestoredAs { copy, from } => {
            format!("its version as of {} restored as {copy}", short(from))
        }
        DocEvent::RestoredFrom { original, from } => {
            format!("restored from {original} as of {}", short(from))
        }
    }
}

fn doc_log(report: &DocLogReport) -> Vec<String> {
    let title = match &report.name {
        Some(name) => format!("\"{name}\" ({})", report.doc),
        None => report.doc.to_string(),
    };
    let mut lines = vec![match &report.place {
        Some(place) => format!("{title}, in {}", folder(place)),
        None => format!("{title}, deleted on origin"),
    }];
    lines.extend(report.entries.iter().map(|entry| {
        let events: Vec<String> = entry.events.iter().map(event_text).collect();
        format!(
            "{}  {}  {}: {}",
            short(&entry.commit),
            when(entry.time),
            entry.author,
            events.join("; ")
        )
    }));
    if let Some(tablet) = &report.tablet {
        lines.push(tablet_line(tablet, "it"));
    }
    lines.push(format!(
        "To bring a version back as a new document: rmu hub restore {} --at <commit>",
        report.doc
    ));
    lines
}

fn restore(report: &RestoreReport) -> Vec<String> {
    let from = match report.from == report.at {
        true => format!("as of {}", short(&report.from)),
        false => format!(
            "as of {}, just before its deletion (asked for {})",
            short(&report.from),
            short(&report.at)
        ),
    };
    let mut lines = vec![
        format!(
            "Restored {} {from} as \"{}\" ({}) in {}",
            report.original,
            report.name,
            report.copy,
            folder(&report.place)
        ),
        format!(
            "Pushed to origin ({}); every tablet gets it at its next sync",
            short(&report.commit)
        ),
    ];
    if report.push_attempts > 1 {
        lines.push(format!(
            "(after {} push attempts: origin moved meanwhile)",
            report.push_attempts
        ));
    }
    lines
}

fn add(report: &AddReport) -> Vec<String> {
    let pages = match report.pages {
        Some(pages) => format!(", {} (page list written)", plural(pages, "page", "pages")),
        None => ", page list left to the tablet's first open".to_string(),
    };
    let mut lines = vec![
        format!(
            "Added \"{}\" ({}) to {}: {}, {}{pages}",
            report.name,
            report.doc,
            folder(&report.place),
            report.file_type,
            HumanBytes(report.bytes)
        ),
        format!(
            "Pushed to origin ({}); every tablet gets it at its next sync",
            short(&report.commit)
        ),
    ];
    if report.push_attempts > 1 {
        lines.push(format!(
            "(after {} push attempts: origin moved meanwhile)",
            report.push_attempts
        ));
    }
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

/// What error details can draw on besides the error.
#[derive(Debug, Default)]
pub struct ErrorContext {
    pub names: Names,
    /// The tablet's repository, if its state could be read.
    pub repo_id: Option<RepoId>,
    /// Where local caches live.
    pub cache_dir: Option<PathBuf>,
}

/// Details printed to stderr before the error itself (never suppressed
/// by `--quiet`): what the deletion guard refused, and how to recover
/// from an origin that does not match the tablet.
pub fn error_details(error: &Error, context: &ErrorContext) -> Vec<String> {
    let names = merged(error_names(error), &context.names);
    let lines = match error {
        Error::HubCycle(CycleError::DeletionsNeedConsent {
            from_origin,
            from_tablet,
            empty_snapshot,
            ..
        }) => {
            let mut lines = Vec::new();
            if *empty_snapshot {
                lines.push(
                    "The tablet looks empty, but its last sync was not: a sync would delete the library"
                        .to_string(),
                );
            }
            let labels = |docs: &[DocId]| docs.iter().map(|doc| label(&names, doc)).collect();
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
            lines
        }
        Error::HubCycle(CycleError::OriginEmpty { url }) => origin_empty(url, context),
        Error::HubCycle(CycleError::DeviceRefConflict { .. }) => vec![
            "Origin's record of this tablet's backups (its device ref) was rewritten or replaced since the tablet last synced; the tablet's documents are untouched.".to_string(),
            "`rmu hub clone <origin> --reattach` gives this tablet a fresh start on origin, keeping its documents.".to_string(),
        ],
        _ => Vec::new(),
    };
    lines.iter().map(|line| escape_controls(line)).collect()
}

/// Recovery from an origin without `main`: never re-`init` first, which
/// makes a new repository and strands every tablet.
fn origin_empty(url: &str, context: &ErrorContext) -> Vec<String> {
    let cache = match (&context.cache_dir, &context.repo_id) {
        (Some(dir), Some(repo)) => {
            shell_quote(&dir.join(format!("{repo}.git")).display().to_string())
        }
        (Some(dir), None) => shell_quote(&dir.join("<repo-id>.git").display().to_string()),
        _ => "\"$XDG_CACHE_HOME/rmu/hub/<repo-id>.git\"".to_string(),
    };
    let quoted = shell_quote(url);
    vec![
        format!("Origin {url} has no main branch. To recover:"),
        "  1. Check the URL: it is the one stored on the tablet (`rmu hub remote`), or --origin."
            .to_string(),
        "  2. Restore origin from a backup, or from the local cache of the computer that synced"
            .to_string(),
        "     most recently (a bare repository; origin's main is refs/remotes/origin/main):"
            .to_string(),
        format!("       git --git-dir {cache} push {quoted} refs/remotes/origin/main:refs/heads/main"),
        "     Then restore the tablets' backups (device refs) from every computer that syncs a"
            .to_string(),
        "     tablet; each cache holds those of the tablets it syncs:".to_string(),
        format!(
            "       git --git-dir {cache} push {quoted} 'refs/remotes/origin/devices/*:refs/rmu/devices/*'"
        ),
        "     (An origin that keeps them under refs/heads/rmu/devices/*, as on forges that refuse"
            .to_string(),
        "     custom refs, takes 'refs/remotes/origin/devices/*:refs/heads/rmu/devices/*' instead."
            .to_string(),
        "     `rejected (non-fast-forward)` lines are harmless: a newer backup is already there.)"
            .to_string(),
        "     A tablet whose backups are not restored needs `rmu hub clone <origin> --reattach`."
            .to_string(),
        "  3. Only as a last resort: `rmu hub init` origin again and `rmu hub clone <origin> --reattach`"
            .to_string(),
        "     every tablet; that starts a new repository without the old history.".to_string(),
    ]
}
