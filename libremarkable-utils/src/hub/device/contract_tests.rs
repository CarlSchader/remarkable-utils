//! Contract scenarios, written against the [`Tablet`] trait and a small
//! [`Harness`] for what only the environment can do (the user editing a
//! file, the watchdog firing, a reboot). They run against the fake and,
//! on Linux, against the real [`SshTablet`] with a mock `ssh` that runs
//! the generated scripts locally on scratch directories with hostile
//! names, with a curated `PATH` and stub `systemctl`/`systemd-run` (so
//! nothing ever reaches the host's systemd): once with the host's GNU
//! tools and, when `RMU_TEST_BUSYBOX` names a BusyBox binary (the nix
//! checks set it), once with BusyBox applets and `ash`, as on a tablet.
//! A real tablet could get a harness too; the test suite never uses one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::*;
use crate::error::Error;
use crate::hub::model::{DeviceId, DeviceName, DocFile, PageId, RepoId};

const DOC: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const OTHER: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const WATCHDOG: Duration = Duration::from_secs(600);

/// What a scenario needs besides the trait.
trait Harness {
    fn tablet(&self) -> &dyn Tablet;
    /// Write a file in the data dir behind rmu's back.
    fn put(&self, path: &str, contents: &[u8]);
    fn get(&self, path: &str) -> Option<Vec<u8>>;
    /// Make the armed watchdog fire now.
    fn fire_watchdog(&self);
    fn reboot(&self);
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rmu-device-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn path(text: &str) -> XochitlPath {
    XochitlPath::new(text).unwrap()
}

fn doc(id: &str) -> DocId {
    DocId::new(id).unwrap()
}

fn request() -> LockRequest {
    LockRequest::for_this_process("test-host")
}

fn device_error<T>(result: &Result<T>, expected: fn(&DeviceError) -> bool) -> bool {
    matches!(result, Err(Error::HubDevice(error)) if expected(error))
}

fn lock_lost<T>(result: Result<T>) -> bool {
    device_error(&result, |e| matches!(e, DeviceError::LockLost))
}

fn watchdog_fired<T>(result: Result<T>) -> bool {
    device_error(&result, |e| matches!(e, DeviceError::WatchdogFired))
}

fn locked<T>(result: Result<T>) -> bool {
    device_error(&result, |e| matches!(e, DeviceError::Locked(_)))
}

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Stage `files` (path → contents) through the trait.
fn stage(tablet: &dyn Tablet, lock: &HeldLock, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let list: Vec<XochitlPath> = files.keys().map(|p| path(p)).collect();
    tablet.stage(lock, &list, &mut |file, sink| {
        let data = &files[file.as_str()];
        sink(data.len() as u64, &mut &data[..])
    })
}

fn download_all(tablet: &dyn Tablet, files: &[XochitlPath]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut got = BTreeMap::new();
    tablet.download(files, &mut |file, size, reader| {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        assert_eq!(data.len() as u64, size);
        got.insert(file.to_string(), data);
        Ok(())
    })?;
    Ok(got)
}

fn hub_state() -> TabletState {
    TabletState::new(
        RepoId::new("11111111-0000-4000-8000-000000000001").unwrap(),
        "ssh://origin.invalid/library.git".to_string(),
        DeviceId::new("22222222-0000-4000-8000-000000000002").unwrap(),
        DeviceName::new("Test tablet").unwrap(),
    )
}

fn document_files() -> BTreeMap<String, Vec<u8>> {
    [
        (
            format!("{DOC}.metadata"),
            br#"{"type":"DocumentType","visibleName":"Fabricated"}"#.to_vec(),
        ),
        (
            format!("{DOC}.content"),
            br#"{"fileType":"notebook","formatVersion":2}"#.to_vec(),
        ),
        (
            format!("{DOC}/p1.rm"),
            b"reMarkable .lines file, version=6 \x00\x01\xff".to_vec(),
        ),
        (format!("{DOC}.highlights/p1.json"), b"{}".to_vec()),
    ]
    .into_iter()
    .collect()
}

fn full_write() -> DocApply {
    DocApply::Write {
        doc: doc(DOC),
        write: vec![
            DocFile::Metadata,
            DocFile::Content,
            DocFile::Page(PageId::new("p1").unwrap()),
            DocFile::Highlights(PageId::new("p1").unwrap()),
        ],
        remove: vec![],
    }
}

// ---- scenarios ---------------------------------------------------------------

fn lock_lifecycle(h: &dyn Harness) {
    let t = h.tablet();
    assert_eq!(t.lock_state().unwrap(), LockState::Free);
    let mine = request();
    let first = t.acquire_lock(&mine, false).unwrap();
    assert_eq!(first.took_over(), None);
    assert!(
        matches!(t.lock_state().unwrap(), LockState::Held(Some(owner)) if owner.run == mine.run())
    );
    assert!(locked(t.acquire_lock(&request(), false)));
    // A retry with the same request (its first reply was lost) adopts it.
    let again = t.acquire_lock(&mine, false).unwrap();
    assert_eq!((again.run(), again.took_over()), (first.run(), None));
    t.check_guard(&first).unwrap();
    t.release_lock(&first).unwrap();
    assert_eq!(t.lock_state().unwrap(), LockState::Free);
    assert!(lock_lost(t.check_guard(&first)));
    assert!(lock_lost(t.release_lock(&first)));
    assert!(lock_lost(t.stop_xochitl(&first)));
    assert!(lock_lost(t.start_xochitl(&first)));

    let second = t.acquire_lock(&request(), false).unwrap();
    let broken = t.acquire_lock(&request(), true).unwrap();
    assert_eq!(
        broken
            .took_over()
            .map(|(owner, reason)| (owner.map(|o| o.run.clone()), reason)),
        Some((Some(second.run().to_string()), StaleReason::Broken))
    );
    assert!(lock_lost(t.check_guard(&second)));
    assert!(lock_lost(t.arm_watchdog(&second, WATCHDOG)));
    assert!(lock_lost(t.write_state(&second, &hub_state())));
    t.check_guard(&broken).unwrap();
    t.release_lock(&broken).unwrap();
}

fn reboot_frees_the_lock(h: &dyn Harness) {
    let t = h.tablet();
    let old = t.acquire_lock(&request(), false).unwrap();
    h.reboot();
    let new = t.acquire_lock(&request(), false).unwrap();
    assert_eq!(
        new.took_over().map(|(_, reason)| reason),
        Some(StaleReason::BootChanged)
    );
    assert_ne!(new.owner().boot_id, old.owner().boot_id);
    assert!(lock_lost(t.check_guard(&old)));
    t.release_lock(&new).unwrap();
}

fn fired_watchdog_ends_the_run(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    assert!(!t.xochitl_active().unwrap());
    assert!(!t.watchdog_fired().unwrap());
    h.fire_watchdog();
    assert!(t.xochitl_active().unwrap(), "the watchdog starts xochitl");
    assert!(t.watchdog_fired().unwrap());
    assert!(watchdog_fired(t.check_guard(&lock)));
    assert!(watchdog_fired(t.stop_xochitl(&lock)));
    assert!(watchdog_fired(t.write_state(&lock, &hub_state())));
    assert!(watchdog_fired(t.arm_watchdog(&lock, WATCHDOG)));
    // Bringing xochitl back stays allowed to the lock holder.
    t.start_xochitl(&lock).unwrap();
    // A fired watchdog makes the lock stale; taking it clears the marker,
    // and the old run still stops: the lock is no longer its own.
    let next = t.acquire_lock(&request(), false).unwrap();
    assert_eq!(
        next.took_over().map(|(_, reason)| reason),
        Some(StaleReason::WatchdogFired)
    );
    assert!(!t.watchdog_fired().unwrap());
    t.check_guard(&next).unwrap();
    assert!(lock_lost(t.check_guard(&lock)));
    assert!(lock_lost(t.start_xochitl(&lock)));
    t.release_lock(&next).unwrap();
}

fn xochitl_stop_and_start(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    assert!(t.xochitl_active().unwrap());
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    // Re-arming replaces the watchdog.
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    assert!(!t.xochitl_active().unwrap());
    let started = t.start_xochitl(&lock).unwrap();
    assert!(
        started > 1_600_000_000 * 1_000_000_000,
        "device time {started}"
    );
    assert!(t.xochitl_active().unwrap());
    t.disarm_watchdog(&lock).unwrap();
    // Disarming twice is harmless.
    t.disarm_watchdog(&lock).unwrap();
    assert!(!t.cloud_paired().unwrap());
    t.device_info().unwrap();
    t.release_lock(&lock).unwrap();
}

fn state_round_trip(h: &dyn Harness) {
    let t = h.tablet();
    assert_eq!(t.read_state().unwrap(), None);
    let lock = t.acquire_lock(&request(), false).unwrap();
    let mut state = hub_state();
    t.write_state(&lock, &state).unwrap();
    assert_eq!(t.read_state().unwrap(), Some(state.clone()));
    state.quarantine.insert(doc(DOC));
    t.write_state(&lock, &state).unwrap();
    assert_eq!(t.read_state().unwrap(), Some(state));
    t.release_lock(&lock).unwrap();
}

fn stage_apply_read_and_delete(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    let files = document_files();
    stage(t, &lock, &files).unwrap();
    // A thumbnail is outside the mirror; only a deletion removes it.
    h.put(&format!("{DOC}.thumbnails/p1.png"), b"png");
    h.put(&format!("{DOC}.pagedata"), b"stale");
    h.put(&format!("{OTHER}.metadata"), b"{}");
    let write = DocApply::Write {
        doc: doc(DOC),
        write: vec![
            DocFile::Metadata,
            DocFile::Content,
            DocFile::Page(PageId::new("p1").unwrap()),
            DocFile::Highlights(PageId::new("p1").unwrap()),
        ],
        remove: vec![DocFile::Pagedata],
    };
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let report = t.apply(&lock, &ApplyPlan { docs: vec![write] }).unwrap();
    t.start_xochitl(&lock).unwrap();
    assert_eq!(report.applied, vec![doc(DOC)]);
    files
        .iter()
        .for_each(|(p, data)| assert_eq!(h.get(p).as_deref(), Some(&data[..]), "{p}"));
    assert_eq!(h.get(&format!("{DOC}.pagedata")), None);

    // The stat pass sees what apply reported.
    let listing = t.stat_all().unwrap();
    report.written.iter().for_each(|written| {
        let listed = listing
            .entries
            .iter()
            .find(|entry| entry.path == written.path)
            .unwrap_or_else(|| panic!("{} not listed", written.path));
        assert_eq!(listed, written);
        assert!(listed.is_regular());
        assert_eq!(listed.stat.size, files[&written.path].len() as u64);
        // Staged files were touched: device-clock nanosecond mtimes.
        assert!(listed.stat.mtime_ns > report.recorded_at_ns - 600 * 1_000_000_000);
    });
    assert_eq!(report.written.len(), files.len());
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.path == format!("{DOC}.thumbnails/p1.png"))
    );

    // Downloads deliver exactly the bytes, in one call; none for nothing.
    let list: Vec<XochitlPath> = files.keys().map(|p| path(p)).collect();
    assert_eq!(download_all(t, &list).unwrap(), files);
    assert!(download_all(t, &[]).unwrap().is_empty());
    let missing = download_all(t, &[path(&format!("{DOC}.missing"))]);
    assert!(
        device_error(&missing, |e| matches!(e, DeviceError::Download(_)))
            || matches!(missing, Err(Error::Remote { .. })),
        "{missing:?}"
    );
    assert!(device_error(
        &download_all(t, &[list[0].clone(), list[0].clone()]),
        |e| matches!(e, DeviceError::InvalidRequest(_))
    ));

    // Verification reads.
    let hashes = t.sha256(&[list[0].clone(), path("absent.rm")]).unwrap();
    assert_eq!(hashes[&list[0]], Some(sha256_hex(&files[list[0].as_str()])));
    assert_eq!(hashes[&path("absent.rm")], None);
    let metadata = path(&format!("{DOC}.metadata"));
    let read = t
        .read_files(&[metadata.clone(), path("absent.content")])
        .unwrap();
    assert_eq!(
        read[&metadata].as_deref(),
        Some(&files[metadata.as_str()][..])
    );
    assert_eq!(read[&path("absent.content")], None);
    assert!(t.read_files(&[]).unwrap().is_empty());
    assert!(t.sha256(&[]).unwrap().is_empty());

    // A deletion removes the whole set, and nothing else.
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let report = t
        .apply(
            &lock,
            &ApplyPlan {
                docs: vec![DocApply::Delete { doc: doc(DOC) }],
            },
        )
        .unwrap();
    t.start_xochitl(&lock).unwrap();
    assert_eq!(report.applied, vec![doc(DOC)]);
    assert!(report.written.is_empty());
    let left: Vec<String> = t
        .stat_all()
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(left, vec![format!("{OTHER}.metadata")]);
    t.clear_staging(&lock).unwrap();
    t.disarm_watchdog(&lock).unwrap();
    t.release_lock(&lock).unwrap();
}

