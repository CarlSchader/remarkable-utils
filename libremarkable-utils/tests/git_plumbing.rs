//! `git.rs` against real temporary repositories. Hermetic: every git
//! process gets `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, a
//! private `HOME`/`XDG_CONFIG_HOME` and an explicit identity, so the
//! developer's git configuration never leaks in.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use libremarkable_utils::error::Error;
use libremarkable_utils::git::{
    CommitMeta, Expected, FileMode, Git, ObjectKind, Oid, Repo, Signature,
};
use libremarkable_utils::progress::{NoProgress, Progress};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MAIN: &str = "refs/heads/main";
const DEVICE_REF: &str = "refs/rmu/devices/0000aaaa";
const IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Test Author"),
    ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
    ("GIT_COMMITTER_NAME", "Test Committer"),
    ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
];

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rmu-git-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("home")).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn hermetic_env(&self) -> Vec<(&'static str, PathBuf)> {
        [
            ("GIT_CONFIG_GLOBAL", PathBuf::from("/dev/null")),
            ("GIT_CONFIG_NOSYSTEM", PathBuf::from("1")),
            ("HOME", self.path("home")),
            ("XDG_CONFIG_HOME", self.path("home")),
        ]
        .into_iter()
        .chain(IDENTITY.iter().map(|&(k, v)| (k, PathBuf::from(v))))
        .collect()
    }

    fn git(&self) -> Git {
        self.hermetic_env()
            .into_iter()
            .fold(Git::new().interactive(false), |git, (k, v)| git.env(k, v))
    }

    fn init(&self, name: &str) -> Repo {
        self.git().init_bare(&self.path(name)).unwrap()
    }

    /// A git command outside the module under test: same environment,
    /// minus anything inherited that could point at another repository.
    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(self.hermetic_env());
        cmd
    }

    fn raw(&self, git_dir: &Path, args: &[&str]) -> String {
        let output = self
            .command()
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn sig(name: &str, time: i64) -> Signature {
    Signature {
        name: name.into(),
        email: format!("{}@example.invalid", name.to_lowercase().replace(' ', "-")),
        time,
        offset_minutes: 60,
    }
}

fn meta(parents: &[&Oid], message: &str) -> CommitMeta {
    CommitMeta {
        parents: parents.iter().map(|&p| p.clone()).collect(),
        author: sig("Fake Tablet", 1_700_000_000),
        committer: sig("rmu", 1_700_000_100),
        message: message.into(),
    }
}

/// One commit of small in-memory files.
fn commit_files(repo: &Repo, parents: &[&Oid], files: &[(&str, &[u8])], message: &str) -> Oid {
    let mut writer = repo.commit_writer(&NoProgress, None).unwrap();
    files.iter().for_each(|(path, data)| {
        writer.add_bytes(*path, FileMode::Regular, data).unwrap();
    });
    writer.commit(&meta(parents, message)).unwrap()
}

/// Deterministic pseudo-random bytes, produced lazily.
struct Pattern {
    remaining: u64,
    state: u32,
}

impl Read for Pattern {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.remaining as usize);
        buf[..n].iter_mut().for_each(|b| {
            self.state = self.state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (self.state >> 16) as u8;
        });
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn pattern(len: u64, seed: u32) -> Vec<u8> {
    let mut data = Vec::new();
    Pattern {
        remaining: len,
        state: seed,
    }
    .read_to_end(&mut data)
    .unwrap();
    data
}

/// Blob paths → (mode, oid, size) of a commit, via `ls_tree_recursive`.
fn blobs(repo: &Repo, rev: &str) -> BTreeMap<Vec<u8>, (u32, Oid, Option<u64>)> {
    repo.ls_tree_recursive(rev)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == ObjectKind::Blob)
        .map(|e| (e.path, (e.mode, e.oid, e.size)))
        .collect()
}

#[derive(Default)]
struct ByteCounter(std::cell::Cell<(u64, Option<u64>, bool)>);

impl Progress for ByteCounter {
    fn step(&self, _: &str) {}
    fn bytes(&self, transferred: u64, total: Option<u64>) {
        let (_, _, done) = self.0.get();
        self.0.set((transferred, total, done));
    }
    fn bytes_done(&self) {
        let (n, total, _) = self.0.get();
        self.0.set((n, total, true));
    }
    fn finished(&self) {}
}

