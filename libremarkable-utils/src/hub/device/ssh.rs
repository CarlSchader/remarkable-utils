//! [`SshTablet`]: the device layer over SSH. Thin: every operation sends
//! one script generated in `scripts` and parses its output there.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::lock::{self, AcquireProbe, HeldLock, LockRequest, LockState};
use super::scripts::{self, EXIT_LOCK_CHANGED, EXIT_LOCK_LOST, EXIT_WATCHDOG_FIRED};
use super::{
    ApplyPlan, ApplyReport, DeviceError, DeviceInfo, DownloadVisitor, ExactReader, StageContent,
    StatListing, Tablet, TabletPaths, timeout_secs, transfer_paths,
};
use crate::error::{Error, Result};
use crate::hub::model::{TabletState, XochitlPath};
use crate::progress::{NoProgress, Progress};
use crate::ssh::SshSession;
use crate::status;

/// The device layer over SSH.
pub struct SshTablet {
    session: SshSession,
    paths: TabletPaths,
    progress: Arc<dyn Progress>,
}

impl SshTablet {
    /// A tablet at the standard paths.
    pub fn new(session: SshSession) -> Self {
        Self {
            session,
            paths: TabletPaths::default(),
            progress: Arc::new(NoProgress),
        }
    }

    /// Use other paths (tests; a real tablet always uses the defaults).
    pub fn with_paths(mut self, paths: TabletPaths) -> Self {
        self.paths = paths;
        self
    }

    /// Attach a progress observer; long operations report steps.
    pub fn with_progress(mut self, progress: Arc<dyn Progress>) -> Self {
        self.progress = progress;
        self
    }

    pub fn paths(&self) -> &TabletPaths {
        &self.paths
    }

    #[cfg(test)]
    pub(super) fn session_for_tests(&self) -> &SshSession {
        &self.session
    }

    /// Run a script; a non-zero exit is an error (the guard's exit
    /// statuses become their typed errors).
    fn script(&self, script: &str) -> Result<Output> {
        checked(self.session.run_script(script)?)
    }

    fn script_text(&self, script: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.script(script)?.stdout).into_owned())
    }

    fn guarded_script(&self, lock: &HeldLock, body: &str) -> Result<Output> {
        self.script(&scripts::guarded(&self.paths, lock.run(), body))
    }

    /// Run `step` as a progress step, finishing the progress UI after.
    fn step<T>(&self, message: &str, step: impl FnOnce() -> Result<T>) -> Result<T> {
        self.progress.step(message);
        let result = step();
        self.progress.finished();
        result
    }
}

fn guard_error(status: Option<i32>) -> Option<DeviceError> {
    match status {
        Some(EXIT_WATCHDOG_FIRED) => Some(DeviceError::WatchdogFired),
        Some(EXIT_LOCK_LOST) => Some(DeviceError::LockLost),
        _ => None,
    }
}

fn checked(output: Output) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    Err(remote_error(&output))
}

fn remote_error(output: &Output) -> Error {
    guard_error(output.status.code()).map_or_else(
        || Error::Remote {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Error::from,
    )
}

/// Guard exit statuses from checked session calls as typed errors.
fn map_guard<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| match error {
        Error::Remote { status, stderr } => {
            guard_error(Some(status)).map_or(Error::Remote { status, stderr }, Error::from)
        }
        other => other,
    })
}

/// Read a tar stream of exactly `files` (names may carry a `./` prefix),
/// handing each regular-file entry to `visit` without buffering it.
pub(super) fn read_tar(
    reader: &mut dyn Read,
    files: &[XochitlPath],
    visit: &mut DownloadVisitor<'_>,
) -> Result<()> {
    let wanted: BTreeMap<&str, &XochitlPath> =
        files.iter().map(|file| (file.as_str(), file)).collect();
    let download = |detail: String| Error::from(DeviceError::Download(detail));
    let mut archive = tar::Archive::new(reader);
    let seen = archive
        .entries()?
        .try_fold(BTreeSet::new(), |mut seen, entry| {
            let mut entry = entry?;
            let raw = entry.path_bytes().into_owned();
            let name = std::str::from_utf8(&raw)
                .map_err(|_| download(format!("non-UTF-8 entry {}", raw.escape_ascii())))?;
            let name = name.strip_prefix("./").unwrap_or(name);
            let path = *wanted
                .get(name)
                .ok_or_else(|| download(format!("unexpected entry {name:?}")))?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                return Err(download(format!("{name} is not a regular file")));
            }
            if !seen.insert(name.to_string()) {
                return Err(download(format!("{name} arrived twice")));
            }
            let size = entry.size();
            let mut counted = Counted {
                inner: &mut entry,
                count: 0,
            };
            visit(path, size, &mut counted)?;
            // Skip what the visitor left; a short entry is a truncated
            // archive, which the tar reader itself does not report.
            std::io::copy(&mut counted, &mut std::io::sink())?;
            if counted.count != size {
                return Err(download(format!(
                    "{name}: archive truncated ({} of {size} bytes)",
                    counted.count
                )));
            }
            Ok(seen)
        })?;
    let missing: Vec<&str> = wanted
        .keys()
        .filter(|name| !seen.contains(**name))
        .copied()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(download(format!("missing {}", missing.join(", "))))
    }
}

