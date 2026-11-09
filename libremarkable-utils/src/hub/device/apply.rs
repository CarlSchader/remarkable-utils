//! The apply plan and its ordering into steps. Pure: the real tablet
//! renders the steps as one shell script (`scripts::apply_script`), the
//! fake executes them on local directories, so both apply in exactly the
//! same order.

use std::collections::BTreeSet;

use super::DeviceError;
use crate::git::FileMode;
use crate::hub::model::{DocFile, DocId};

/// What to change on the tablet, per document, in order. Files to write
/// must already be staged ([`super::Tablet::stage`]) under their
/// xochitl-relative paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyPlan {
    pub docs: Vec<DocApply>,
}

/// A file to move into place, as it must have been staged: the apply
/// checks the staged size before changing anything (a short or partial
/// upload is never applied) and gives the file its mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub file: DocFile,
    pub size: u64,
    /// [`FileMode::Regular`] or [`FileMode::Executable`].
    pub mode: FileMode,
}

impl StagedFile {
    /// A regular (0644) file of `size` bytes.
    pub fn regular(file: DocFile, size: u64) -> Self {
        Self {
            file,
            size,
            mode: FileMode::Regular,
        }
    }
}

/// One staged file of a plan, by xochitl-relative path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StagedPath {
    pub path: String,
    pub size: u64,
    pub executable: bool,
}

/// One document's change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocApply {
    /// Move staged files into place and remove mirrored files the merge
    /// dropped (e.g. a deleted page). Removing `.metadata` would delete
    /// the document: use [`DocApply::Delete`].
    Write {
        doc: DocId,
        write: Vec<StagedFile>,
        remove: Vec<DocFile>,
    },
    /// Delete the whole `<uuid>`/`<uuid>.*` set, including files outside
    /// the mirror (thumbnails, `.local`).
    Delete { doc: DocId },
}

impl DocApply {
    pub fn doc(&self) -> &DocId {
        match self {
            DocApply::Write { doc, .. } | DocApply::Delete { doc } => doc,
        }
    }
}

/// One step of an apply, in execution order. Paths are relative to the
/// xochitl data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyStep {
    /// Stop unless the watchdog marker is absent and the lock is ours.
    Guard,
    /// `mkdir -p` a document's page or highlights directory.
    MakeDir(String),
    /// Rename the staged copy of this path into place.
    Move(String),
    /// Remove this file if present.
    Remove(String),
    /// Remove `<uuid>` (recursively) and every `<uuid>.*`.
    DeleteSet(DocId),
    /// The document is complete.
    Done(DocId),
}

impl ApplyStep {
    /// Steps that change the data dir's contents (what "kill after K
    /// moves" counts in the fake).
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            ApplyStep::Move(_) | ApplyStep::Remove(_) | ApplyStep::DeleteSet(_)
        )
    }
}

/// Within a document: pages and payload (everything else) first, then
/// `.content`, then `.metadata`, so an interrupted apply never leaves a
/// `.metadata` pointing at missing content.
fn phase(file: &DocFile) -> u8 {
    match file {
        DocFile::Content => 1,
        DocFile::Metadata => 2,
        _ => 0,
    }
}

/// The directory a file lives in, if it is not the data dir itself.
fn parent_dir(doc: &DocId, file: &DocFile) -> Option<String> {
    match file {
        DocFile::Page(_) | DocFile::PageMetadata(_) => Some(doc.to_string()),
        DocFile::Highlights(_) => Some(format!("{doc}.highlights")),
        _ => None,
    }
}

fn invalid(doc: &DocId, detail: &str) -> DeviceError {
    DeviceError::InvalidPlan(format!("document {doc}: {detail}"))
}

fn unique<'a>(
    doc: &DocId,
    files: impl IntoIterator<Item = &'a DocFile>,
    what: &str,
) -> Result<BTreeSet<&'a DocFile>, DeviceError> {
    files
        .into_iter()
        .try_fold(BTreeSet::new(), |mut set, file| {
            if set.insert(file) {
                Ok(set)
            } else {
                Err(invalid(
                    doc,
                    &format!("{} listed twice to {what}", file.repo_name()),
                ))
            }
        })
}

