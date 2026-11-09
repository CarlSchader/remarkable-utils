//! Step 5: reading the tablet into the local cache, and building the
//! snapshot from what was read.
//!
//! While xochitl is stopped only device reads happen: one stat pass, then
//! one streaming download of every mirrored file whose stat signature is
//! not a recorded one (or whose recorded blob the cache lacks), each
//! streamed straight into `git fast-import` ([`CommitWriter::write_blob`]).
//! Validation and carry-forward are pure and run once xochitl is back.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::CycleError;
use super::plan::{LocalJson, json_kind};
use super::store::Store;
use crate::error::Result;
use crate::git::{CommitWriter, Oid};
use crate::hub::device::{StatListing, Tablet};
use crate::hub::merge::TreeItem;
use crate::hub::model::{
    self, Classification, DocFile, DocId, DocOutcome, Exclusion, ManifestEntry, MirrorRules,
    MirroredFile, ParseResult, QuarantineReason, RepoTree, SnapshotFiles, SnapshotValidation,
    StatSignature, XochitlPath,
};
use crate::progress::Progress;

/// A file whose content is known without reading it: its stat signature
/// still matches the one recorded with this blob.
#[derive(Debug, Clone)]
pub(super) struct Known {
    pub stat: StatSignature,
    pub blob: Oid,
    /// `.metadata`/`.content` bytes, if already at hand.
    pub json: Option<Vec<u8>>,
    /// The blob is in the commit being written, so the cache cannot be
    /// asked for it (a re-read in the same run); manifest blobs are
    /// checked against the cache first.
    pub trusted: bool,
}

/// The recorded manifest as [`Known`] files.
pub(super) fn known_from_manifest(
    manifest: &BTreeMap<XochitlPath, ManifestEntry>,
) -> BTreeMap<XochitlPath, Known> {
    manifest
        .iter()
        .map(|(path, entry)| {
            (
                path.clone(),
                Known {
                    stat: entry.stat,
                    blob: entry.blob.clone(),
                    json: None,
                    trusted: false,
                },
            )
        })
        .collect()
}

/// One mirrored regular file as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadFile {
    pub file: MirroredFile,
    pub stat: StatSignature,
    pub blob: Oid,
}

/// Everything step 5 read from the tablet.
#[derive(Debug, Clone, Default)]
pub(super) struct TabletRead {
    /// Mirrored regular files.
    pub files: BTreeMap<XochitlPath, ReadFile>,
    /// The bytes of every mirrored `.metadata`/`.content`.
    pub json: BTreeMap<XochitlPath, Vec<u8>>,
    /// Regular files the mirror excludes, with their stats.
    pub excluded: Excluded,
    /// Entries that are not regular files (a symlink at a mirrored name
    /// is never mirrored).
    pub irregular: Vec<String>,
    /// Names that cannot be listed (newlines).
    pub unlisted: usize,
    /// Files downloaded, and their bytes.
    pub downloaded: (usize, u64),
}

impl TabletRead {
    /// As [`Known`] files for a re-read later in the same run.
    pub fn as_known(&self) -> BTreeMap<XochitlPath, Known> {
        self.files
            .iter()
            .map(|(path, read)| {
                (
                    path.clone(),
                    Known {
                        stat: read.stat,
                        blob: read.blob.clone(),
                        json: self.json.get(path).cloned(),
                        trusted: true,
                    },
                )
            })
            .collect()
    }

    /// Mirrored files per document, by blob.
    pub fn doc_files(&self) -> BTreeMap<DocId, BTreeMap<DocFile, Oid>> {
        self.files.values().fold(
            BTreeMap::new(),
            |mut docs: BTreeMap<_, BTreeMap<_, _>>, read| {
                docs.entry(read.file.doc.clone())
                    .or_default()
                    .insert(read.file.file.clone(), read.blob.clone());
                docs
            },
        )
    }

    /// Parsed `.metadata`/`.content` per document (unparsable ones left
    /// out), for the view-state overlay.
    pub fn local_json(&self) -> BTreeMap<DocId, LocalJson> {
        self.files
            .iter()
            .filter_map(|(path, read)| {
                let kind = json_kind(&read.file.file)?;
                let value = serde_json::from_slice::<Value>(self.json.get(path)?).ok()?;
                Some((read.file.doc.clone(), kind, value))
            })
            .fold(BTreeMap::new(), |mut docs, (doc, kind, value)| {
                docs.entry(doc)
                    .or_insert_with(LocalJson::new)
                    .insert(kind, value);
                docs
            })
    }

    /// Parent and kind of every document with a readable `.metadata`.
    pub fn items(&self) -> BTreeMap<DocId, TreeItem> {
        self.local_json()
            .into_iter()
            .filter_map(|(doc, json)| {
                json.get(&model::JsonFile::Metadata)
                    .filter(|metadata| metadata.is_object())
                    .map(|metadata| (doc, TreeItem::from_metadata(metadata)))
            })
            .collect()
    }