/// Counts the bytes read through it.
struct Counted<'r> {
    inner: &'r mut dyn Read,
    count: u64,
}

impl Read for Counted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        Ok(n)
    }
}

/// Build the staging tar: directory entries for every parent, then each
/// file from `content`, size-checked.
pub(super) fn write_stage_tar(
    out: &mut dyn Write,
    files: &[XochitlPath],
    content: &mut StageContent<'_>,
) -> Result<()> {
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let header = |kind, size, mode| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(mode);
        header.set_mtime(mtime);
        header
    };
    let mut builder = tar::Builder::new(out);
    let dirs: BTreeSet<&str> = files
        .iter()
        .flat_map(|file| {
            let path = file.as_str();
            path.match_indices('/').map(move |(at, _)| &path[..at])
        })
        .collect();
    dirs.iter().try_for_each(|dir| {
        builder.append_data(
            &mut header(tar::EntryType::Directory, 0, 0o755),
            dir,
            std::io::empty(),
        )
    })?;
    files.iter().try_for_each(|file| {
        let mut calls = 0;
        content(file, &mut |size, reader| {
            calls += 1;
            if calls > 1 {
                return Err(stage_error(file, "content supplied twice"));
            }
            let mut exact = ExactReader::new(reader, size);
            let appended = builder.append_data(
                &mut header(tar::EntryType::Regular, size, 0o644),
                file.as_str(),
                &mut exact,
            );
            match (appended, exact.problem) {
                (Err(_), Some(problem)) => Err(stage_error(file, &problem)),
                (result, _) => Ok(result?),
            }
        })?;
        if calls == 1 {
            Ok(())
        } else {
            Err(stage_error(file, "no content supplied"))
        }
    })?;
    builder.into_inner()?.flush()?;
    Ok(())
}

fn stage_error(file: &XochitlPath, detail: &str) -> Error {
    DeviceError::Stage {
        path: file.to_string(),
        detail: detail.to_string(),
    }
    .into()
}

impl Tablet for SshTablet {
    fn acquire_lock(&self, request: &LockRequest, break_lock: bool) -> Result<HeldLock> {
        let probe = || -> Result<AcquireProbe> {
            Ok(scripts::parse_acquire(&self.script_text(
                &scripts::acquire_script(&self.paths, request),
            )?)?)
        };
        let take_over = |version: &str| -> Result<Option<String>> {
            let output = self.session.run_script(&scripts::takeover_script(
                &self.paths,
                request,
                version,
            ))?;
            match output.status.code() {
                Some(EXIT_LOCK_CHANGED) => Ok(None),
                _ => Ok(Some(scripts::parse_boot(&String::from_utf8_lossy(
                    &checked(output)?.stdout,
                ))?)),
            }
        };
        lock::acquire(request, break_lock, &probe, &take_over)
    }

    fn release_lock(&self, lock: &HeldLock) -> Result<()> {
        self.script(&scripts::release_script(&self.paths, lock.run()))?;
        Ok(())
    }

    fn lock_state(&self) -> Result<LockState> {
        Ok(scripts::parse_lock_state(
            &self.script_text(&scripts::lock_state_script(&self.paths))?,
        )?)
    }

    fn check_guard(&self, lock: &HeldLock) -> Result<()> {
        self.guarded_script(lock, "true")?;
        Ok(())
    }