#[test]
fn fast_import_commit_round_trips_through_ls_tree_and_cat_file() {
    let sandbox = Sandbox::new();
    let cache = sandbox.init("cache.git");
    let big = pattern(3 * 1024 * 1024 + 17, 7);
    let progress = ByteCounter::default();
    let mut odd_paths: Vec<Vec<u8>> = vec![
        b"docs/with space/metadata".to_vec(),
        b"docs/new\nline".to_vec(),
        b"docs/tab\there \"quoted\" back\\slash".to_vec(),
        b"\"leading-quote".to_vec(),
    ];
    if cfg!(unix) {
        odd_paths.push(b"docs/non-utf8-\xff\xfe".to_vec());
    }

    let mut writer = cache
        .commit_writer(&progress, Some(big.len() as u64 + 10))
        .unwrap();
    let metadata_oid = writer
        .add_bytes(
            "docs/1111/metadata",
            FileMode::Regular,
            br#"{"visibleName":"Fabricated"}"#,
        )
        .unwrap();
    let big_oid = writer
        .add_blob(
            "docs/1111/payload.pdf",
            FileMode::Regular,
            big.len() as u64,
            &mut Pattern {
                remaining: big.len() as u64,
                state: 7,
            },
        )
        .unwrap();
    let empty_oid = writer
        .add_bytes("docs/1111/pages/empty.rm", FileMode::Regular, b"")
        .unwrap();
    writer
        .add_bytes("tool.sh", FileMode::Executable, b"#!/bin/sh\n")
        .unwrap();
    writer
        .add_bytes("link", FileMode::Symlink, b"tool.sh")
        .unwrap();
    odd_paths.iter().for_each(|path| {
        writer
            .add_bytes(path.clone(), FileMode::Regular, path)
            .unwrap();
    });
    let first = writer.commit(&meta(&[], "snapshot\n")).unwrap();
    let (transferred, total, done) = progress.0.get();
    assert_eq!(
        transferred,
        big.len() as u64 + 28 + 10 + 7 + odd_paths.iter().map(|p| p.len() as u64).sum::<u64>()
    );
    assert_eq!(total, Some(big.len() as u64 + 10));
    assert!(done);

    // No ref was written, not even fast-import's scratch branch.
    assert_eq!(sandbox.raw(cache.git_dir(), &["for-each-ref"]), "");

    let listed = blobs(&cache, first.as_str());
    assert_eq!(listed.len(), 5 + odd_paths.len());
    assert_eq!(
        listed[&b"docs/1111/payload.pdf".to_vec()],
        (0o100644, big_oid.clone(), Some(big.len() as u64))
    );
    assert_eq!(listed[&b"docs/1111/metadata".to_vec()].1, metadata_oid);
    assert_eq!(listed[&b"docs/1111/pages/empty.rm".to_vec()].1, empty_oid);
    assert_eq!(listed[&b"tool.sh".to_vec()].0, 0o100755);
    assert_eq!(listed[&b"link".to_vec()].0, 0o120000);
    odd_paths
        .iter()
        .for_each(|path| assert!(listed.contains_key(path), "{path:?}"));

    // Trees are listed too, before their contents.
    let entries = cache.ls_tree_recursive(first.as_str()).unwrap();
    let position = |p: &[u8]| entries.iter().position(|e| e.path == p).unwrap();
    assert_eq!(entries[position(b"docs")].kind, ObjectKind::Tree);
    assert_eq!(entries[position(b"docs")].size, None);
    assert!(position(b"docs/1111") < position(b"docs/1111/metadata"));

    let mut cat = cache.cat_file().unwrap();
    assert_eq!(
        cat.read(&big_oid).unwrap(),
        Some((ObjectKind::Blob, big.clone()))
    );
    odd_paths.iter().for_each(|path| {
        let oid = &listed[path].1;
        assert_eq!(
            cat.read(oid).unwrap(),
            Some((ObjectKind::Blob, path.clone()))
        );
    });
    let (kind, commit) = cat.read(&first).unwrap().unwrap();
    assert_eq!(kind, ObjectKind::Commit);
    let commit = String::from_utf8(commit).unwrap();
    assert!(!commit.contains("\nparent "), "{commit}");
    assert!(
        commit.contains("\nauthor Fake Tablet <fake-tablet@example.invalid> 1700000000 +0100\n")
    );
    assert!(commit.contains("\ncommitter rmu <rmu@example.invalid> 1700000100 +0100\n"));
    assert!(commit.ends_with("\n\nsnapshot\n"));
    cat.finish().unwrap();

    // Carry forward: a second commit reusing existing blobs by id.
    let mut writer = cache.commit_writer(&NoProgress, None).unwrap();
    listed
        .iter()
        .filter(|(path, _)| path.as_slice() != b"docs/1111/metadata")
        .for_each(|(path, (mode, oid, _))| {
            writer
                .add_existing(path.clone(), FileMode::from_bits(*mode).unwrap(), oid)
                .unwrap();
        });
    let new_metadata = writer
        .add_bytes(
            "docs/1111/metadata",
            FileMode::Regular,
            br#"{"visibleName":"Renamed"}"#,
        )
        .unwrap();
    let second = writer.commit(&meta(&[&first], "rename\n")).unwrap();
    let relisted = blobs(&cache, second.as_str());
    assert_eq!(relisted.len(), listed.len());
    assert_eq!(relisted[&b"docs/1111/metadata".to_vec()].1, new_metadata);
    assert_eq!(relisted[&b"docs/1111/payload.pdf".to_vec()].1, big_oid);
    assert_eq!(
        cache.rev_parse(&format!("{second}^")).unwrap(),
        Some(first.clone())
    );
    assert!(cache.is_ancestor(&first, &second).unwrap());
    assert!(!cache.is_ancestor(&second, &first).unwrap());
    assert!(cache.is_ancestor(&first, &first).unwrap());
    assert_eq!(sandbox.raw(cache.git_dir(), &["for-each-ref"]), "");
}

