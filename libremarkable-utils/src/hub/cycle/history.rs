//! The history index: which version of each document every ancestor of
//! origin `main` holds, built from one `rev-list --topo-order --parents`
//! and one `diff-tree` stream (each commit against its first parent,
//! `docs/` only), so a search over thousands of commits reads no tree
//! per commit. Pure: built from those two outputs, queried without I/O.
//!
//! A document's version is the id of its `docs/<uuid>` subtree. A commit
//! holds what its first parent holds, except where its own diff touches
//! `docs/<uuid>`; a root holds only what it adds.

use std::collections::{HashMap, HashSet};

use crate::error::{Error, Result};
use crate::git::{CommitChanges, CommitParents, Oid};
use crate::hub::model::DocId;

const TREE_MODE: u32 = 0o040000;

/// One document's change in one commit against its first parent: its
/// subtree before and after (`None`: absent).
pub(super) type DocStep<'a> = (&'a DocId, Option<&'a Oid>, Option<&'a Oid>);

/// A commit of a first-parent chain and the documents its diff against
/// its first parent changed, by id.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ChainStep<'a> {
    pub commit: &'a Oid,
    pub changes: Vec<DocStep<'a>>,
}

/// See the module docs.
#[derive(Debug)]
pub(super) struct HistoryIndex {
    /// Every ancestor of the tip, children before parents; the tip first.
    commits: Vec<Oid>,
    position: HashMap<Oid, usize>,
    /// Position of each commit's first parent in `commits`.
    first_parent: Vec<Option<usize>>,
    /// Whether each commit is on the tip's first-parent chain.
    on_chain: Vec<bool>,
    /// Per document, the commits whose diff touches it and its subtree
    /// there (`None`: absent), by position, newest first.
    touches: HashMap<DocId, Vec<(usize, Option<Oid>)>>,
}

/// A first-parent chain: its positions, newest first, and as a set.
struct Chain {
    order: Vec<usize>,
    members: HashSet<usize>,
}

fn incomplete(detail: String) -> Error {
    Error::GitOutput {
        command: "rev-list/diff-tree (history index)".to_string(),
        detail,
    }
}

/// The document a raw-diff path names, if it is `docs/<uuid>` itself.
fn doc_of(path: &[u8]) -> Option<DocId> {
    let rest = path.strip_prefix(b"docs/")?;
    std::str::from_utf8(rest)
        .ok()
        .filter(|name| !name.contains('/'))
        .and_then(|name| DocId::new(name).ok())
}