fn apply_refuses_unstaged_files_before_any_change(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    // Only the first document is staged: nothing at all may change.
    let first: BTreeMap<String, Vec<u8>> = [(format!("{OTHER}.metadata"), b"{}".to_vec())].into();
    stage(t, &lock, &first).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let plan = ApplyPlan {
        docs: vec![
            DocApply::Write {
                doc: doc(OTHER),
                write: vec![DocFile::Metadata],
                remove: vec![],
            },
            full_write(),
        ],
    };
    let result = t.apply(&lock, &plan);
    assert!(
        matches!(
            &result,
            Err(Error::HubDevice(DeviceError::ApplyIncomplete { applied, .. })) if applied.is_empty()
        ),
        "{result:?}"
    );
    assert_eq!(h.get(&format!("{OTHER}.metadata")), None);
    t.start_xochitl(&lock).unwrap();
}

fn apply_stops_when_the_watchdog_fired(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    stage(t, &lock, &document_files()).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    h.fire_watchdog();
    assert!(watchdog_fired(t.apply(
        &lock,
        &ApplyPlan {
            docs: vec![full_write()]
        }
    )));
    assert_eq!(h.get(&format!("{DOC}.metadata")), None);
}

fn stat_pass_lists_odd_names(h: &dyn Harness) {
    let names = [
        " lead",
        "trail ",
        "two  spaces",
        "it's",
        "$(x)",
        "-dash",
        "uni\u{e9}",
    ];
    names.iter().for_each(|name| h.put(name, name.as_bytes()));
    h.put("dir/with\nnewline", b"x");
    h.put("new\nline", b"x");
    let listing = h.tablet().stat_all().unwrap();
    let mut expected: Vec<&str> = names.to_vec();
    expected.sort_unstable();
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(listing.unlisted, 2);
    listing.entries.iter().for_each(|entry| {
        assert_eq!(entry.stat.size, entry.path.len() as u64);
        assert!(entry.stat.mtime_ns > 0 && entry.stat.ctime_ns > 0);
    });
    // The same odd names transfer.
    let list: Vec<XochitlPath> = names.iter().map(|n| path(n)).collect();
    let got = download_all(h.tablet(), &list).unwrap();
    names
        .iter()
        .for_each(|name| assert_eq!(got[*name], name.as_bytes()));
    let hashes = h.tablet().sha256(&list).unwrap();
    names.iter().for_each(|name| {
        assert_eq!(hashes[&path(name)], Some(sha256_hex(name.as_bytes())));
    });
    let read = h.tablet().read_files(&list).unwrap();
    names
        .iter()
        .for_each(|name| assert_eq!(read[&path(name)].as_deref(), Some(name.as_bytes())));
}