    /// Step-5 stat and blob of every mirrored file (for the manifest).
    pub fn stats(&self) -> BTreeMap<XochitlPath, (StatSignature, Oid)> {
        self.files
            .iter()
            .map(|(path, read)| (path.clone(), (read.stat, read.blob.clone())))
            .collect()
    }
}

/// A listed file to classify.
struct Candidate {
    path: XochitlPath,
    file: MirroredFile,
    stat: StatSignature,
}

/// Regular files the mirror excludes, with why and their stats.
type Excluded = BTreeMap<String, (Exclusion, StatSignature)>;

/// Classify the stat pass: mirrored regular files, excluded regular
/// files, irregular entries.
fn classify(
    rules: &MirrorRules,
    listing: &StatListing,
) -> Result<(Vec<Candidate>, Excluded, Vec<String>)> {
    listing.entries.iter().try_fold(
        (Vec::new(), BTreeMap::new(), Vec::new()),
        |(mut mirrored, mut excluded, mut irregular), entry| {
            if !entry.is_regular() {
                irregular.push(entry.path.clone());
                return Ok((mirrored, excluded, irregular));
            }
            match rules.classify(&entry.path)? {
                Classification::Mirrored(file) => mirrored.push(Candidate {
                    path: XochitlPath::new(entry.path.as_str())?,
                    file,
                    stat: entry.stat,
                }),
                Classification::Excluded(reason) => {
                    excluded.insert(entry.path.clone(), (reason, entry.stat));
                }
            }
            Ok((mirrored, excluded, irregular))
        },
    )
}

/// How strictly to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadMode {
    /// The snapshot (xochitl stopped): any surprise aborts the run.
    Strict,
    /// While xochitl runs (a pre-read, `status`): files that change while
    /// read, or a download that fails part-way, are left out.
    BestEffort,
}

/// Pre-read files usable by the stopped read: their stat is trusted only
/// with a nanosecond mtime, since a coarse one could hide a change xochitl
/// made within the same second (see "Snapshot validation"); their blobs
/// are checked against the cache like manifest entries (the pre-read
/// commits them on its own, and may not have got that far).
pub(super) fn pre_read_known(read: &TabletRead) -> BTreeMap<XochitlPath, Known> {
    read.as_known()
        .into_iter()
        .filter(|(_, known)| known.stat.keep_in_manifest(i64::MAX))
        .map(|(path, known)| {
            (
                path,
                Known {
                    trusted: false,
                    ..known
                },
            )
        })
        .collect()
}