fn write_steps(
    doc: &DocId,
    write: &[StagedFile],
    remove: &[DocFile],
) -> Result<Vec<ApplyStep>, DeviceError> {
    if let Some(odd) = write.iter().find(|staged| !staged.mode.is_file()) {
        return Err(invalid(
            doc,
            &format!("{} has mode {:o}", odd.file.repo_name(), odd.mode.bits()),
        ));
    }
    let writes = unique(doc, write.iter().map(|staged| &staged.file), "write")?;
    let removes = unique(doc, remove, "remove")?;
    if writes.is_empty() && removes.is_empty() {
        return Err(invalid(doc, "nothing to write or remove"));
    }
    if removes.contains(&DocFile::Metadata) {
        return Err(invalid(
            doc,
            "removing metadata deletes the document; plan a deletion instead",
        ));
    }
    if let Some(both) = writes.intersection(&removes).next() {
        return Err(invalid(
            doc,
            &format!("{} is both written and removed", both.repo_name()),
        ));
    }
    let (_, steps) = (0..=2).fold(
        (BTreeSet::<String>::new(), vec![ApplyStep::Guard]),
        |(mut dirs, mut steps), current| {
            removes
                .iter()
                .filter(|file| phase(file) == current)
                .for_each(|file| steps.push(ApplyStep::Remove(file.xochitl_path(doc))));
            writes
                .iter()
                .filter(|file| phase(file) == current)
                .for_each(|file| {
                    if let Some(dir) = parent_dir(doc, file)
                        && dirs.insert(dir.clone())
                    {
                        steps.push(ApplyStep::MakeDir(dir));
                    }
                    steps.push(ApplyStep::Move(file.xochitl_path(doc)));
                });
            (dirs, steps)
        },
    );
    Ok(steps
        .into_iter()
        .chain(std::iter::once(ApplyStep::Done(doc.clone())))
        .collect())
}

fn delete_steps(doc: &DocId) -> Vec<ApplyStep> {
    vec![
        ApplyStep::Guard,
        // `.metadata` first: an interrupted deletion leaves invisible
        // orphan files, never a document with half its files.
        ApplyStep::Remove(DocFile::Metadata.xochitl_path(doc)),
        ApplyStep::DeleteSet(doc.clone()),
        ApplyStep::Done(doc.clone()),
    ]
}

impl ApplyPlan {
    /// The plan as ordered steps, after validating it: each document at
    /// most once, no file listed twice, nothing both written and removed.
    pub fn steps(&self) -> Result<Vec<ApplyStep>, DeviceError> {
        self.docs
            .iter()
            .try_fold(
                (BTreeSet::new(), Vec::new()),
                |(mut seen, mut steps), entry| {
                    if !seen.insert(entry.doc()) {
                        return Err(invalid(entry.doc(), "planned twice"));
                    }
                    steps.extend(match entry {
                        DocApply::Write { doc, write, remove } => write_steps(doc, write, remove)?,
                        DocApply::Delete { doc } => delete_steps(doc),
                    });
                    Ok((seen, steps))
                },
            )
            .map(|(_, steps)| steps)
    }