impl HistoryIndex {
    /// Build the index from `rev-list` (`commits`, topo order, the tip
    /// first) and the first-parent diffs of those commits.
    pub fn build(commits: &[CommitParents], changes: &[CommitChanges]) -> Result<Self> {
        let position: HashMap<Oid, usize> = commits
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.commit.clone(), index))
            .collect();
        let first_parent = commits
            .iter()
            .map(|entry| {
                entry
                    .parents
                    .first()
                    .map(|parent| {
                        position
                            .get(parent)
                            .copied()
                            .ok_or_else(|| incomplete(format!("parent {parent} is not listed")))
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let chain: HashSet<usize> =
            std::iter::successors((!commits.is_empty()).then_some(0), |&index| {
                first_parent[index]
            })
            .collect();
        let on_chain = (0..commits.len())
            .map(|index| chain.contains(&index))
            .collect();
        let touches = changes
            .iter()
            .map(|entry| {
                position
                    .get(&entry.commit)
                    .map(|&index| (index, entry))
                    .ok_or_else(|| incomplete(format!("diff of unlisted commit {}", entry.commit)))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|(index, entry)| {
                // Per path: the new subtree if any entry for it is a tree
                // now (a type change lists a deletion and an addition).
                entry
                    .changes
                    .iter()
                    .filter_map(|change| Some((doc_of(&change.path)?, change)))
                    .fold(
                        HashMap::<DocId, Option<Oid>>::new(),
                        |mut docs, (doc, change)| {
                            let now = (change.new_mode == TREE_MODE).then(|| change.new.clone());
                            let slot = docs.entry(doc).or_default();
                            if now.is_some() {
                                *slot = now;
                            }
                            docs
                        },
                    )
                    .into_iter()
                    .map(move |(doc, subtree)| (doc, (index, subtree)))
            })
            .fold(
                HashMap::<DocId, Vec<(usize, Option<Oid>)>>::new(),
                |mut touches, (doc, touch)| {
                    touches.entry(doc).or_default().push(touch);
                    touches
                },
            )
            .into_iter()
            .map(|(doc, mut list)| {
                list.sort_by_key(|(index, _)| *index);
                (doc, list)
            })
            .collect();
        Ok(Self {
            commits: commits.iter().map(|entry| entry.commit.clone()).collect(),
            position,
            first_parent,
            on_chain,
            touches,
        })
    }

    /// The commit the index was built for.
    pub fn tip(&self) -> Option<&Oid> {
        self.commits.first()
    }

    /// Whether `commit` is the tip or one of its ancestors.
    pub fn contains(&self, commit: &Oid) -> bool {
        self.position.contains_key(commit)
    }

    /// `doc`'s subtree in `commit` (`Some(None)`: absent), if the index
    /// has the commit: its own touch, else its first parent's.
    pub fn subtree_at(&self, commit: &Oid, doc: &DocId) -> Option<Option<&Oid>> {
        let start = *self.position.get(commit)?;
        let touches = self.touches.get(doc).map(Vec::as_slice).unwrap_or_default();
        Some(
            std::iter::successors(Some(start), |&index| self.first_parent[index])
                .find_map(|index| {
                    touches
                        .binary_search_by_key(&index, |(at, _)| *at)
                        .ok()
                        .map(|found| touches[found].1.as_ref())
                })
                .flatten(),
        )
    }

    /// Whether `doc` is absent from `commit` but some first-parent
    /// ancestor of it held it (`None` if the index lacks the commit).
    pub fn deleted_at(&self, commit: &Oid, doc: &DocId) -> Option<bool> {
        let start = *self.position.get(commit)?;
        let touches = self.touches.get(doc).map(Vec::as_slice).unwrap_or_default();
        let mut held = std::iter::successors(Some(start), |&index| self.first_parent[index])
            .filter_map(|index| {
                touches
                    .binary_search_by_key(&index, |(at, _)| *at)
                    .ok()
                    .map(|found| touches[found].1.is_some())
            });
        Some(held.next() == Some(false) && held.any(|present| present))
    }

    /// `doc`'s distinct versions (`None`: absent) in the order of the
    /// newest commit holding each, with that commit: the tip's version
    /// first, then each older one as `--topo-order` first meets it.
    pub fn versions(&self, doc: &DocId) -> Vec<(&Oid, Option<&Oid>)> {
        let touches = self.touches.get(doc).map(Vec::as_slice).unwrap_or_default();
        // Oldest first, so each commit's first parent is resolved before
        // it (a parent always comes after its children).
        let (held, _) = (0..self.commits.len()).rev().fold(
            (vec![None::<&Oid>; self.commits.len()], touches.len()),
            |(mut held, pending), index| {
                let own = pending
                    .checked_sub(1)
                    .filter(|&last| touches[last].0 == index);
                held[index] = match own {
                    Some(last) => touches[last].1.as_ref(),
                    None => self.first_parent[index].and_then(|parent| held[parent]),
                };
                (held, own.unwrap_or(pending))
            },
        );
        let mut seen = HashSet::new();
        held.into_iter()
            .enumerate()
            .filter(|(_, version)| seen.insert(*version))
            .map(|(index, version)| (&self.commits[index], version))
            .collect()
    }

    /// The first-parent chain from `from` (the tip or one of its
    /// ancestors; `None` if the index lacks it), newest first, every
    /// commit with the documents it changed against its first parent
    /// (none for a commit that changed no document). A document's touches
    /// on the chain, in order, are the versions it moved between: what it
    /// held before a touch is what the next older touch set (absent
    /// before the first).
    pub fn chain(&self, from: &Oid) -> Option<Vec<ChainStep<'_>>> {
        let chain = self.chain_positions(from)?;
        let mut steps = self
            .touches
            .iter()
            .flat_map(|(doc, touches)| self.steps_on(&chain, doc, touches))
            .fold(
                HashMap::<usize, Vec<DocStep<'_>>>::new(),
                |mut steps, (at, step)| {
                    steps.entry(at).or_default().push(step);
                    steps
                },
            );
        Some(
            chain
                .order
                .iter()
                .map(|at| {
                    let mut changes = steps.remove(at).unwrap_or_default();
                    changes.sort_by_key(|(doc, ..)| *doc);
                    ChainStep {
                        commit: &self.commits[*at],
                        changes,
                    }
                })
                .collect(),
        )
    }

    /// `doc`'s changes on the first-parent chain from `from`, newest
    /// first, with the commit making each (`None` if the index lacks
    /// `from`).
    pub fn doc_chain(&self, from: &Oid, doc: &DocId) -> Option<Vec<(&Oid, DocStep<'_>)>> {
        let chain = self.chain_positions(from)?;
        let touches = self.touches.get_key_value(doc);
        Some(
            touches
                .map(|(doc, touches)| self.steps_on(&chain, doc, touches))
                .into_iter()
                .flatten()
                .map(|(at, step)| (&self.commits[at], step))
                .collect(),
        )
    }

    fn chain_positions(&self, from: &Oid) -> Option<Chain> {
        let start = *self.position.get(from)?;
        let order: Vec<usize> =
            std::iter::successors(Some(start), |&index| self.first_parent[index]).collect();
        let members = order.iter().copied().collect();
        Some(Chain { order, members })
    }

    /// A document's touches on `chain` as steps, newest first.
    fn steps_on<'s>(
        &'s self,
        chain: &Chain,
        doc: &'s DocId,
        touches: &'s [(usize, Option<Oid>)],
    ) -> Vec<(usize, DocStep<'s>)> {
        let on: Vec<&(usize, Option<Oid>)> = touches
            .iter()
            .filter(|(at, _)| chain.members.contains(at))
            .collect();
        on.iter()
            .enumerate()
            .map(|(k, (at, after))| {
                let before = on.get(k + 1).and_then(|(_, subtree)| subtree.as_ref());
                (*at, (doc, before, after.as_ref()))
            })
            .filter(|(_, (_, before, after))| before != after)
            .collect()
    }

    /// `doc`'s versions along the tip's first-parent chain, newest first,
    /// one per stretch of commits holding it (`None`: absent).
    pub fn first_parent_versions(&self, doc: &DocId) -> Vec<Option<&Oid>> {
        self.touches
            .get(doc)
            .map(|touches| {
                touches
                    .iter()
                    .filter(|(index, _)| self.on_chain[*index])
                    .map(|(_, subtree)| subtree.as_ref())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RawChange;

    fn oid(n: u32) -> Oid {
        format!("{n:040x}").parse().unwrap()
    }

    fn doc(n: u32) -> DocId {
        DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
    }

    fn commit(n: u32, parents: &[u32]) -> CommitParents {
        CommitParents {
            commit: oid(n),
            parents: parents.iter().map(|&p| oid(p)).collect(),
        }
    }

    /// `docs/<doc>` set to subtree `to` (`None`: deleted), in commit `n`.
    fn touch(n: u32, changes: &[(u32, Option<u32>)]) -> CommitChanges {
        let zero: Oid = "0".repeat(40).parse().unwrap();
        CommitChanges {
            commit: oid(n),
            changes: changes
                .iter()
                .flat_map(|&(d, to)| {
                    let path = format!("docs/{}", doc(d)).into_bytes();
                    [
                        // The `docs` tree itself and a file below: ignored.
                        RawChange {
                            old_mode: TREE_MODE,
                            new_mode: TREE_MODE,
                            old: oid(1),
                            new: oid(2),
                            status: b'M',
                            path: b"docs".to_vec(),
                        },
                        RawChange {
                            old_mode: 0,
                            new_mode: 0o100644,
                            old: zero.clone(),
                            new: oid(3),
                            status: b'A',
                            path: [path.clone(), b"/metadata".to_vec()].concat(),
                        },
                        RawChange {
                            old_mode: if to.is_some() { 0 } else { TREE_MODE },
                            new_mode: if to.is_some() { TREE_MODE } else { 0 },
                            old: zero.clone(),
                            new: to.map_or(zero.clone(), oid),
                            status: if to.is_some() { b'A' } else { b'D' },
                            path,
                        },
                    ]
                })
                .collect(),
        }
    }

    /// History (children first): 9 = merge(8, 5); 8 → 7 → 1 on the first
    /// parent; 5 → 1 on the side (a merged snapshot). Document 1: added at
    /// 1 (v100), changed at 5 (v500) and at 7 (v700); 9 takes v500 back
    /// from the side. Document 2: added at 1 (v200), deleted at 8.
    fn index() -> HistoryIndex {
        let commits = [
            commit(9, &[8, 5]),
            commit(8, &[7]),
            commit(7, &[1]),
            commit(5, &[1]),
            commit(1, &[]),
        ];
        let changes = [
            touch(9, &[(1, Some(500))]),
            touch(8, &[(2, None)]),
            touch(7, &[(1, Some(700))]),
            touch(5, &[(1, Some(500))]),
            touch(1, &[(1, Some(100)), (2, Some(200))]),
        ];
        HistoryIndex::build(&commits, &changes).unwrap()
    }

    #[test]
    fn versions_come_newest_holder_first() {
        let index = index();
        assert_eq!(index.tip(), Some(&oid(9)));
        assert_eq!(
            index.versions(&doc(1)),
            vec![
                (&oid(9), Some(&oid(500))),
                (&oid(8), Some(&oid(700))),
                (&oid(1), Some(&oid(100))),
            ]
        );
        // Deleted at 8: absent at the tip; 7, before the deletion, is the
        // newest commit still holding the version 1 introduced.
        assert_eq!(
            index.versions(&doc(2)),
            vec![(&oid(9), None), (&oid(7), Some(&oid(200)))]
        );
        assert_eq!(index.versions(&doc(3)), vec![(&oid(9), None)]);
    }

    #[test]
    fn subtrees_at_any_commit() {
        let index = index();
        assert!(index.contains(&oid(5)) && !index.contains(&oid(6)));
        assert_eq!(index.subtree_at(&oid(8), &doc(1)), Some(Some(&oid(700))));
        assert_eq!(index.subtree_at(&oid(9), &doc(1)), Some(Some(&oid(500))));
        assert_eq!(index.subtree_at(&oid(5), &doc(2)), Some(Some(&oid(200))));
        assert_eq!(index.subtree_at(&oid(9), &doc(2)), Some(None));
        assert_eq!(index.subtree_at(&oid(1), &doc(3)), Some(None));
        assert_eq!(index.subtree_at(&oid(6), &doc(1)), None);
        // Document 2 was deleted at 8, so at 9 too; never at 5 or 1.
        assert_eq!(index.deleted_at(&oid(8), &doc(2)), Some(true));
        assert_eq!(index.deleted_at(&oid(9), &doc(2)), Some(true));
        assert_eq!(index.deleted_at(&oid(5), &doc(2)), Some(false));
        assert_eq!(index.deleted_at(&oid(1), &doc(3)), Some(false));
        assert_eq!(index.deleted_at(&oid(6), &doc(2)), None);
    }

    #[test]
    fn first_parent_versions_skip_side_lines() {
        let index = index();
        assert_eq!(
            index.first_parent_versions(&doc(1)),
            vec![Some(&oid(500)), Some(&oid(700)), Some(&oid(100))]
        );
        assert_eq!(
            index.first_parent_versions(&doc(2)),
            vec![None, Some(&oid(200))]
        );
        assert!(index.first_parent_versions(&doc(3)).is_empty());
    }

    type OwnedStep = (DocId, Option<Oid>, Option<Oid>);

    fn owned((doc, before, after): DocStep) -> OwnedStep {
        (doc.clone(), before.cloned(), after.cloned())
    }

    #[test]
    fn chains_list_each_commits_changes_with_before_and_after() {
        let index = index();
        let (d1, d2) = (doc(1), doc(2));
        let v = |n: u32| Some(oid(n));
        let chain: Vec<(Oid, Vec<OwnedStep>)> = index
            .chain(&oid(9))
            .unwrap()
            .into_iter()
            .map(|step| {
                let changes = step.changes.into_iter().map(owned).collect();
                (step.commit.clone(), changes)
            })
            .collect();
        // The side commit 5 is not on the chain; 9 takes v500 over v700.
        assert_eq!(
            chain,
            vec![
                (oid(9), vec![(d1.clone(), v(700), v(500))]),
                (oid(8), vec![(d2.clone(), v(200), None)]),
                (oid(7), vec![(d1.clone(), v(100), v(700))]),
                (
                    oid(1),
                    vec![(d1.clone(), None, v(100)), (d2.clone(), None, v(200))]
                ),
            ]
        );
        // From the side commit: its own chain (5, 1).
        let side: Vec<&Oid> = index
            .chain(&oid(5))
            .unwrap()
            .iter()
            .map(|step| step.commit)
            .collect();
        assert_eq!(side, vec![&oid(5), &oid(1)]);
        let doc_chain: Vec<(Oid, OwnedStep)> = index
            .doc_chain(&oid(5), &d1)
            .unwrap()
            .into_iter()
            .map(|(commit, step)| (commit.clone(), owned(step)))
            .collect();
        assert_eq!(
            doc_chain,
            vec![
                (oid(5), (d1.clone(), v(100), v(500))),
                (oid(1), (d1.clone(), None, v(100)))
            ]
        );
        assert!(index.doc_chain(&oid(9), &doc(3)).unwrap().is_empty());
        assert!(index.chain(&oid(6)).is_none());
    }

    #[test]
    fn a_type_change_counts_as_the_new_tree() {
        let zero: Oid = "0".repeat(40).parse().unwrap();
        let path = format!("docs/{}", doc(1)).into_bytes();
        let changes = [CommitChanges {
            commit: oid(1),
            changes: vec![
                RawChange {
                    old_mode: TREE_MODE,
                    new_mode: 0,
                    old: oid(7),
                    new: zero.clone(),
                    status: b'D',
                    path: path.clone(),
                },
                RawChange {
                    old_mode: 0,
                    new_mode: 0o100644,
                    old: zero.clone(),
                    new: oid(8),
                    status: b'A',
                    path,
                },
            ],
        }];
        let index = HistoryIndex::build(&[commit(1, &[])], &changes).unwrap();
        assert_eq!(index.versions(&doc(1)), vec![(&oid(1), None)]);
    }

    #[test]
    fn an_incomplete_history_is_refused() {
        assert!(HistoryIndex::build(&[commit(2, &[1])], &[]).is_err());
        assert!(HistoryIndex::build(&[commit(1, &[])], &[touch(2, &[(1, Some(3))])]).is_err());
        let empty = HistoryIndex::build(&[], &[]).unwrap();
        assert_eq!(empty.tip(), None);
        assert!(empty.versions(&doc(1)).is_empty());
    }
}