    fn watchdog_fired(&self) -> Result<bool> {
        Ok(scripts::parse_bool(
            "watchdog marker",
            &self.script_text(&scripts::marker_script(&self.paths))?,
        )?)
    }

    fn arm_watchdog(&self, lock: &HeldLock, timeout: Duration) -> Result<()> {
        let token = lock::new_run_id();
        self.script(&scripts::arm_script(
            &self.paths,
            lock.run(),
            timeout_secs(timeout),
            &token,
        ))?;
        Ok(())
    }

    fn disarm_watchdog(&self, lock: &HeldLock) -> Result<()> {
        self.script(&scripts::disarm_script(&self.paths, lock.run()))?;
        Ok(())
    }

    fn stop_xochitl(&self, lock: &HeldLock) -> Result<()> {
        self.step("Stopping xochitl", || {
            self.script(&scripts::stop_script(&self.paths, lock.run()))?;
            Ok(())
        })
    }

    fn start_xochitl(&self, lock: &HeldLock) -> Result<i64> {
        self.step("Starting xochitl", || {
            let output = self
                .session
                .run_script(&scripts::start_script(&self.paths, lock.run()))?;
            if output.status.success() {
                return Ok(scripts::parse_device_time(
                    "start xochitl",
                    &String::from_utf8_lossy(&output.stdout),
                )?);
            }
            if let Some(error) = guard_error(output.status.code()) {
                return Err(error.into());
            }
            // As in `Client::restart_xochitl`: the command channel can
            // drop while xochitl comes back; check before failing. The
            // time printed before the start is lost then, and a later one
            // would not be conservative: report the earliest possible.
            if self.xochitl_active()? {
                return Ok(0);
            }
            Err(DeviceError::XochitlStart(format!(
                "exit {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into())
        })
    }

    fn xochitl_active(&self) -> Result<bool> {
        let output = self.session.run_script(scripts::IS_ACTIVE_SCRIPT)?;
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // `is-active` exits non-zero for every state but `active`; 255 is
        // ssh's own failure.
        if output.status.code() == Some(255) || state.is_empty() {
            return Err(remote_error(&output));
        }
        Ok(state == "active")
    }

    fn cloud_paired(&self) -> Result<bool> {
        Ok(scripts::parse_bool(
            "cloud pairing check",
            &self.script_text(&scripts::cloud_paired_script(&self.paths))?,
        )?)
    }

    fn device_info(&self) -> Result<DeviceInfo> {
        let marker = format!("===RMU:{}===", uuid::Uuid::new_v4().simple());
        let status =
            status::parse_status(&marker, &self.script_text(&status::status_script(&marker))?);
        Ok(DeviceInfo {
            model: status.model,
            firmware: status.os_version,
        })
    }

    fn stat_all(&self) -> Result<StatListing> {
        self.step("Listing tablet files", || {
            Ok(scripts::parse_stat_output(
                &self.script_text(&scripts::stat_script(&self.paths))?,
            )?)
        })
    }

    fn download(&self, files: &[XochitlPath], visit: &mut DownloadVisitor<'_>) -> Result<()> {
        if files.is_empty() {
            // BusyBox tar fails on an empty list.
            return Ok(());
        }
        let names = transfer_paths(files)?;
        self.step(&format!("Downloading {} file(s)", files.len()), || {
            let mut stream = self.session.stream_stdout(
                &scripts::download_command(&self.paths),
                scripts::download_list(names),
                &*self.progress,
            )?;
            let result = read_tar(&mut stream, files, visit);
            stream.finish(result)
        })
    }

    fn read_files(&self, files: &[XochitlPath]) -> Result<BTreeMap<XochitlPath, Option<Vec<u8>>>> {
        if files.is_empty() {
            return Ok(BTreeMap::new());
        }
        let names = transfer_paths(files)?;
        let output = self.script(&scripts::read_files_script(&self.paths, names))?;
        let frames = scripts::parse_read_frames(&output.stdout, files.len())?;
        Ok(files.iter().cloned().zip(frames).collect())
    }

    fn sha256(&self, files: &[XochitlPath]) -> Result<BTreeMap<XochitlPath, Option<String>>> {
        if files.is_empty() {
            return Ok(BTreeMap::new());
        }
        let names = transfer_paths(files)?;
        self.step(
            &format!("Hashing {} file(s) on the tablet", files.len()),
            || {
                let output = self.script_text(&scripts::sha256_script(&self.paths, &names))?;
                let hashes = scripts::parse_sha256(&output, &names)?;
                Ok(files
                    .iter()
                    .cloned()
                    .zip(hashes.into_iter().map(|(_, hash)| hash))
                    .collect())
            },
        )
    }

    fn read_state(&self) -> Result<Option<TabletState>> {
        let output = self.script(&scripts::read_state_script(&self.paths))?;
        scripts::parse_state_output(&output.stdout)?
            .map(|bytes| Ok(TabletState::parse(bytes)?))
            .transpose()
    }

    fn write_state(&self, lock: &HeldLock, state: &TabletState) -> Result<()> {
        let json = state.to_json();
        let command = scripts::write_state_command(&self.paths, lock.run(), json.len());
        map_guard(
            self.session
                .run_checked_with_writer(&command, |stdin| Ok(stdin.write_all(json.as_bytes())?)),
        )
    }

    fn stage(
        &self,
        lock: &HeldLock,
        files: &[XochitlPath],
        content: &mut StageContent<'_>,
    ) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        transfer_paths(files)?;
        self.step(
            &format!("Staging {} file(s) on the tablet", files.len()),
            || {
                let command =
                    scripts::stage_command(&self.paths, lock.run(), &self.paths.staging_dir(lock));
                map_guard(self.session.run_checked_with_writer(&command, |stdin| {
                    write_stage_tar(stdin, files, content)
                }))
            },
        )
    }

    fn clear_staging(&self, lock: &HeldLock) -> Result<()> {
        self.script(&scripts::clear_staging_script(&self.paths, lock.run()))?;
        Ok(())
    }

    fn apply(&self, lock: &HeldLock, plan: &ApplyPlan) -> Result<ApplyReport> {
        let steps = plan.steps()?;
        let written = plan.written();
        self.step(&format!("Applying {} document(s)", plan.docs.len()), || {
            let output = self.session.run_script(&scripts::apply_script(
                &self.paths,
                lock.run(),
                &self.paths.staging_dir(lock),
                &steps,
                &written,
            ))?;
            if let Some(error) = guard_error(output.status.code()) {
                return Err(error.into());
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed = scripts::parse_apply_output(&stdout);
            match (output.status.success(), parsed) {
                (true, Ok(parsed)) if parsed.complete && parsed.written.len() == written.len() => {
                    Ok(ApplyReport {
                        applied: parsed.applied,
                        written: parsed.written,
                        recorded_at_ns: parsed.recorded_at_ns.ok_or_else(|| {
                            DeviceError::Output {
                                operation: "apply",
                                detail: "no device time".to_string(),
                            }
                        })?,
                    })
                }
                (success, _) => Err(DeviceError::ApplyIncomplete {
                    applied: scripts::applied_documents(&stdout),
                    reason: if success {
                        "incomplete output".to_string()
                    } else {
                        format!(
                            "exit {:?}: {}",
                            output.status.code(),
                            String::from_utf8_lossy(&output.stderr).trim()
                        )
                    },
                }
                .into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> XochitlPath {
        XochitlPath::new(text).unwrap()
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        entries.iter().for_each(|(name, data)| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, name, *data).unwrap();
        });
        builder.into_inner().unwrap()
    }

    fn collect(archive: &[u8], files: &[XochitlPath]) -> Result<Vec<(String, Vec<u8>)>> {
        let mut seen = Vec::new();
        read_tar(&mut &archive[..], files, &mut |file, size, reader| {
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            assert!(data.len() as u64 <= size);
            seen.push((file.to_string(), data));
            Ok(())
        })?;
        Ok(seen)
    }

    #[test]
    fn tar_entries_stream_to_the_visitor() {
        let long = format!("{}/{}.rm", "d".repeat(36), "p".repeat(120));
        let archive = tar_of(&[("./a.metadata", b"{}"), (&long, b"ink"), ("./e", b"")]);
        let files = [path("a.metadata"), path(&long), path("e")];
        assert_eq!(
            collect(&archive, &files).unwrap(),
            vec![
                ("a.metadata".to_string(), b"{}".to_vec()),
                (long.clone(), b"ink".to_vec()),
                ("e".to_string(), Vec::new()),
            ]
        );
    }

    #[test]
    fn tar_contents_must_match_the_request() {
        let archive = tar_of(&[("a", b"1")]);
        let download_error =
            |result: Result<_>| matches!(result, Err(Error::HubDevice(DeviceError::Download(_))));
        assert!(download_error(collect(&archive, &[path("a"), path("b")])));
        assert!(download_error(collect(&archive, &[])));
        assert!(download_error(collect(
            &tar_of(&[("a", b"1"), ("a", b"1")]),
            &[path("a")]
        )));
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder.append_link(&mut header, "a", "elsewhere").unwrap();
        assert!(download_error(collect(
            &builder.into_inner().unwrap(),
            &[path("a")]
        )));
        // A truncated archive is an error, not a short file, whether the
        // visitor reads everything or nothing.
        let truncated = &tar_of(&[("a", &[7; 2000])])[..1024];
        assert!(download_error(collect(truncated, &[path("a")])));
        assert!(download_error(
            read_tar(&mut &truncated[..], &[path("a")], &mut |_, _, _| Ok(())).map(|()| Vec::new())
        ));
        // Unread bytes are skipped.
        let archive = tar_of(&[("a", b"skipped"), ("b", b"read")]);
        let mut got = Vec::new();
        read_tar(
            &mut &archive[..],
            &[path("a"), path("b")],
            &mut |file, _, reader| {
                if file.as_str() == "b" {
                    reader.read_to_end(&mut got)?;
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(got, b"read");
    }

    #[test]
    fn stage_tar_has_parents_and_exact_sizes() {
        let files = [
            path("u/p.rm"),
            path("u.highlights/h.json"),
            path("u.content"),
        ];
        let mut out = Vec::new();
        write_stage_tar(&mut out, &files, &mut |file, sink| {
            let data = file.as_str().as_bytes().to_vec();
            sink(data.len() as u64, &mut &data[..])
        })
        .unwrap();
        let mut archive = tar::Archive::new(&out[..]);
        let entries: Vec<(String, tar::EntryType, Vec<u8>)> = archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let kind = entry.header().entry_type();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                (name, kind, data)
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                ("u".into(), tar::EntryType::Directory, vec![]),
                ("u.highlights".into(), tar::EntryType::Directory, vec![]),
                ("u/p.rm".into(), tar::EntryType::Regular, b"u/p.rm".to_vec()),
                (
                    "u.highlights/h.json".into(),
                    tar::EntryType::Regular,
                    b"u.highlights/h.json".to_vec()
                ),
                (
                    "u.content".into(),
                    tar::EntryType::Regular,
                    b"u.content".to_vec()
                ),
            ]
        );
    }

    #[test]
    fn stage_tar_refuses_misbehaving_content() {
        let stage = |content: &mut StageContent<'_>| {
            write_stage_tar(&mut Vec::new(), &[path("f")], content)
        };
        let stage_error =
            |result: Result<()>| matches!(result, Err(Error::HubDevice(DeviceError::Stage { .. })));
        assert!(stage_error(stage(&mut |_, sink| sink(5, &mut &b"abc"[..]))));
        assert!(stage_error(stage(&mut |_, sink| sink(1, &mut &b"abc"[..]))));
        assert!(stage_error(stage(&mut |_, _| Ok(()))));
        assert!(stage_error(stage(&mut |_, sink| {
            sink(1, &mut &b"a"[..])?;
            sink(1, &mut &b"a"[..])
        })));
        // Errors from the content source pass through unchanged.
        assert!(matches!(
            stage(&mut |_, _| Err(Error::InvalidName("source".into()))),
            Err(Error::InvalidName(_))
        ));
    }

    #[test]
    fn guard_statuses_become_typed_errors() {
        assert!(matches!(
            map_guard::<()>(Err(Error::Remote {
                status: EXIT_WATCHDOG_FIRED,
                stderr: String::new()
            })),
            Err(Error::HubDevice(DeviceError::WatchdogFired))
        ));
        assert!(matches!(
            map_guard::<()>(Err(Error::Remote {
                status: EXIT_LOCK_LOST,
                stderr: String::new()
            })),
            Err(Error::HubDevice(DeviceError::LockLost))
        ));
        assert!(matches!(
            map_guard::<()>(Err(Error::Remote {
                status: 1,
                stderr: "x".into()
            })),
            Err(Error::Remote { status: 1, .. })
        ));
    }
}