/// Read the tablet: one stat pass, one streaming download of every
/// mirrored file that is not [`Known`], each streamed into `writer`
/// (byte counts must match the stat pass: in [`ReadMode::Strict`] a file
/// that changed while read aborts the run), and the bytes of every
/// `.metadata`/`.content`.
pub(super) fn read_tablet(
    tablet: &dyn Tablet,
    rules: &MirrorRules,
    known: &BTreeMap<XochitlPath, Known>,
    store: &Store,
    writer: &mut CommitWriter,
    progress: &dyn Progress,
    mode: ReadMode,
) -> Result<TabletRead> {
    let listing = tablet.stat_all()?;
    let (candidates, excluded, irregular) = classify(rules, &listing)?;
    let matching: Vec<(&Candidate, &Known)> = candidates
        .iter()
        .filter_map(|candidate| {
            known
                .get(&candidate.path)
                .filter(|known| known.stat == candidate.stat)
                .map(|known| (candidate, known))
        })
        .collect();
    let present = store.repo.object_sizes(
        matching
            .iter()
            .filter(|(_, known)| !known.trusted)
            .map(|(_, known)| &known.blob),
    )?;
    let reusable: BTreeMap<&XochitlPath, &Known> = matching
        .into_iter()
        .filter(|(_, known)| known.trusted || present.contains_key(&known.blob))
        .map(|(candidate, known)| (&candidate.path, known))
        .collect();
    let wanted: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| !reusable.contains_key(&candidate.path))
        .collect();
    let sizes: BTreeMap<&XochitlPath, u64> = wanted
        .iter()
        .map(|candidate| (&candidate.path, candidate.stat.size))
        .collect();
    let total: u64 = sizes.values().sum();
    progress.step(&format!(
        "Reading {} changed file(s) from the tablet",
        wanted.len()
    ));
    let mut fetched: BTreeMap<XochitlPath, (Oid, Option<Vec<u8>>)> = BTreeMap::new();
    let mut transferred = 0u64;
    let paths: Vec<XochitlPath> = wanted
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();
    let downloaded = tablet.download(&paths, &mut |path, size, reader| {
        let expected = sizes.get(path).copied().unwrap_or_default();
        if size != expected {
            return match mode {
                // Left out; the stopped read reads it again.
                ReadMode::BestEffort => Ok(()),
                ReadMode::Strict => Err(CycleError::FileChanged {
                    path: path.to_string(),
                    expected,
                    actual: size,
                }
                .into()),
            };
        }
        let is_json = matches!(
            rules.classify(path.as_str()),
            Ok(Classification::Mirrored(file)) if json_kind(&file.file).is_some()
        );
        let entry = if is_json {
            let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            reader.read_to_end(&mut bytes)?;
            let oid = writer.write_blob(path.as_str(), size, &mut &bytes[..])?;
            (oid, Some(bytes))
        } else {
            (writer.write_blob(path.as_str(), size, reader)?, None)
        };
        transferred += size;
        progress.bytes(transferred, Some(total));
        fetched.insert(path.clone(), entry);
        Ok(())
    });
    progress.bytes_done();
    match (downloaded, mode) {
        (Ok(()), _) | (Err(_), ReadMode::BestEffort) => {}
        (Err(error), ReadMode::Strict) => return Err(error),
    }
    let (files, json) = candidates.iter().try_fold(
        (BTreeMap::new(), BTreeMap::new()),
        |(mut files, mut json), candidate| -> Result<_> {
            let (blob, bytes) = match (
                reusable.get(&candidate.path),
                fetched.remove(&candidate.path),
            ) {
                (_, Some(found)) => found,
                (Some(known), None) => (known.blob.clone(), known.json.clone()),
                (None, None) => match mode {
                    ReadMode::BestEffort => return Ok((files, json)),
                    ReadMode::Strict => {
                        return Err(CycleError::FileChanged {
                            path: candidate.path.to_string(),
                            expected: candidate.stat.size,
                            actual: 0,
                        }
                        .into());
                    }
                },
            };
            if json_kind(&candidate.file.file).is_some() {
                let bytes = match bytes {
                    Some(bytes) => bytes,
                    None => store.blob(&blob)?,
                };
                json.insert(candidate.path.clone(), bytes);
            }
            files.insert(
                candidate.path.clone(),
                ReadFile {
                    file: candidate.file.clone(),
                    stat: candidate.stat,
                    blob,
                },
            );
            Ok((files, json))
        },
    )?;
    Ok(TabletRead {
        files,
        json,
        excluded,
        irregular,
        unlisted: listing.unlisted,
        downloaded: (paths.len(), total),
    })
}

/// A validated snapshot: the tree to commit and what validation found.
#[derive(Debug, Clone)]
pub(super) struct Snapshot {
    /// The base tree with the tablet's documents carried in.
    pub tree: RepoTree,
    pub validation: SnapshotValidation,
    pub quarantined: BTreeMap<DocId, QuarantineReason>,
    pub orphans: BTreeSet<DocId>,
}

/// Validate what was read and build the snapshot from `base` (the
/// virtual base: each document at its own base version).
pub(super) fn build(rules: &MirrorRules, read: &TabletRead, base: &RepoTree) -> Result<Snapshot> {
    let files: BTreeMap<String, StatSignature> = read
        .files
        .iter()
        .map(|(path, file)| (path.to_string(), file.stat))
        .chain(
            read.excluded
                .iter()
                .map(|(path, (_, stat))| (path.clone(), *stat)),
        )
        .collect();
    let json: BTreeMap<String, ParseResult> = read
        .json
        .iter()
        .map(|(path, bytes)| {
            (
                path.to_string(),
                serde_json::from_slice::<Value>(bytes).map_err(|error| error.to_string()),
            )
        })
        .collect();
    let in_base: BTreeSet<DocId> = base
        .docs
        .iter()
        .filter(|(_, version)| version.is_present())
        .map(|(doc, _)| doc.clone())
        .collect();
    let validation = rules.validate_snapshot(&files, &json, &in_base)?;
    let snapshot_files: SnapshotFiles = read
        .files
        .values()
        .map(|file| (file.file.clone(), file.blob.clone()))
        .collect();
    let tree = model::carry_forward(base, &snapshot_files, &validation)?;
    let (quarantined, orphans) = validation.docs.iter().fold(
        (BTreeMap::new(), BTreeSet::new()),
        |(mut quarantined, mut orphans), (doc, outcome)| {
            match outcome {
                DocOutcome::Quarantined(reason) => {
                    quarantined.insert(doc.clone(), reason.clone());
                }
                DocOutcome::Orphan => {
                    orphans.insert(doc.clone());
                }
                DocOutcome::Ok => {}
            }
            (quarantined, orphans)
        },
    );
    Ok(Snapshot {
        tree,
        validation,
        quarantined,
        orphans,
    })
}