type Scenario = fn(&dyn Harness);

const SCENARIOS: &[(&str, Scenario)] = &[
    ("lock_lifecycle", lock_lifecycle),
    ("reboot_frees_the_lock", reboot_frees_the_lock),
    ("fired_watchdog_ends_the_run", fired_watchdog_ends_the_run),
    ("xochitl_stop_and_start", xochitl_stop_and_start),
    ("state_round_trip", state_round_trip),
    ("stage_apply_read_and_delete", stage_apply_read_and_delete),
    (
        "apply_refuses_unstaged_files_before_any_change",
        apply_refuses_unstaged_files_before_any_change,
    ),
    (
        "apply_stops_when_the_watchdog_fired",
        apply_stops_when_the_watchdog_fired,
    ),
    ("stat_pass_lists_odd_names", stat_pass_lists_odd_names),
];

// ---- the fake ------------------------------------------------------------------

struct FakeHarness {
    tablet: FakeTablet,
    _scratch: Scratch,
}

impl FakeHarness {
    fn new() -> Self {
        Self::with(|builder| builder)
    }

    fn with(configure: impl FnOnce(FakeTabletBuilder) -> FakeTabletBuilder) -> Self {
        let scratch = Scratch::new("fake");
        let tablet = configure(FakeTablet::builder(&scratch.0)).build().unwrap();
        Self {
            tablet,
            _scratch: scratch,
        }
    }
}

