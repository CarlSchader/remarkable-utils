//! What a run knows about origin (fetched `main`, device ref, pinned
//! configuration), and the tablet's bases in its history.

use std::collections::BTreeSet;

use super::CycleError;
use super::store::{self, Cache, Remote, Store};
use crate::error::Result;
use crate::git::Oid;
use crate::hub::model::{DeviceId, MirrorRules, RepoConfig, RepoTree, TabletState};

/// Everything known about origin during a run.
pub(super) struct OriginCtx {
    pub(super) url: String,
    pub(super) cache: Cache,
    pub(super) store: Store,
    pub(super) remote: Remote,
    pub(super) main: Oid,
    pub(super) config: RepoConfig,
    pub(super) rules: &'static MirrorRules,
}

impl OriginCtx {
    /// Fetch and read origin's configuration; `advertised` is an
    /// `ls-remote` just made (by [`store::open_cache`]), reused.
    pub(super) fn new(
        cache: Cache,
        url: String,
        device: Option<&DeviceId>,
        advertised: Option<Vec<crate::git::RemoteRef>>,
    ) -> Result<Self> {
        let remote = store::fetch(&cache.repo, &url, device, advertised)?;
        let main = remote
            .main
            .clone()
            .ok_or_else(|| CycleError::OriginEmpty { url: url.clone() })?;
        let store = Store::new(cache.repo.clone())?;
        let config = store.repo_config(&main)?;
        let rules = config.rules()?;
        Ok(Self {
            url,
            cache,
            store,
            remote,
            main,
            config,
            rules,
        })
    }

    /// Fetch again (under the lock, at step 8, and to resolve a lost
    /// push acknowledgement): `main` may have moved, its `.rmu/hub.json`
    /// must not have changed.
    pub(super) fn refresh(&mut self, device: &DeviceId) -> Result<()> {
        let remote = store::fetch(&self.cache.repo, &self.url, Some(device), None)?;
        let main = remote.main.clone().ok_or_else(|| CycleError::OriginEmpty {
            url: self.url.clone(),
        })?;
        // Pinned for the whole run: the snapshot was classified under
        // these rules.
        if self.store.repo_config(&main)? != self.config {
            return Err(CycleError::RepoConfigChanged.into());
        }
        self.remote = remote;
        self.main = main;
        Ok(())
    }

    /// The tablet must belong to this origin, and every base must be in
    /// its history.
    pub(super) fn check(&self, state: &TabletState) -> Result<()> {
        if state.repo_id != self.config.repo_id {
            return Err(CycleError::RepoMismatch {
                tablet: state.repo_id.clone(),
                origin: self.config.repo_id.clone(),
            }
            .into());
        }
        bases(state)
            .into_iter()
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .try_for_each(|base| {
                if self.store.is_ancestor(base, &self.main)? {
                    Ok(())
                } else {
                    Err(CycleError::OriginRewound { base: base.clone() }.into())
                }
            })
    }
}

/// The default base and every override (`None` = the empty base).
fn bases(state: &TabletState) -> Vec<Option<&Oid>> {
    std::iter::once(state.base.as_ref())
        .chain(state.base_overrides.values().map(Option::as_ref))
        .collect()
}

/// The virtual base: the default base's tree with every override
/// document at its own base version.
pub(super) fn virtual_base(
    store: &Store,
    rules: &MirrorRules,
    state: &TabletState,
) -> Result<RepoTree> {
    let tree = match &state.base {
        Some(base) => store.tree_of(rules, base)?,
        None => RepoTree::default(),
    };
    state
        .base_overrides
        .iter()
        .try_fold(tree, |mut tree, (doc, base)| {
            match base
                .as_ref()
                .map(|base| store.version_at(rules, base, doc))
                .transpose()?
                .flatten()
            {
                Some(version) => tree.docs.insert(doc.clone(), version),
                None => tree.docs.remove(doc),
            };
            Ok(tree)
        })
}
