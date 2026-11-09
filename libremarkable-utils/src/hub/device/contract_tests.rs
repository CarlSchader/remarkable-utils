//! Contract scenarios, written against the [`Tablet`] trait and the
//! small [`Harness`] (see `harness`). They run against the fake and, on
//! Linux, against the real [`SshTablet`] through a mock `ssh`: once with
//! the host's GNU tools and, when `RMU_TEST_BUSYBOX` names a BusyBox
//! binary (the dev shell and the nix checks set it), once with BusyBox
//! applets and `ash`, as on a tablet. A real tablet could get a harness
//! too; the test suite never uses one.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::harness::{FakeHarness, Harness, running_as_root};
use super::*;
use crate::error::Error;
use crate::git::FileMode;
use crate::hub::model::{DeviceId, DeviceName, DocFile, PageId, RepoId};

const DOC: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const OTHER: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const WATCHDOG: Duration = Duration::from_secs(600);

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

fn rebooted<T>(result: Result<T>) -> bool {
    device_error(&result, |e| matches!(e, DeviceError::Rebooted))
}

fn locked<T>(result: Result<T>) -> bool {
    device_error(&result, |e| matches!(e, DeviceError::Locked(_)))
}

/// Refused by the real tablet with `real`, by the fake as a contract
/// violation.
fn refused<T>(result: Result<T>, real: fn(&DeviceError) -> bool) -> bool {
    matches!(
        &result,
        Err(Error::HubDevice(DeviceError::ContractViolation(_)))
    ) || device_error(&result, real)
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

/// A page id long enough that its path needs tar's long-name entries.
fn long_page() -> PageId {
    PageId::new("L".repeat(120)).unwrap()
}

/// The fabricated document: each file and its contents.
fn document() -> Vec<(DocFile, Vec<u8>)> {
    vec![
        (
            DocFile::Metadata,
            br#"{"type":"DocumentType","visibleName":"Fabricated"}"#.to_vec(),
        ),
        (
            DocFile::Content,
            br#"{"fileType":"notebook","formatVersion":2}"#.to_vec(),
        ),
        (
            DocFile::Page(PageId::new("p1").unwrap()),
            b"reMarkable .lines file, version=6 \x00\x01\xff".to_vec(),
        ),
        (DocFile::Page(long_page()), b"long page".to_vec()),
        (
            DocFile::Highlights(PageId::new("p1").unwrap()),
            b"{}".to_vec(),
        ),
    ]
}

fn document_files() -> BTreeMap<String, Vec<u8>> {
    document()
        .into_iter()
        .map(|(file, data)| (file.xochitl_path(&doc(DOC)), data))
        .collect()
}

fn write_of(id: &str, files: Vec<(DocFile, Vec<u8>)>, remove: Vec<DocFile>) -> DocApply {
    DocApply::Write {
        doc: doc(id),
        write: files
            .into_iter()
            .map(|(file, data)| StagedFile::regular(file, data.len() as u64))
            .collect(),
        remove,
    }
}

fn full_write() -> DocApply {
    write_of(DOC, document(), vec![])
}

fn metadata_only(id: &str) -> (DocApply, BTreeMap<String, Vec<u8>>) {
    let data = b"{}".to_vec();
    (
        write_of(id, vec![(DocFile::Metadata, data.clone())], vec![]),
        [(format!("{id}.metadata"), data)].into(),
    )
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
    assert!(lock_lost(stage(t, &first, &document_files())));
    // Starting xochitl needs only that no other run holds the lock.
    t.start_xochitl(&first).unwrap();

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
    assert!(lock_lost(t.start_xochitl(&second)));
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

fn reboot_ends_the_run(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let (plan, files) = metadata_only(DOC);
    stage(t, &lock, &files).unwrap();
    h.reboot();
    // The watchdog died with the boot and xochitl may be running again:
    // every guarded operation now ends the run.
    assert!(rebooted(t.check_guard(&lock)));
    assert!(rebooted(t.stop_xochitl(&lock)));
    assert!(rebooted(t.arm_watchdog(&lock, WATCHDOG)));
    assert!(rebooted(t.write_state(&lock, &hub_state())));
    assert!(rebooted(stage(t, &lock, &files)));
    assert!(rebooted(t.apply(&lock, &ApplyPlan { docs: vec![plan] })));
    assert_eq!(h.get(&format!("{DOC}.metadata")), None);
    // Bringing xochitl back and releasing stay possible.
    t.start_xochitl(&lock).unwrap();
    assert!(t.xochitl_active().unwrap());
    t.release_lock(&lock).unwrap();
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
    assert!(watchdog_fired(stage(t, &lock, &document_files())));
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

fn watchdog_fires_without_the_state_dir(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    // The state dir goes (or /home turns read-only): the marker cannot be
    // written, and the watchdog must still bring xochitl back.
    h.wipe_state();
    h.fire_watchdog();
    assert!(t.xochitl_active().unwrap());
    assert!(lock_lost(t.check_guard(&lock)));
    assert!(lock_lost(t.arm_watchdog(&lock, WATCHDOG)));
    assert!(lock_lost(t.write_state(&lock, &hub_state())));
    assert!(lock_lost(t.release_lock(&lock)));
    // With no lock left, starting xochitl is allowed.
    t.start_xochitl(&lock).unwrap();
    assert_eq!(t.lock_state().unwrap(), LockState::Free);
    // A new run starts from scratch.
    let next = t.acquire_lock(&request(), false).unwrap();
    t.release_lock(&next).unwrap();
}

fn takeover_keeps_the_previous_watchdog(h: &dyn Harness) {
    let t = h.tablet();
    let old = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&old, WATCHDOG).unwrap();
    t.stop_xochitl(&old).unwrap();
    let new = t.acquire_lock(&request(), true).unwrap();
    // The new owner can neither disarm while xochitl is stopped nor stop
    // the old owner's watchdog: it is what brings xochitl back.
    assert!(refused(t.disarm_watchdog(&new), |e| matches!(
        e,
        DeviceError::XochitlStopped
    )));
    t.start_xochitl(&new).unwrap();
    t.stop_xochitl(&new).unwrap_err();
    t.disarm_watchdog(&new).unwrap();
    // The old run's watchdog is still armed and fires.
    h.fire_watchdog();
    assert!(t.xochitl_active().unwrap());
}

fn xochitl_stop_and_start(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    assert!(t.xochitl_active().unwrap());
    // No stop without this run's watchdog.
    assert!(refused(t.stop_xochitl(&lock), |e| matches!(
        e,
        DeviceError::WatchdogNotArmed
    )));
    assert!(t.xochitl_active().unwrap());
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    // Re-arming replaces the watchdog.
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    assert!(!t.xochitl_active().unwrap());
    // No disarm while xochitl is stopped.
    assert!(refused(t.disarm_watchdog(&lock), |e| matches!(
        e,
        DeviceError::XochitlStopped
    )));
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
    let write = write_of(DOC, document(), vec![DocFile::Pagedata]);
    let plan = ApplyPlan { docs: vec![write] };
    // No apply while xochitl runs.
    assert!(refused(t.apply(&lock, &plan), |e| matches!(
        e,
        DeviceError::XochitlRunning
    )));
    assert_eq!(h.get(&format!("{DOC}.metadata")), None);
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let report = t.apply(&lock, &plan).unwrap();
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
        assert!(listed.is_regular() && !listed.is_executable());
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
        device_error(&missing, |e| matches!(e, DeviceError::Download(_))),
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

fn staged_files_lists_what_is_not_moved_yet(h: &dyn Harness) {
    let t = h.tablet();
    assert!(t.staged_files().unwrap().entries.is_empty());
    let lock = t.acquire_lock(&request(), false).unwrap();
    let (write, files) = metadata_only(DOC);
    stage(t, &lock, &files).unwrap();
    let staged = |t: &dyn Tablet| -> Vec<String> {
        t.staged_files()
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect()
    };
    assert_eq!(staged(t), vec![format!("{}/{DOC}.metadata", lock.run())]);
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.apply(&lock, &ApplyPlan { docs: vec![write] }).unwrap();
    t.start_xochitl(&lock).unwrap();
    assert!(staged(t).is_empty());
    t.clear_staging(&lock).unwrap();
    assert!(staged(t).is_empty());
    t.disarm_watchdog(&lock).unwrap();
    t.release_lock(&lock).unwrap();
}

fn apply_sets_planned_modes(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    let data = b"payload".to_vec();
    let files: BTreeMap<String, Vec<u8>> = [(format!("{DOC}.pdf"), data.clone())].into();
    stage(t, &lock, &files).unwrap();
    let plan = ApplyPlan {
        docs: vec![DocApply::Write {
            doc: doc(DOC),
            write: vec![StagedFile {
                file: DocFile::PayloadPdf,
                size: data.len() as u64,
                mode: FileMode::Executable,
            }],
            remove: vec![],
        }],
    };
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let report = t.apply(&lock, &plan).unwrap();
    assert!(report.written[0].is_executable());
    t.start_xochitl(&lock).unwrap();
}

fn apply_refuses_unstaged_files_before_any_change(h: &dyn Harness) {
    let t = h.tablet();
    let lock = t.acquire_lock(&request(), false).unwrap();
    let (first, staged) = metadata_only(OTHER);
    stage(t, &lock, &staged).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    let incomplete_before_any_change = |result: &Result<ApplyReport>| {
        matches!(
            result,
            Err(Error::HubDevice(DeviceError::ApplyIncomplete { applied, .. })) if applied.is_empty()
        )
    };
    // Only the first document is staged: nothing at all may change.
    let result = t.apply(
        &lock,
        &ApplyPlan {
            docs: vec![first.clone(), full_write()],
        },
    );
    assert!(incomplete_before_any_change(&result), "{result:?}");
    assert_eq!(h.get(&format!("{OTHER}.metadata")), None);
    // A staged file of the wrong size (a partial upload) neither.
    let short = write_of(OTHER, vec![(DocFile::Metadata, b"{}...".to_vec())], vec![]);
    let result = t.apply(&lock, &ApplyPlan { docs: vec![short] });
    assert!(incomplete_before_any_change(&result), "{result:?}");
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
        "-",
        "uni\u{e9}",
    ];
    names.iter().for_each(|name| h.put(name, name.as_bytes()));
    h.put("dir/with\nnewline", b"x");
    h.put("new\nline", b"x");
    h.put("carriage\rreturn", b"x");
    h.put(&format!("{DOC}.metadata\r"), b"x");
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
    assert_eq!(listing.unlisted, 4);
    listing.entries.iter().for_each(|entry| {
        assert_eq!(entry.stat.size, entry.path.len() as u64);
        assert!(entry.stat.mtime_ns > 0 && entry.stat.ctime_ns > 0);
    });
    // The same odd names transfer, `-` included (never read as stdin).
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

fn symlinks_are_never_read_as_files(h: &dyn Harness) {
    let t = h.tablet();
    h.put("target", b"secret");
    h.put_symlink(&format!("{DOC}.metadata"), "target");
    let link = path(&format!("{DOC}.metadata"));
    let listing = t.stat_all().unwrap();
    let entry = listing
        .entries
        .iter()
        .find(|entry| entry.path == link.as_str())
        .unwrap();
    assert!(!entry.is_regular());
    assert!(t.read_files(std::slice::from_ref(&link)).is_err());
    assert!(t.sha256(std::slice::from_ref(&link)).is_err());
    assert!(download_all(t, std::slice::from_ref(&link)).is_err());
}

fn unreadable_entries_fail_the_stat_pass(h: &dyn Harness) {
    if running_as_root() {
        eprintln!("running as root; skipping the unreadable-entry scenario");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        h.put("visible", b"x");
        h.put(&format!("{DOC}/p1.rm"), b"ink");
        let dir = h.data_dir().join(DOC);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let result = h.tablet().stat_all();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        // A partial listing would read as deleted pages: it must fail.
        assert!(result.is_err(), "{result:?}");
        assert_eq!(h.tablet().stat_all().unwrap().entries.len(), 2);
    }
}

type Scenario = fn(&dyn Harness);

const SCENARIOS: &[(&str, Scenario)] = &[
    ("lock_lifecycle", lock_lifecycle),
    ("reboot_frees_the_lock", reboot_frees_the_lock),
    ("reboot_ends_the_run", reboot_ends_the_run),
    ("fired_watchdog_ends_the_run", fired_watchdog_ends_the_run),
    (
        "watchdog_fires_without_the_state_dir",
        watchdog_fires_without_the_state_dir,
    ),
    (
        "takeover_keeps_the_previous_watchdog",
        takeover_keeps_the_previous_watchdog,
    ),
    ("xochitl_stop_and_start", xochitl_stop_and_start),
    ("state_round_trip", state_round_trip),
    ("stage_apply_read_and_delete", stage_apply_read_and_delete),
    (
        "staged_files_lists_what_is_not_moved_yet",
        staged_files_lists_what_is_not_moved_yet,
    ),
    ("apply_sets_planned_modes", apply_sets_planned_modes),
    (
        "apply_refuses_unstaged_files_before_any_change",
        apply_refuses_unstaged_files_before_any_change,
    ),
    (
        "apply_stops_when_the_watchdog_fired",
        apply_stops_when_the_watchdog_fired,
    ),
    ("stat_pass_lists_odd_names", stat_pass_lists_odd_names),
    (
        "symlinks_are_never_read_as_files",
        symlinks_are_never_read_as_files,
    ),
    (
        "unreadable_entries_fail_the_stat_pass",
        unreadable_entries_fail_the_stat_pass,
    ),
];

// ---- the fake ------------------------------------------------------------------

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
fn fake_fails_reads_too() {
    let h = FakeHarness::with(|b| b.file(format!("{DOC}.metadata"), "{}"));
    let t = &h.tablet;
    let file = path(&format!("{DOC}.metadata"));
    // FailOp counts every operation, reads included.
    t.inject(Fault::FailOp(1));
    assert!(transport(&t.stat_all()));
    t.stat_all().unwrap();
    t.inject(Fault::FailOp(2));
    t.read_state().unwrap();
    assert!(transport(&t.sha256(std::slice::from_ref(&file))));
    t.inject(Fault::FailOp(1));
    assert!(transport(&t.read_files(std::slice::from_ref(&file))));
    // DisconnectAtOp: that and every later operation fail.
    t.inject(Fault::DisconnectAtOp(1));
    assert!(transport(&t.read_state()));
    assert!(transport(&t.stat_all()));
    t.reconnect();
    t.stat_all().unwrap();
}

#[test]
fn fake_fails_a_download_part_way() {
    let h = FakeHarness::with(|b| {
        b.file("a", "1")
            .file("b", "2")
            .fault(Fault::FailDownloadAfter(1))
    });
    let t = &h.tablet;
    let mut seen = Vec::new();
    let result = t.download(&[path("a"), path("b")], &mut |file, _, _| {
        seen.push(file.to_string());
        Ok(())
    });
    assert!(transport(&result));
    assert_eq!(seen, vec!["a"]);
    assert_eq!(download_all(t, &[path("a"), path("b")]).unwrap().len(), 2);
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
    assert!(t.fire_watchdog());
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
fn fake_handles_a_lost_start_reply_like_a_real_tablet() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    // The start happened but its reply was lost: xochitl is up, time 0.
    t.inject(Fault::LoseAck(1));
    assert_eq!(t.start_xochitl(&lock).unwrap(), 0);
    // The start never ran but xochitl was up anyway: also 0.
    t.inject(Fault::FailMutation(1));
    assert_eq!(t.start_xochitl(&lock).unwrap(), 0);
    // The start never ran and xochitl is down: an error.
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.inject(Fault::FailMutation(1));
    assert!(device_error(&t.start_xochitl(&lock), |e| matches!(
        e,
        DeviceError::XochitlStart(_)
    )));
    // The connection is gone: the error as it is.
    t.inject(Fault::DisconnectAfter(1));
    assert!(transport(&t.start_xochitl(&lock)));
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
    // Five moves: two pages, highlights, content, metadata.
    let plan = ApplyPlan {
        docs: vec![full_write()],
    };
    t.inject(Fault::KillApplyAfter(4));
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
    t.inject(Fault::KillApplyAfter(5));
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
fn fake_deletes_metadata_before_the_rest_of_the_set() {
    let h = FakeHarness::with(|b| {
        b.file(format!("{DOC}.metadata"), "{}")
            .file(format!("{DOC}.content"), "{}")
            .file(format!("{DOC}/p1.rm"), "ink")
    });
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.inject(Fault::KillApplyAfter(1));
    let plan = ApplyPlan {
        docs: vec![DocApply::Delete { doc: doc(DOC) }],
    };
    assert!(t.apply(&lock, &plan).is_err());
    // An interrupted deletion leaves invisible orphans, not half a document.
    assert_eq!(h.get(&format!("{DOC}.metadata")), None);
    assert!(h.get(&format!("{DOC}.content")).is_some());
    assert!(h.get(&format!("{DOC}/p1.rm")).is_some());
}

#[test]
fn fake_watchdog_firing_mid_apply_stops_at_the_next_document() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    let (first, mut files) = metadata_only(OTHER);
    files.extend(document_files());
    stage(t, &lock, &files).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.inject(Fault::FireWatchdogAfterMoves(1));
    let result = t.apply(
        &lock,
        &ApplyPlan {
            docs: vec![first, full_write()],
        },
    );
    assert!(watchdog_fired(result));
    assert!(h.get(&format!("{OTHER}.metadata")).is_some());
    assert_eq!(h.get(&format!("{DOC}.content")), None);
    assert!(t.is_running());
}

#[test]
fn fake_watchdog_armed_before_a_start_does_not_cover_a_later_stop() {
    let h = FakeHarness::new();
    let t = &h.tablet;
    let lock = t.acquire_lock(&request(), false).unwrap();
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
    t.start_xochitl(&lock).unwrap();
    // Step 13 must re-arm: the step-3 timer may have fired or expired.
    assert!(device_error(&t.stop_xochitl(&lock), |e| matches!(
        e,
        DeviceError::ContractViolation(_)
    )));
    t.arm_watchdog(&lock, WATCHDOG).unwrap();
    t.stop_xochitl(&lock).unwrap();
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
    assert!(violation(t.disarm_watchdog(&lock)));
    t.apply(&lock, &plan).unwrap();
    assert_eq!(
        t.ops(),
        vec![
            OpKind::AcquireLock,
            OpKind::StopXochitl,
            OpKind::Apply,
            OpKind::ArmWatchdog,
            OpKind::StopXochitl,
            OpKind::DisarmWatchdog,
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
    assert!(!t.fire_watchdog(), "nothing armed after a reboot");
}

// ---- the real implementation over a local mock transport ------------------------

#[cfg(target_os = "linux")]
mod ssh_tablet {
    use super::super::harness::local_ssh::{SshHarness, Tools, busybox};
    use super::*;

    /// Run `check` with the host's tools and, if available, BusyBox.
    fn on_both(systemd_run: bool, check: impl Fn(&SshHarness)) {
        check(&SshHarness::new(Tools::Host, systemd_run));
        match busybox() {
            Some(busybox) => check(&SshHarness::new(Tools::BusyBox(&busybox), systemd_run)),
            None => eprintln!("RMU_TEST_BUSYBOX is not set; skipping the BusyBox run"),
        }
    }

    fn run_contract(tools: Tools<'_>, label: &str) {
        SCENARIOS.iter().for_each(|(name, scenario)| {
            eprintln!("{label}: {name}");
            let harness = SshHarness::new(tools, true);
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

    #[test]
    fn honours_the_contract() {
        run_contract(Tools::Host, "ssh");
    }

    #[test]
    fn honours_the_contract_on_busybox() {
        let Some(busybox) = busybox() else {
            eprintln!("RMU_TEST_BUSYBOX is not set; skipping the BusyBox contract run");
            return;
        };
        run_contract(Tools::BusyBox(&busybox), "busybox");
    }

    #[test]
    fn rearms_before_removing_the_old_watchdog() {
        let harness = SshHarness::new(Tools::Host, true);
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

    #[test]
    fn detects_cloud_pairing_and_fails_closed() {
        on_both(true, |harness| {
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
        });
    }

    fn check_sleep_watchdog(harness: &SshHarness) {
        if !harness.has_tool("setsid") || !harness.has_tool("flock") {
            eprintln!("setsid or flock not available; skipping the sleep-watchdog test");
            return;
        }
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
        let record = || fs::read_to_string(paths.watchdog_pid()).unwrap();
        let pid = || record().split(' ').next().unwrap().to_string();
        // Re-arming replaces the sleeper; the record names this run; the
        // disarm kills it and marks the token before it could fire.
        t.arm_watchdog(&lock, Duration::from_secs(60)).unwrap();
        let first = pid();
        assert!(record().trim_end().ends_with(lock.run()), "{}", record());
        t.arm_watchdog(&lock, Duration::from_secs(60)).unwrap();
        let second = pid();
        assert_ne!(first, second);
        assert!(gone(&first), "the replaced sleeper survived");
        t.disarm_watchdog(&lock).unwrap();
        assert!(!Path::new(&paths.watchdog_pid()).exists());
        assert_eq!(
            fs::read_to_string(paths.watchdog_token()).unwrap(),
            "disarmed\n"
        );
        assert!(gone(&second), "the sleeper survived the disarm");

        // A pid file naming an invalid group is never signalled.
        fs::write(paths.watchdog_pid(), format!("1 x {}\n", lock.run())).unwrap();
        t.arm_watchdog(&lock, Duration::from_secs(60)).unwrap();
        let third = pid();
        t.disarm_watchdog(&lock).unwrap();
        assert!(gone(&third));

        // Armed briefly, with xochitl stopped, and the state dir wiped
        // (token gone): it still fires.
        t.arm_watchdog(&lock, Duration::from_secs(3)).unwrap();
        t.stop_xochitl(&lock).unwrap();
        assert!(!harness.running());
        fs::remove_dir_all(&paths.state_dir).unwrap();
        let fired = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            harness.running()
        });
        assert!(fired, "the sleep watchdog did not fire");
        let log = harness.systemctl_log();
        assert!(
            log.ends_with("reset-failed xochitl.service\nstart xochitl.service\n"),
            "{log}"
        );
    }

    #[test]
    fn sleep_watchdog_fires_and_disarms() {
        on_both(false, check_sleep_watchdog);
    }

    #[test]
    fn takeover_is_a_compare_and_swap() {
        // The takeover replaces the record only if its checksum is still
        // the one inspected: a record that changed meanwhile is never
        // overwritten.
        let harness = SshHarness::new(Tools::Host, true);
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

    #[test]
    fn guard_flock_serialises_and_the_state_write_rechecks_the_guard() {
        on_both(true, |harness| {
            let t = harness.ssh_tablet();
            let lock = t.acquire_lock(&request(), false).unwrap();
            let marker = crate::ssh::shell_quote(&t.paths().watchdog_marker());
            // Another holder of the guard flock leaves the marker while
            // the state write waits for the flock after its upload.
            let mut holder = harness.hold_guard_flock(&format!("sleep 1; : > {marker}; sleep 1"));
            let started = Instant::now();
            let result = t.write_state(&lock, &hub_state());
            let waited = started.elapsed();
            holder.wait().unwrap();
            assert!(watchdog_fired(result));
            assert!(waited >= Duration::from_millis(1500), "{waited:?}");
            assert_eq!(t.read_state().unwrap(), None);
        });
    }

    #[test]
    fn apply_checks_the_guard_before_every_document() {
        on_both(true, |harness| {
            let t = harness.ssh_tablet();
            let lock = t.acquire_lock(&request(), false).unwrap();
            let (first, mut files) = metadata_only(OTHER);
            files.extend(document_files());
            stage(t, &lock, &files).unwrap();
            t.arm_watchdog(&lock, WATCHDOG).unwrap();
            t.stop_xochitl(&lock).unwrap();
            // The watchdog "fires" right after the first move.
            harness.marker_after_next_move();
            let result = t.apply(
                &lock,
                &ApplyPlan {
                    docs: vec![first, full_write()],
                },
            );
            assert!(watchdog_fired(result));
            assert!(harness.get(&format!("{OTHER}.metadata")).is_some());
            assert_eq!(harness.get(&format!("{DOC}.content")), None);
        });
    }
}