impl Harness for FakeHarness {
    fn tablet(&self) -> &dyn Tablet {
        &self.tablet
    }

    fn put(&self, path: &str, contents: &[u8]) {
        self.tablet.write_file(path, contents).unwrap();
    }

    fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.tablet.read_file(path)
    }

    fn fire_watchdog(&self) {
        assert!(self.tablet.fire_watchdog().unwrap(), "no watchdog armed");
    }

    fn reboot(&self) {
        self.tablet.reboot();
    }
}

#[test]
fn fake_honours_the_contract() {
    SCENARIOS.iter().for_each(|(name, scenario)| {
        eprintln!("fake: {name}");
        scenario(&FakeHarness::new());
    });
}

fn transport(result: &Result<impl std::fmt::Debug>) -> bool {
    matches!(result, Err(Error::Remote { status: 255, .. }))
}

#[test]
fn fake_fails_a_mutation_without_effect() {
    let h = FakeHarness::with(|b| b.fault(Fault::FailMutation(1)));
    let t = &h.tablet;
    assert!(transport(&t.acquire_lock(&request(), false)));
    assert_eq!(t.lock_state().unwrap(), LockState::Free);
    // Reads are not mutations; the fault fired once.
    t.stat_all().unwrap();
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.inject(Fault::FailMutation(2));
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    assert!(transport(&t.write_state(&lock, &hub_state())));
    assert_eq!(t.read_state().unwrap(), None);
    t.write_state(&lock, &hub_state()).unwrap();
    assert_eq!(t.mutations(), 5);
}

#[test]
fn fake_loses_acknowledgements_after_the_effect() {
    let h = FakeHarness::with(|b| b.fault(Fault::LoseAck(1)));
    let t = &h.tablet;
    // The lock was taken but the reply lost: another run finds it live,
    // while a retry of the same request adopts it.
    let mine = request();
    assert!(transport(&t.acquire_lock(&mine, false)));
    assert!(locked(t.acquire_lock(&request(), false)));
    let lock = t.acquire_lock(&mine, false).unwrap();
    t.inject(Fault::LoseAck(1));
    assert!(transport(&t.write_state(&lock, &hub_state())));
    assert_eq!(t.read_state().unwrap(), Some(hub_state()));
}

#[test]
fn fake_disconnects_like_a_dead_rmu() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.inject(Fault::DisconnectAfter(1));
    // The stop happened, then the connection went: no error path can
    // bring xochitl back or release the lock.
    assert!(transport(&t.stop_xochitl(&lock)));
    assert!(t.is_disconnected());
    assert!(transport(&t.start_xochitl(&lock)));
    assert!(transport(&t.release_lock(&lock)));
    assert!(transport(&t.stat_all()));
    assert!(!t.is_running());
    assert_eq!(t.watchdog_armed(), Some(WATCHDOG));
    // The watchdog brings xochitl back; the next run takes the lock over.
    assert!(t.fire_watchdog().unwrap());
    t.reconnect();
    assert!(t.is_running());
    let next = t.acquire_lock(&request(), false).unwrap();
    assert_eq!(
        next.took_over().map(|(_, reason)| reason),
        Some(StaleReason::WatchdogFired)
    );
    // DisconnectAt: no effect at all.
    t.inject(Fault::DisconnectAt(1));
    assert!(transport(&t.write_state(&next, &hub_state())));
    t.reconnect();
    assert_eq!(t.read_state().unwrap(), None);
}