    /// Every file the plan writes, with its staged size and mode, sorted
    /// by path.
    pub fn staged(&self) -> Vec<StagedPath> {
        self.docs
            .iter()
            .flat_map(|entry| match entry {
                DocApply::Write { doc, write, .. } => write
                    .iter()
                    .map(|staged| StagedPath {
                        path: staged.file.xochitl_path(doc),
                        size: staged.size,
                        executable: staged.mode == FileMode::Executable,
                    })
                    .collect(),
                DocApply::Delete { .. } => Vec::new(),
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Every file the plan writes (xochitl-relative), sorted.
    pub fn written(&self) -> Vec<String> {
        self.staged()
            .into_iter()
            .map(|staged| staged.path)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::model::PageId;

    const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

    fn doc(id: &str) -> DocId {
        DocId::new(id).unwrap()
    }

    fn page(id: &str) -> DocFile {
        DocFile::Page(PageId::new(id).unwrap())
    }

    fn staged(files: Vec<DocFile>) -> Vec<StagedFile> {
        files
            .into_iter()
            .map(|file| StagedFile::regular(file, 1))
            .collect()
    }

    #[test]
    fn writes_go_pages_and_payload_then_content_then_metadata() {
        let plan = ApplyPlan {
            docs: vec![DocApply::Write {
                doc: doc(A),
                write: staged(vec![
                    DocFile::Metadata,
                    DocFile::Content,
                    page("p2"),
                    DocFile::PayloadPdf,
                    DocFile::Highlights(PageId::new("p1").unwrap()),
                    page("p1"),
                ]),
                remove: vec![page("gone"), DocFile::Pagedata],
            }],
        };
        let steps = plan.steps().unwrap();
        let s = |text: &str| text.replace("A", A);
        assert_eq!(
            steps,
            vec![
                ApplyStep::Guard,
                ApplyStep::Remove(s("A.pagedata")),
                ApplyStep::Remove(s("A/gone.rm")),
                ApplyStep::Move(s("A.pdf")),
                ApplyStep::MakeDir(s("A")),
                ApplyStep::Move(s("A/p1.rm")),
                ApplyStep::Move(s("A/p2.rm")),
                ApplyStep::MakeDir(s("A.highlights")),
                ApplyStep::Move(s("A.highlights/p1.json")),
                ApplyStep::Move(s("A.content")),
                ApplyStep::Move(s("A.metadata")),
                ApplyStep::Done(doc(A)),
            ]
        );
        assert_eq!(
            plan.written(),
            [
                "A.content",
                "A.highlights/p1.json",
                "A.metadata",
                "A.pdf",
                "A/p1.rm",
                "A/p2.rm"
            ]
            .map(s)
        );
    }

    #[test]
    fn documents_keep_plan_order_and_deletions_remove_metadata_first() {
        let plan = ApplyPlan {
            docs: vec![
                DocApply::Delete { doc: doc(B) },
                DocApply::Write {
                    doc: doc(A),
                    write: staged(vec![DocFile::Metadata]),
                    remove: vec![DocFile::Content],
                },
            ],
        };
        assert_eq!(
            plan.steps().unwrap(),
            vec![
                ApplyStep::Guard,
                ApplyStep::Remove(format!("{B}.metadata")),
                ApplyStep::DeleteSet(doc(B)),
                ApplyStep::Done(doc(B)),
                ApplyStep::Guard,
                ApplyStep::Remove(format!("{A}.content")),
                ApplyStep::Move(format!("{A}.metadata")),
                ApplyStep::Done(doc(A)),
            ]
        );
        assert_eq!(plan.written(), vec![format!("{A}.metadata")]);
        assert_eq!(
            plan.steps()
                .unwrap()
                .iter()
                .filter(|step| step.is_mutation())
                .count(),
            4
        );
    }

    #[test]
    fn invalid_plans_are_refused() {
        let write = |write: Vec<DocFile>, remove: Vec<DocFile>| ApplyPlan {
            docs: vec![DocApply::Write {
                doc: doc(A),
                write: staged(write),
                remove,
            }],
        };
        let refused = |plan: ApplyPlan| matches!(plan.steps(), Err(DeviceError::InvalidPlan(_)));
        assert!(refused(write(vec![], vec![])));
        assert!(refused(write(vec![], vec![DocFile::Metadata])));
        assert!(refused(write(vec![page("x"), page("x")], vec![])));
        assert!(refused(write(vec![], vec![page("x"), page("x")])));
        assert!(refused(write(vec![page("x")], vec![page("x")])));
        assert!(refused(ApplyPlan {
            docs: vec![
                DocApply::Delete { doc: doc(A) },
                DocApply::Delete { doc: doc(A) }
            ],
        }));
        assert_eq!(ApplyPlan::default().steps().unwrap(), vec![]);
        let symlink = ApplyPlan {
            docs: vec![DocApply::Write {
                doc: doc(A),
                write: vec![StagedFile {
                    file: DocFile::Metadata,
                    size: 1,
                    mode: FileMode::Symlink,
                }],
                remove: vec![],
            }],
        };
        assert!(refused(symlink));
    }

    #[test]
    fn staged_files_carry_size_and_mode() {
        let plan = ApplyPlan {
            docs: vec![DocApply::Write {
                doc: doc(A),
                write: vec![
                    StagedFile::regular(DocFile::Metadata, 12),
                    StagedFile {
                        file: DocFile::PayloadPdf,
                        size: 3,
                        mode: FileMode::Executable,
                    },
                ],
                remove: vec![],
            }],
        };
        assert_eq!(
            plan.staged(),
            vec![
                StagedPath {
                    path: format!("{A}.metadata"),
                    size: 12,
                    executable: false
                },
                StagedPath {
                    path: format!("{A}.pdf"),
                    size: 3,
                    executable: true
                },
            ]
        );
    }
}