#[test]
fn empty_tree_and_merge_commits() {
    let sandbox = Sandbox::new();
    let cache = sandbox.init("cache.git");
    let empty = commit_files(&cache, &[], &[], "empty\n");
    assert_eq!(
        cache.rev_parse(&format!("{empty}^{{tree}}")).unwrap(),
        Some(EMPTY_TREE.parse().unwrap())
    );
    assert_eq!(cache.ls_tree_recursive(empty.as_str()).unwrap(), vec![]);

    // A full tree replaces the parent's: files absent from it are gone.
    let base = commit_files(&cache, &[&empty], &[("a", b"1"), ("b", b"2")], "base\n");
    let ours = commit_files(&cache, &[&base], &[("a", b"ours")], "ours\n");
    let theirs = commit_files(
        &cache,
        &[&base],
        &[("a", b"1"), ("b", b"2"), ("c", b"3")],
        "theirs\n",
    );
    let merge = commit_files(
        &cache,
        &[&theirs, &ours],
        &[("a", b"ours"), ("c", b"3")],
        "merge\n",
    );

    assert_eq!(
        blobs(&cache, merge.as_str())
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![b"a".to_vec(), b"c".to_vec()]
    );
    let raw = sandbox.raw(cache.git_dir(), &["cat-file", "commit", merge.as_str()]);
    assert!(
        raw.contains(&format!("\nparent {theirs}\nparent {ours}\n")),
        "{raw}"
    );
    assert_eq!(
        cache.rev_parse(&format!("{merge}^1")).unwrap(),
        Some(theirs.clone())
    );
    assert!(cache.is_ancestor(&ours, &merge).unwrap());
    assert!(cache.is_ancestor(&theirs, &merge).unwrap());
    assert!(!cache.is_ancestor(&ours, &theirs).unwrap());
    // A missing object is an error, not "not an ancestor".
    let absent: Oid = "1234567890123456789012345678901234567890".parse().unwrap();
    assert!(matches!(
        cache.is_ancestor(&absent, &merge),
        Err(Error::Git { .. })
    ));
}