#[test]
fn fake_reports_apply_transport_failures_like_a_real_tablet() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    let plan = ApplyPlan {
        docs: vec![full_write()],
    };
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    stage(t, &lock, &document_files()).unwrap();
    let incomplete = |result: &Result<ApplyReport>, docs: usize| matches!(result, Err(Error::HubDevice(DeviceError::ApplyIncomplete { applied, .. })) if applied.len() == docs);
    t.inject(Fault::FailMutation(1));
    assert!(incomplete(&t.apply(&lock, &plan), 0));
    assert_eq!(h.get(&format!("{DOC}.metadata")), None);
    t.inject(Fault::LoseAck(1));
    assert!(incomplete(&t.apply(&lock, &plan), 1));
    assert!(h.get(&format!("{DOC}.metadata")).is_some());
}

#[test]
fn fake_kills_an_apply_after_k_moves() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    let files = document_files();
    stage(t, &lock, &files).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.inject(Fault::KillApplyAfter(2));
    let plan = ApplyPlan {
        docs: vec![DocApply::Write {
            doc: doc(DOC),
            write: vec![
                DocFile::Metadata,
                DocFile::Content,
                DocFile::Page(PageId::new("p1").unwrap()),
            ],
            remove: vec![],
        }],
    };
    let result = t.apply(&lock, &plan);
    assert!(matches!(
        result,
        Err(Error::HubDevice(DeviceError::ApplyIncomplete { ref applied, .. })) if applied.is_empty()
    ));
    // Pages first, then content; metadata never arrived.
    assert!(h.get(&format!("{DOC}/p1.rm")).is_some());
    assert!(h.get(&format!("{DOC}.content")).is_some());
    assert_eq!(h.get(&format!("{DOC}.metadata")), None);
    // Killed right after a document's last move: applied but unreported.
    stage(t, &lock, &files).unwrap();
    t.inject(Fault::KillApplyAfter(3));
    let result = t.apply(&lock, &plan);
    assert!(matches!(
        result,
        Err(Error::HubDevice(DeviceError::ApplyIncomplete { ref applied, .. })) if applied.is_empty()
    ));
    assert!(h.get(&format!("{DOC}.metadata")).is_some());
    // The kill is one-shot; re-staging and re-applying completes.
    stage(t, &lock, &files).unwrap();
    let report = t.apply(&lock, &plan).unwrap();
    assert_eq!(report.applied, vec![doc(DOC)]);
}

#[test]
fn fake_runs_user_edits_between_operations() {
    let target = format!("{DOC}.metadata");
    let edit = |contents: &'static [u8]| -> Edit {
        let target = target.clone();
        Box::new(move |dir: &Path| fs::write(dir.join(&target), contents).unwrap())
    };
    let h = FakeHarness::with(|b| b.fault(Fault::EditBefore(2, edit(b"first"))));
    let t = &h.tablet;
    assert!(t.stat_all().unwrap().entries.is_empty());
    let listing = t.stat_all().unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, target);
    assert_eq!(t.ops(), vec![OpKind::StatAll, OpKind::StatAll]);

    // Nobody edits while xochitl is stopped: the edit waits for the start.
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.inject(Fault::EditBefore(1, edit(b"second")));
    t.stat_all().unwrap();
    t.read_files(&[path(&target)]).unwrap();
    assert_eq!(h.get(&target).as_deref(), Some(&b"first"[..]));
    t.start_xochitl(&lock).unwrap();
    t.stat_all().unwrap();
    assert_eq!(h.get(&target).as_deref(), Some(&b"second"[..]));
}

#[test]
fn fake_checks_the_callers_side_of_the_contract() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    let violation = |result: Result<()>| {
        device_error(&result, |e| matches!(e, DeviceError::ContractViolation(_)))
    };
    assert!(violation(t.stop_xochitl(&lock)));
    assert!(t.is_running());
    let plan = ApplyPlan {
        docs: vec![DocApply::Delete { doc: doc(DOC) }],
    };
    assert!(violation(t.apply(&lock, &plan).map(drop)));
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.apply(&lock, &plan).unwrap();
    assert_eq!(
        t.ops(),
        vec![
            OpKind::AcquireLock,
            OpKind::StopXochitl,
            OpKind::Apply,
            OpKind::ArmWatchdog,
            OpKind::StopXochitl,
            OpKind::Apply,
        ]
    );
}

#[test]
fn fake_builder_seeds_files_state_and_device() {
    let h = FakeHarness::with(|b| {
        b.file(format!("{DOC}.metadata"), "{}")
            .stopped()
            .cloud_paired(true)
            .device_info("reMarkable Test", "9.9")
            .hub_state(hub_state())
    });
    let t = &h.tablet;
    assert!(!t.xochitl_active().unwrap());
    assert!(t.cloud_paired().unwrap());
    assert_eq!(t.device_info().unwrap().firmware.as_deref(), Some("9.9"));
    assert_eq!(t.read_state().unwrap(), Some(hub_state()));
    assert_eq!(t.stat_all().unwrap().entries.len(), 1);
    let boot = t.boot_id();
    t.reboot();
    assert_ne!(t.boot_id(), boot);
    assert!(t.is_running());
    assert!(!t.fire_watchdog().unwrap(), "nothing armed after a reboot");
}

