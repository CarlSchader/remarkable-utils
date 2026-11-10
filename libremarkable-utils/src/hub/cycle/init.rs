//! `init`: give an empty origin its `.rmu/hub.json`.

use std::path::Path;

use super::store;
use super::{CycleError, Hub, InitReport, check_url, is_local_path, rmu_signature, unix_now};
use crate::error::Result;
use crate::git::{CommitMeta, Expected, FileMode, Git, Oid};
use crate::hub::model::{RepoConfig, RepoId};

impl<'a> Hub<'a> {
    /// `rmu hub init`: give an empty origin its `.rmu/hub.json` (a new repo
    /// id, the newest mirror rules) as the first commit of `main`. A local
    /// path ([`is_local_path`]) that does not exist yet becomes a new bare
    /// repository. `origin` is used as given: [`HubOptions::origin`] does
    /// not apply. On failure, whatever this call created (that new
    /// repository, the cache) is removed again.
    ///
    /// [`HubOptions::origin`]: super::HubOptions::origin
    pub fn init(&self, origin: &str) -> Result<InitReport> {
        self.progress.step("Initialising origin");
        let result = self.init_inner(origin);
        self.progress.finished();
        result
    }

    fn init_inner(&self, origin: &str) -> Result<InitReport> {
        check_url(origin)?;
        let local = Path::new(origin);
        let created = is_local_path(origin) && !local.exists();
        if created {
            self.git.init_bare(local, crate::git::ObjectFormat::Sha1)?;
        }
        let config = RepoConfig::new(RepoId::random());
        let cache_path = self.cache_dir.join(format!("{}.git", config.repo_id));
        let lock_path = self.cache_dir.join(format!("{}.lock", config.repo_id));
        let attempt = || -> Result<Oid> {
            let (cache, refs) =
                store::open_cache(&self.git, &self.cache_dir, &config.repo_id, origin)?;
            if refs.iter().any(|reference| reference.name == store::MAIN) {
                return Err(CycleError::AlreadyInitialised {
                    url: origin.to_string(),
                }
                .into());
            }
            let signature = rmu_signature(unix_now());
            let mut writer = cache.repo.commit_writer(self.progress, None)?;
            writer.add_bytes(
                crate::hub::model::REPO_CONFIG_PATH,
                FileMode::Regular,
                config.to_json().as_bytes(),
            )?;
            let commit = writer.commit(&CommitMeta {
                parents: Vec::new(),
                author: signature.clone(),
                committer: signature,
                message: "rmu hub: init\n".to_string(),
            })?;
            store::push(
                &cache.repo,
                origin,
                &commit,
                store::MAIN,
                Some(&Expected::Absent),
                store::LOCAL_MAIN,
            )?;
            Ok(commit)
        };
        attempt()
            .map(|commit| InitReport {
                config: config.clone(),
                commit,
            })
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&cache_path);
                let _ = std::fs::remove_file(&lock_path);
                if created {
                    remove_if_uninitialised(&self.git, local);
                }
            })
    }
}

/// Remove the repository `init` created at `path` after it failed, unless
/// it has a `main` by now: a concurrent `init` of the same new path won
/// the race, and that repository is its. (A winner whose push lands
/// between this check and the removal still loses its repository; two
/// `init`s of one new path at the same instant are not otherwise
/// guarded.)
fn remove_if_uninitialised(git: &Git, path: &Path) {
    let initialised = git
        .open(path)
        .and_then(|repo| repo.rev_parse(store::MAIN))
        .is_ok_and(|main| main.is_some());
    if !initialised {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_init_removes_only_an_uninitialised_repository() {
        let scratch = test_support::Scratch::new("init-cleanup");
        let git = Git::new()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", scratch.path());
        let (won, empty) = (
            scratch.path().join("won.git"),
            scratch.path().join("empty.git"),
        );
        let repo = git.init_bare(&won, crate::git::ObjectFormat::Sha1).unwrap();
        let mut writer = repo
            .commit_writer(&crate::progress::NoProgress, None)
            .unwrap();
        writer
            .add_bytes(
                crate::hub::model::REPO_CONFIG_PATH,
                FileMode::Regular,
                b"{}",
            )
            .unwrap();
        let signature = rmu_signature(0);
        let commit = writer
            .commit(&CommitMeta {
                parents: Vec::new(),
                author: signature.clone(),
                committer: signature,
                message: "init\n".to_string(),
            })
            .unwrap();
        repo.update_ref(store::MAIN, &commit, None).unwrap();
        git.init_bare(&empty, crate::git::ObjectFormat::Sha1)
            .unwrap();
        remove_if_uninitialised(&git, &won);
        remove_if_uninitialised(&git, &empty);
        assert!(won.exists(), "another init's repository was removed");
        assert!(!empty.exists());
    }
}