#[test]
fn cat_file_streams_many_blobs_through_one_process() {
    let sandbox = Sandbox::new();
    let cache = sandbox.init("cache.git");
    let contents = (0..300u32)
        .map(|i| pattern(u64::from(i * 37 % 5000), i))
        .collect::<Vec<_>>();
    let mut writer = cache.commit_writer(&NoProgress, None).unwrap();
    let oids = contents
        .iter()
        .enumerate()
        .map(|(i, data)| {
            writer
                .add_bytes(format!("blobs/{i}"), FileMode::Regular, data)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let big = pattern(1024 * 1024, 99);
    let big_oid = writer.add_bytes("big", FileMode::Regular, &big).unwrap();
    let commit = writer.commit(&meta(&[], "many\n")).unwrap();

    let mut cat = cache.cat_file().unwrap();
    oids.iter().zip(&contents).for_each(|(oid, data)| {
        assert_eq!(
            cat.read(oid).unwrap(),
            Some((ObjectKind::Blob, data.clone()))
        );
    });

    // Partial and unread objects are skipped by the next request.
    let mut object = cat.object(&big_oid).unwrap().unwrap();
    assert_eq!(
        (object.kind, object.size),
        (ObjectKind::Blob, big.len() as u64)
    );
    let mut head = [0u8; 5];
    object.read_exact(&mut head).unwrap();
    assert_eq!(head, big[..5]);
    assert!(cat.object(&oids[1]).unwrap().is_some());
    assert_eq!(cat.read(&oids[2]).unwrap().unwrap().1, contents[2]);

    // Streaming the whole big blob in small reads.
    let mut object = cat.object(&big_oid).unwrap().unwrap();
    let mut streamed = Vec::new();
    let mut chunk = [0u8; 1000];
    loop {
        match object.read(&mut chunk).unwrap() {
            0 => break,
            n => streamed.extend_from_slice(&chunk[..n]),
        }
    }
    assert_eq!(streamed, big);

    let missing: Oid = "0000000000000000000000000000000000000001".parse().unwrap();
    assert!(cat.object(&missing).unwrap().is_none());
    assert_eq!(cat.read(&oids[299]).unwrap().unwrap().1, contents[299]);
    let tree = cache
        .rev_parse(&format!("{commit}^{{tree}}"))
        .unwrap()
        .unwrap();
    assert_eq!(cat.object(&tree).unwrap().unwrap().kind, ObjectKind::Tree);
    cat.finish().unwrap();
}

#[test]
fn commit_writer_rejects_bad_input_without_writing_anything() {
    let sandbox = Sandbox::new();
    let cache = sandbox.init("cache.git");

    // A reader shorter or longer than declared.
    [(10u64, 9u64), (10, 11)]
        .iter()
        .for_each(|&(declared, actual)| {
            let mut writer = cache.commit_writer(&NoProgress, None).unwrap();
            writer.add_bytes("ok", FileMode::Regular, b"fine").unwrap();
            let data = vec![1u8; actual as usize];
            let result = writer.add_blob(
                "docs/x/payload.pdf",
                FileMode::Regular,
                declared,
                &mut &data[..],
            );
            assert!(
                matches!(&result, Err(Error::BlobSizeMismatch { path, expected, actual: got })
                if path == "docs/x/payload.pdf" && *expected == declared && *got == actual),
                "{result:?}"
            );
            // The aborted writer reports errors, never panics.
            assert!(matches!(
                writer.add_bytes("more", FileMode::Regular, b"x"),
                Err(Error::GitOutput { .. })
            ));
            assert!(matches!(
                writer.commit(&meta(&[], "x\n")),
                Err(Error::GitOutput { .. })
            ));
        });

    let mut writer = cache.commit_writer(&NoProgress, None).unwrap();
    ["", "a//b", "../x", ".git/hooks/pre-push", "a\0b"]
        .iter()
        .for_each(|path| {
            assert!(matches!(
                writer.add_bytes(path.as_bytes(), FileMode::Regular, b""),
                Err(Error::InvalidGitInput(_))
            ));
        });
    writer.add_bytes("a", FileMode::Regular, b"file").unwrap();
    writer
        .add_bytes("a/b", FileMode::Regular, b"nested")
        .unwrap();
    assert!(matches!(
        writer.commit(&meta(&[], "x\n")),
        Err(Error::InvalidGitInput(_))
    ));

    let mut writer = cache.commit_writer(&NoProgress, None).unwrap();
    writer.add_bytes("a", FileMode::Regular, b"1").unwrap();
    let mut bad = meta(&[], "x\n");
    bad.author.name = "Evil <name>".into();
    assert!(matches!(
        writer.commit(&bad),
        Err(Error::InvalidGitInput(_))
    ));

    // Referencing a blob or parent the repository lacks fails in git.
    let absent: Oid = "1234567890123456789012345678901234567890".parse().unwrap();
    let mut writer = cache.commit_writer(&NoProgress, None).unwrap();
    writer
        .add_existing("gone", FileMode::Regular, &absent)
        .unwrap();
    let result = writer.commit(&meta(&[], "x\n"));
    assert!(
        matches!(&result, Err(Error::Git { command, .. }) if command == "fast-import"),
        "{result:?}"
    );
    let writer = cache.commit_writer(&NoProgress, None).unwrap();
    assert!(matches!(
        writer.commit(&meta(&[&absent], "x\n")),
        Err(Error::Git { .. })
    ));

    assert_eq!(sandbox.raw(cache.git_dir(), &["for-each-ref"]), "");
    // The cache is still usable.
    commit_files(&cache, &[], &[("a", b"1")], "after\n");
}

#[test]
fn fetch_and_fast_forward_push() {
    let sandbox = Sandbox::new();
    let origin = sandbox.init("origin.git");
    let url = origin.git_dir().to_str().unwrap();
    let a = sandbox.init("a.git");
    let b = sandbox.init("b.git");
    assert_eq!(a.ls_remote(url).unwrap(), vec![]);

    let c1 = commit_files(&a, &[], &[("docs/x/metadata", b"{}")], "c1\n");
    a.push(url, c1.as_str(), MAIN, None).unwrap();
    // Pushing the same value again is a no-op success.
    a.push(url, c1.as_str(), MAIN, None).unwrap();
    let advertised = b.ls_remote(url).unwrap();
    assert!(
        advertised.iter().any(|r| r.name == MAIN && r.oid == c1),
        "{advertised:?}"
    );

    b.fetch(url, &["+refs/heads/main:refs/remotes/origin/main"])
        .unwrap();
    assert_eq!(
        b.rev_parse("refs/remotes/origin/main").unwrap(),
        Some(c1.clone())
    );
    assert_eq!(blobs(&b, "refs/remotes/origin/main").len(), 1);

    let c2 = commit_files(&b, &[&c1], &[("docs/x/metadata", b"{\"v\":2}")], "c2\n");
    b.push(url, c2.as_str(), MAIN, None).unwrap();
    assert_eq!(origin.rev_parse(MAIN).unwrap(), Some(c2.clone()));

    a.fetch(url, &["+refs/heads/main:refs/remotes/origin/main"])
        .unwrap();
    assert_eq!(a.rev_parse("refs/remotes/origin/main").unwrap(), Some(c2));
}

#[test]
fn concurrent_writer_causes_typed_push_rejection() {
    let sandbox = Sandbox::new();
    let origin = sandbox.init("origin.git");
    let url = origin.git_dir().to_str().unwrap();
    let a = sandbox.init("a.git");
    let b = sandbox.init("b.git");
    let base = commit_files(&a, &[], &[("f", b"base")], "base\n");
    a.push(url, base.as_str(), MAIN, None).unwrap();
    b.fetch(url, &["+refs/heads/main:refs/remotes/origin/main"])
        .unwrap();

    let ours = commit_files(&a, &[&base], &[("f", b"a")], "a\n");
    let theirs = commit_files(&b, &[&base], &[("f", b"b")], "b\n");
    a.push(url, ours.as_str(), MAIN, None).unwrap();

    // b has not seen a's commit yet...
    let result = b.push(url, theirs.as_str(), MAIN, None);
    assert!(
        matches!(&result, Err(Error::PushRejected { reference, reason }) if reference == MAIN && reason == "fetch first"),
        "{result:?}"
    );
    // ...and after fetching it, its own commit is still not a fast-forward.
    b.fetch(url, &["+refs/heads/main:refs/remotes/origin/main"])
        .unwrap();
    let result = b.push(url, theirs.as_str(), MAIN, None);
    assert!(
        matches!(&result, Err(Error::PushRejected { reason, .. }) if reason == "non-fast-forward"),
        "{result:?}"
    );
    assert_eq!(origin.rev_parse(MAIN).unwrap(), Some(ours.clone()));

    // Merging on top of what origin has succeeds.
    let merge = commit_files(&b, &[&ours, &theirs], &[("f", b"merged")], "merge\n");
    b.push(url, merge.as_str(), MAIN, None).unwrap();
    assert_eq!(origin.rev_parse(MAIN).unwrap(), Some(merge));
}

#[test]
fn leased_push_is_a_compare_and_swap() {
    let sandbox = Sandbox::new();
    let origin = sandbox.init("origin.git");
    let url = origin.git_dir().to_str().unwrap();
    let cache = sandbox.init("cache.git");
    let first = commit_files(&cache, &[], &[("f", b"1")], "1\n");
    let second = commit_files(&cache, &[&first], &[("f", b"2")], "2\n");
    let unrelated = commit_files(&cache, &[], &[("g", b"x")], "x\n");

    cache
        .push(url, first.as_str(), DEVICE_REF, Some(&Expected::Absent))
        .unwrap();
    let stale = cache.push(url, second.as_str(), DEVICE_REF, Some(&Expected::Absent));
    assert!(
        matches!(&stale, Err(Error::PushRejected { reference, reason }) if reference == DEVICE_REF && reason == "stale info"),
        "{stale:?}"
    );
    let wrong = cache.push(
        url,
        second.as_str(),
        DEVICE_REF,
        Some(&Expected::At(unrelated.clone())),
    );
    assert!(
        matches!(wrong, Err(Error::PushRejected { .. })),
        "{wrong:?}"
    );
    assert_eq!(origin.rev_parse(DEVICE_REF).unwrap(), Some(first.clone()));

    cache
        .push(
            url,
            second.as_str(),
            DEVICE_REF,
            Some(&Expected::At(first.clone())),
        )
        .unwrap();
    // A matching lease may rewind (not a fast-forward)...
    cache
        .push(
            url,
            unrelated.as_str(),
            DEVICE_REF,
            Some(&Expected::At(second.clone())),
        )
        .unwrap();
    assert_eq!(
        origin.rev_parse(DEVICE_REF).unwrap(),
        Some(unrelated.clone())
    );
    // ...which a plain push may not.
    let plain = cache.push(url, first.as_str(), DEVICE_REF, None);
    assert!(
        matches!(plain, Err(Error::PushRejected { .. })),
        "{plain:?}"
    );

    assert!(matches!(
        cache.push(url, first.as_str(), "main", None),
        Err(Error::InvalidGitInput(_))
    ));
}

#[test]
fn remote_refusal_and_unreachable_remote_are_distinct_errors() {
    let sandbox = Sandbox::new();
    let origin = sandbox.init("origin.git");
    let url = origin.git_dir().to_str().unwrap();
    let cache = sandbox.init("cache.git");
    let commit = commit_files(&cache, &[], &[("docs/x/payload.pdf", b"%PDF")], "big\n");

    fs::create_dir_all(origin.git_dir().join("hooks")).unwrap();
    let hook = origin.git_dir().join("hooks/pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'file too large: docs/x/payload.pdf' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let result = cache.push(url, commit.as_str(), MAIN, None);
    assert!(
        matches!(&result, Err(Error::PushFailed { reference, reason, stderr })
            if reference == MAIN
                && reason == "[remote rejected] (pre-receive hook declined)"
                && stderr.contains("file too large: docs/x/payload.pdf")),
        "{result:?}"
    );
    assert_eq!(origin.rev_parse(MAIN).unwrap(), None);

    let missing = sandbox.path("missing.git");
    let missing = missing.to_str().unwrap();
    assert!(
        matches!(cache.ls_remote(missing), Err(Error::Git { command, .. }) if command == "ls-remote")
    );
    assert!(
        matches!(cache.fetch(missing, &["+refs/heads/main:refs/x"]), Err(Error::Git { command, .. }) if command == "fetch")
    );
    let push = cache.push(missing, commit.as_str(), MAIN, None);
    assert!(
        matches!(&push, Err(Error::Git { command, status, stderr })
        if command == "push" && *status != 0 && !stderr.is_empty()),
        "{push:?}"
    );
}

#[test]
fn rmu_never_runs_hooks_in_its_own_repositories() {
    let sandbox = Sandbox::new();
    let origin = sandbox.init("origin.git");
    let url = origin.git_dir().to_str().unwrap();
    let cache = sandbox.init("cache.git");
    let hooks = cache.git_dir().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    ["pre-push", "reference-transaction"]
        .iter()
        .for_each(|name| {
            let hook = hooks.join(name);
            fs::write(&hook, "#!/bin/sh\necho hook ran >&2\nexit 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            }
        });
    let commit = commit_files(&cache, &[], &[("f", b"1")], "1\n");
    cache.update_ref("refs/heads/main", &commit, None).unwrap();
    cache.push(url, MAIN, MAIN, None).unwrap();
    cache
        .fetch(url, &["+refs/heads/main:refs/remotes/origin/main"])
        .unwrap();
    assert_eq!(origin.rev_parse(MAIN).unwrap(), Some(commit));
}

#[test]
fn update_ref_checks_the_old_value() {
    let sandbox = Sandbox::new();
    let cache = sandbox.init("cache.git");
    let one = commit_files(&cache, &[], &[("f", b"1")], "1\n");
    let two = commit_files(&cache, &[&one], &[("f", b"2")], "2\n");
    let name = "refs/rmu/test";

    assert_eq!(cache.rev_parse(name).unwrap(), None);
    cache
        .update_ref(name, &one, Some(&Expected::Absent))
        .unwrap();
    assert!(matches!(
        cache.update_ref(name, &two, Some(&Expected::Absent)),
        Err(Error::Git { command, .. }) if command == "update-ref"
    ));
    assert!(
        cache
            .update_ref(name, &two, Some(&Expected::At(two.clone())))
            .is_err()
    );
    assert_eq!(cache.rev_parse(name).unwrap(), Some(one.clone()));
    cache
        .update_ref(name, &two, Some(&Expected::At(one.clone())))
        .unwrap();
    assert_eq!(cache.rev_parse(name).unwrap(), Some(two));
    cache.update_ref(name, &one, None).unwrap();
    assert_eq!(cache.rev_parse(name).unwrap(), Some(one));
}

#[test]
fn open_accepts_only_bare_repositories() {
    let sandbox = Sandbox::new();
    let created = sandbox.init("cache.git");
    let opened = sandbox.git().open(created.git_dir()).unwrap();
    let commit = commit_files(&opened, &[], &[], "x\n");
    assert!(
        created
            .rev_parse(&format!("{commit}^{{commit}}"))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        sandbox.raw(created.git_dir(), &["symbolic-ref", "HEAD"]),
        "refs/heads/main\n"
    );

    assert!(matches!(
        sandbox.git().open(&sandbox.path("nowhere")),
        Err(Error::Git { command, .. }) if command == "rev-parse"
    ));
    let worktree = sandbox.path("worktree");
    let output = sandbox
        .command()
        .args(["init", "--quiet"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(matches!(
        sandbox.git().open(&worktree.join(".git")),
        Err(Error::NotABareRepository(_))
    ));
}

#[test]
fn rev_parse_resolves_or_returns_none() {
    let sandbox = Sandbox::new();
    let cache = sandbox.init("cache.git");
    assert_eq!(cache.rev_parse("refs/heads/main").unwrap(), None);
    assert_eq!(cache.rev_parse("HEAD").unwrap(), None);
    let commit = commit_files(&cache, &[], &[], "x\n");
    cache.update_ref(MAIN, &commit, None).unwrap();
    assert_eq!(cache.rev_parse("HEAD").unwrap(), Some(commit.clone()));
    assert_eq!(cache.rev_parse("--all").unwrap(), None);
    let absent = "1234567890123456789012345678901234567890";
    assert!(cache.rev_parse(absent).unwrap().is_some());
    assert_eq!(
        cache.rev_parse(&format!("{absent}^{{commit}}")).unwrap(),
        None
    );
}