// ---- the real implementation over a local mock transport ------------------------

#[cfg(target_os = "linux")]
mod local_ssh {
    use super::*;
    use crate::ssh::{SshOptions, SshSession, shell_quote};
    use std::process::Command;

    /// Tools the scripts may run; everything else is absent from `PATH`.
    const TOOLS: &[&str] = &[
        "sh",
        "cat",
        "mkdir",
        "mv",
        "rm",
        "stat",
        "find",
        "tar",
        "sha256sum",
        "grep",
        "date",
        "wc",
        "sync",
        "touch",
        "sleep",
        "sed",
        "mktemp",
        "printf",
        "chmod",
        "chown",
        "head",
        "flock",
        "setsid",
        "kill",
        "true",
        "false",
        "uname",
        "df",
    ];

    fn which(tool: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    }

    /// Where the tools come from.
    #[derive(Clone, Copy)]
    pub(super) enum Tools<'a> {
        /// The host's (GNU) tools.
        Host,
        /// BusyBox applets, `sh` included.
        BusyBox(&'a Path),
    }

    /// The BusyBox binary from `RMU_TEST_BUSYBOX`, if set.
    pub(super) fn busybox() -> Option<PathBuf> {
        std::env::var_os("RMU_TEST_BUSYBOX").map(PathBuf::from)
    }

    pub(super) struct SshHarness {
        tablet: SshTablet,
        root: PathBuf,
        bin: PathBuf,
        sys: PathBuf,
        boot_file: PathBuf,
        _scratch: Scratch,
    }

    impl SshHarness {
        /// `systemd_run`: install the `systemd-run` stub (else the sleep
        /// fallback runs).
        pub(super) fn new(tools: Tools<'_>, systemd_run: bool) -> Self {
            let scratch = Scratch::new("ssh");
            // Hostile directory names: quotes, spaces, `$()`, a newline.
            let root = scratch.0.join("it's a $(dir) `x`");
            let xochitl = root.join("xochitl data");
            let state = root.join("state;dir&\nnext");
            let bin = scratch.0.join("bin");
            let sys = scratch.0.join("sys");
            [&xochitl, &state, &bin, &sys]
                .iter()
                .for_each(|dir| fs::create_dir_all(dir).unwrap());
            match tools {
                Tools::Host => TOOLS.iter().for_each(|tool| {
                    if let Some(found) = which(tool) {
                        std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
                    }
                }),
                Tools::BusyBox(busybox) => {
                    let output = Command::new(busybox).arg("--list").output().unwrap();
                    let applets = String::from_utf8(output.stdout).unwrap();
                    TOOLS
                        .iter()
                        .filter(|tool| applets.lines().any(|applet| applet == **tool))
                        .for_each(|tool| {
                            std::os::unix::fs::symlink(busybox, bin.join(tool)).unwrap();
                        });
                }
            }
            let q = |p: &Path| shell_quote(&p.to_string_lossy());
            test_support::write_script(
                &bin.join("systemctl"),
                &format!(
                    "printf '%s\\n' \"$*\" >> {log}\n\
                     case \"$*\" in\n\
                     'stop xochitl.service') rm -f {running} ;;\n\
                     'start xochitl.service') : > {running} ;;\n\
                     'is-active xochitl.service') if [ -e {running} ]; then echo active; else echo inactive; exit 3; fi ;;\n\
                     'stop '*'.timer') u=${{2%.timer}}; rm -f {sys}/watchdog-\"$u\" ;;\n\
                     esac\n\
                     exit 0\n",
                    log = q(&sys.join("systemctl.log")),
                    running = q(&sys.join("running")),
                    sys = q(&sys),
                ),
            );
            if systemd_run {
                test_support::write_script(
                    &bin.join("systemd-run"),
                    &format!(
                        "printf '%s\\n' \"$*\" >> {log}\n\
                         unit=\n\
                         while [ $# -gt 0 ]; do case \"$1\" in --unit=*) unit=${{1#--unit=}}; shift ;; --*) shift ;; *) break ;; esac; done\n\
                         for a do printf '%s\\0' \"$a\"; done > {sys}/watchdog-\"$unit\"\n",
                        log = q(&sys.join("systemd-run.log")),
                        sys = q(&sys),
                    ),
                );
            }
            fs::write(sys.join("running"), b"").unwrap();
            let boot_file = root.join("boot id");
            fs::write(&boot_file, format!("{}\n", uuid::Uuid::new_v4())).unwrap();
            let conf = root.join("xochitl.conf");
            fs::write(&conf, "[General]\ndevicetoken=@Invalid()\n").unwrap();
            let ssh = scratch.0.join("ssh");
            test_support::write_script(
                &ssh,
                &format!(
                    "for cmd do :; done\nPATH={bin}; export PATH\nexec sh -c \"$cmd\"\n",
                    bin = q(&bin)
                ),
            );
            let mut session = SshSession::new(SshOptions {
                destination: "mock.invalid".into(),
                multiplex: false,
                ..SshOptions::default()
            })
            .unwrap();
            session.set_ssh_exe(ssh);
            let paths = TabletPaths {
                xochitl_dir: xochitl.to_string_lossy().into_owned(),
                state_dir: state.to_string_lossy().into_owned(),
                xochitl_conf: conf.to_string_lossy().into_owned(),
                boot_id: boot_file.to_string_lossy().into_owned(),
            };
            Self {
                tablet: SshTablet::new(session).with_paths(paths),
                root,
                bin,
                sys,
                boot_file,
                _scratch: scratch,
            }
        }

        fn data(&self, path: &str) -> PathBuf {
            Path::new(&self.tablet.paths().xochitl_dir).join(path)
        }

        pub(super) fn systemctl_log(&self) -> String {
            fs::read_to_string(self.sys.join("systemctl.log")).unwrap_or_default()
        }

        pub(super) fn systemd_run_log(&self) -> String {
            fs::read_to_string(self.sys.join("systemd-run.log")).unwrap_or_default()
        }

        /// Watchdogs the `systemd-run` stub has armed and not stopped.
        pub(super) fn armed_units(&self) -> Vec<PathBuf> {
            let mut units: Vec<PathBuf> = fs::read_dir(&self.sys)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("watchdog-")
                })
                .collect();
            units.sort();
            units
        }

        pub(super) fn running(&self) -> bool {
            self.sys.join("running").exists()
        }

        pub(super) fn conf(&self) -> PathBuf {
            self.root.join("xochitl.conf")
        }

        pub(super) fn ssh_tablet(&self) -> &SshTablet {
            &self.tablet
        }

        pub(super) fn has_tool(&self, tool: &str) -> bool {
            self.bin.join(tool).exists()
        }
    }

    impl Harness for SshHarness {
        fn tablet(&self) -> &dyn Tablet {
            &self.tablet
        }

        fn put(&self, path: &str, contents: &[u8]) {
            let target = self.data(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, contents).unwrap();
        }

        fn get(&self, path: &str) -> Option<Vec<u8>> {
            fs::read(self.data(path)).ok()
        }

        /// Run the command of the one armed `systemd-run` watchdog.
        fn fire_watchdog(&self) {
            let units = self.armed_units();
            assert_eq!(units.len(), 1, "armed watchdogs: {units:?}");
            let recorded = fs::read(&units[0]).unwrap();
            fs::remove_file(&units[0]).unwrap();
            let args: Vec<String> = recorded
                .split(|&b| b == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
                .collect();
            let status = Command::new(&args[0])
                .args(&args[1..])
                .env("PATH", &self.bin)
                .status()
                .unwrap();
            assert!(status.success());
        }

        fn reboot(&self) {
            fs::write(&self.boot_file, format!("{}\n", uuid::Uuid::new_v4())).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
fn run_ssh_contract(tools: local_ssh::Tools<'_>, label: &str) {
    SCENARIOS.iter().for_each(|(name, scenario)| {
        eprintln!("{label}: {name}");
        let harness = local_ssh::SshHarness::new(tools, true);
        scenario(&harness);
        let log = harness.systemctl_log();
        if *name == "xochitl_stop_and_start" {
            // The scripts really reached the stubs, and re-arming left
            // exactly one watchdog until the disarm.
            assert!(log.contains("stop xochitl.service\n"), "{log}");
            assert!(log.contains("start xochitl.service\n"), "{log}");
            let armed = harness.systemd_run_log();
            assert_eq!(armed.matches("--on-active=600s").count(), 2, "{armed}");
            assert!(
                harness.armed_units().is_empty(),
                "{:?}",
                harness.armed_units()
            );
        }
        // Every start was preceded by reset-failed.
        let lines: Vec<&str> = log.lines().collect();
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == "start xochitl.service")
            .for_each(|(at, _)| {
                assert!(
                    at > 0 && lines[at - 1] == "reset-failed xochitl.service",
                    "{log}"
                );
            });
    });
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_honours_the_contract() {
    run_ssh_contract(local_ssh::Tools::Host, "ssh");
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_honours_the_contract_on_busybox() {
    let Some(busybox) = local_ssh::busybox() else {
        eprintln!("RMU_TEST_BUSYBOX is not set; skipping the BusyBox contract run");
        return;
    };
    run_ssh_contract(local_ssh::Tools::BusyBox(&busybox), "busybox");
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_rearms_before_removing_the_old_watchdog() {
    let harness = local_ssh::SshHarness::new(local_ssh::Tools::Host, true);
    let t = harness.ssh_tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    let first = harness.armed_units();
    assert_eq!(first.len(), 1);
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    let second = harness.armed_units();
    assert_eq!(second.len(), 1);
    assert_ne!(first, second, "each arming uses a fresh unit");
    let log = harness.systemctl_log();
    let file = first[0].file_name().unwrap().to_string_lossy().into_owned();
    let unit = file.strip_prefix("watchdog-").unwrap();
    assert!(log.contains(&format!("stop {unit}.timer\n")), "{log}");
    t.disarm_watchdog(&lock).unwrap();
    assert!(harness.armed_units().is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_detects_cloud_pairing_and_fails_closed() {
    let harness = local_ssh::SshHarness::new(local_ssh::Tools::Host, true);
    let t = harness.ssh_tablet();
    [
        ("devicetoken=@Invalid()\n", false),
        ("devicetoken=\n", false),
        ("devicetoken=@ByteArray()\n", false),
        ("devicetoken=\"\"\n", false),
        ("other=1\n", false),
        ("devicetoken=@ByteArray(fabricated-token)\n", true),
    ]
    .into_iter()
    .for_each(|(conf, paired)| {
        fs::write(harness.conf(), format!("[General]\n{conf}")).unwrap();
        assert_eq!(t.cloud_paired().unwrap(), paired, "{conf}");
    });
    fs::remove_file(harness.conf()).unwrap();
    assert!(matches!(
        t.cloud_paired(),
        Err(Error::Remote { status: 1, .. })
    ));
}

#[cfg(target_os = "linux")]
fn check_sleep_watchdog(harness: &local_ssh::SshHarness) {
    let t = harness.ssh_tablet();
    let paths = t.paths().clone();
    let lock = t.acquire_lock(&request(), false).unwrap();
    let alive = |pid: &str| {
        std::process::Command::new("sh")
            .args(["-c", "kill -0 -- \"-$1\" 2>/dev/null", "sh", pid])
            .status()
            .unwrap()
            .success()
    };
    let gone = |pid: &str| {
        (0..50).any(|_| {
            let alive = alive(pid);
            if alive {
                std::thread::sleep(Duration::from_millis(100));
            }
            !alive
        })
    };
    let pid = || {
        fs::read_to_string(paths.watchdog_pid())
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    // Re-arming replaces the sleeper; disarm kills it and removes its
    // token before it fires.
    t.arm_watchdog(&lock, Duration::from_secs(60)).unwrap();
    let first = pid();
    t.arm_watchdog(&lock, Duration::from_secs(60)).unwrap();
    let second = pid();
    assert_ne!(first, second);
    assert!(gone(&first), "the replaced sleeper survived");
    assert!(Path::new(&paths.watchdog_token()).exists());
    t.disarm_watchdog(&lock).unwrap();
    assert!(!Path::new(&paths.watchdog_pid()).exists());
    assert!(!Path::new(&paths.watchdog_token()).exists());
    assert!(gone(&second), "the sleeper survived the disarm");

    // Armed for a second, with xochitl stopped: it fires.
    t.arm_watchdog(&lock, Duration::from_secs(1)).unwrap();
    t.stop_xochitl(&lock).unwrap();
    assert!(!harness.running());
    let fired = (0..150).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        harness.running() && t.watchdog_fired().unwrap()
    });
    assert!(fired, "the sleep watchdog did not fire");
    assert!(watchdog_fired(t.check_guard(&lock)));
    let log = harness.systemctl_log();
    assert!(
        log.ends_with("reset-failed xochitl.service\nstart xochitl.service\n"),
        "{log}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_sleep_watchdog_fires_and_disarms() {
    let harness = local_ssh::SshHarness::new(local_ssh::Tools::Host, false);
    if !harness.has_tool("setsid") || !harness.has_tool("flock") {
        eprintln!("setsid or flock not available; skipping the sleep-watchdog test");
        return;
    }
    check_sleep_watchdog(&harness);
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_sleep_watchdog_on_busybox() {
    let Some(busybox) = local_ssh::busybox() else {
        eprintln!("RMU_TEST_BUSYBOX is not set; skipping the BusyBox sleep-watchdog test");
        return;
    };
    check_sleep_watchdog(&local_ssh::SshHarness::new(
        local_ssh::Tools::BusyBox(&busybox),
        false,
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn ssh_tablet_takeover_is_a_compare_and_swap() {
    // The takeover replaces the record only if its checksum is still the
    // one inspected: a record that changed meanwhile is never overwritten.
    let harness = local_ssh::SshHarness::new(local_ssh::Tools::Host, true);
    let t = harness.ssh_tablet();
    let paths = t.paths().clone();
    let held = t.acquire_lock(&request(), false).unwrap();
    let stale = "0".repeat(64);
    let output = t
        .session_for_tests()
        .run_script(&scripts::takeover_script(&paths, &request(), &stale))
        .unwrap();
    assert_eq!(output.status.code(), Some(scripts::EXIT_LOCK_CHANGED));
    t.check_guard(&held).unwrap();
    // Even a corrupt record can be broken: the version is its checksum.
    fs::write(paths.lock_owner(), b"\0\xff garbage").unwrap();
    assert!(locked(t.acquire_lock(&request(), false)));
    let broken = t.acquire_lock(&request(), true).unwrap();
    assert_eq!(broken.took_over(), Some((None, StaleReason::Broken)));
    t.check_guard(&broken).unwrap();
}
