//! Local-only subprocess regressions. Run the scenarios in a separate process
//! group so a pipe deadlock fails within a deadline and leaves no mock children.
use super::*;
use std::cell::Cell;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::time::{Duration, Instant};

const PIPE_FILL: usize = 1024 * 1024;
const TEST_NAME: &str = "ssh::streaming_tests::transfers_drain_and_reap";
const CHILD_ENV: &str = "RMU_TEST_STREAMING_CHILD";

struct TestProcess(Child);

impl Drop for TestProcess {
    fn drop(&mut self) {
        // Also kill mock subprocesses if the test hit its deadline or panicked.
        let _ = Command::new("sh")
            .args(["-c", "kill -KILL -\"$1\" 2>/dev/null", "sh"])
            .arg(self.0.id().to_string())
            .status();
        let _ = self.0.wait();
    }
}

#[test]
fn transfers_drain_and_reap() {
    if std::env::var_os(CHILD_ENV).is_some() {
        (0..2).for_each(|_| {
            [0, 23].into_iter().for_each(check_transfers);
            check_early_errors();
            check_inherited_outputs();
        });
        return;
    }

    let mut process = TestProcess(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ENV, "1")
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "streaming scenarios failed: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "streaming subprocess deadlocked");
        thread::sleep(Duration::from_millis(10));
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rmu-ssh-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn session(&self, script: &str) -> SshSession {
        let exe = self.0.join("mock-ssh");
        fs::write(&exe, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let mut session = SshSession::new(SshOptions {
            destination: "mock.invalid".into(),
            multiplex: false,
            ..SshOptions::default()
        })
        .unwrap();
        session.ssh_exe = Some(exe);
        session
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Recorder {
    transferred: Cell<u64>,
    total: Cell<Option<u64>>,
    done: Cell<usize>,
}

impl Progress for Recorder {
    fn step(&self, _: &str) {}
    fn bytes(&self, transferred: u64, total: Option<u64>) {
        assert!(transferred > self.transferred.get());
        self.transferred.set(transferred);
        self.total.set(total);
    }
    fn bytes_done(&self) {
        self.done.set(self.done.get() + 1);
    }
    fn finished(&self) {}
}

fn assert_result(result: Result<()>, status: i32, progress: &Recorder, total: Option<u64>) {
    if status == 0 {
        result.unwrap();
    } else {
        match result.unwrap_err() {
            Error::Remote {
                status: actual,
                stderr,
            } => {
                assert_eq!(actual, status);
                assert_eq!(stderr, "\0".repeat(PIPE_FILL));
            }
            error => panic!("expected captured remote diagnostics, got {error:?}"),
        }
    }
    assert_eq!(progress.transferred.get(), PIPE_FILL as u64);
    assert_eq!(progress.total.get(), total);
    assert_eq!(progress.done.get(), usize::from(status == 0));
}

fn check_transfers(status: i32) {
    let scratch = Scratch::new();
    let pid_file = scratch.0.join("pid");
    let pid_preamble = format!("echo $$ > {}", shell_quote(pid_file.to_str().unwrap()));
    // More than pipe capacity on stderr before producing any stdout.
    let session = scratch.session(&format!(
        "{pid_preamble}\nhead -c {PIPE_FILL} /dev/zero >&2\nhead -c {PIPE_FILL} /dev/zero\nexit {status}"
    ));
    let progress = Recorder::default();
    let result = session.run_checked_bytes_hint("mock command", None, &progress);
    if let Ok(data) = &result {
        assert_eq!(data, &vec![0; PIPE_FILL]);
    }
    assert_result(result.map(|_| ()), status, &progress, None);
    assert_pid_file_reaped(&pid_file);

    let progress = Recorder::default();
    let local = scratch.0.join("download");
    fs::write(&local, b"existing backup").unwrap();
    let total = Some(PIPE_FILL as u64);
    let result = session.download_remote_file("mock remote", &local, total, &progress);
    assert_result(result, status, &progress, total);
    assert_eq!(
        fs::read(local).unwrap(),
        if status == 0 {
            vec![0; PIPE_FILL]
        } else {
            b"existing backup".to_vec()
        }
    );
    assert_no_download_temps(&scratch.0);
    assert_pid_file_reaped(&pid_file);

    // Fill both output pipes before consuming stdin. The input also exceeds
    // pipe capacity, so a synchronous writer cannot finish without draining.
    let captured = scratch.0.join("uploaded");
    let session = scratch.session(&format!(
        "{pid_preamble}\nhead -c {PIPE_FILL} /dev/zero >&2\nhead -c {PIPE_FILL} /dev/zero\ncat > {}\nexit {status}",
        shell_quote(captured.to_str().unwrap())
    ));
    let data = vec![7; PIPE_FILL];
    let progress = Recorder::default();
    let result = session.run_checked_with_stdin("mock command", &data, &progress);
    assert_result(result, status, &progress, total);
    assert_eq!(fs::read(&captured).unwrap(), data);
    assert_pid_file_reaped(&pid_file);

    let local = scratch.0.join("source");
    fs::write(&local, &data).unwrap();
    let progress = Recorder::default();
    let result = session.upload_local_file(&local, "mock remote", &progress);
    assert_result(result, status, &progress, total);
    assert_eq!(fs::read(&captured).unwrap(), data);
    assert_pid_file_reaped(&pid_file);
}

fn assert_no_download_temps(directory: &Path) {
    assert!(fs::read_dir(directory).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rmu-download-")
    }));
}

fn assert_pid_file_reaped(path: &Path) {
    assert_reaped(fs::read_to_string(path).unwrap().trim().parse().unwrap());
}

fn assert_reaped(pid: u32) {
    // kill -0 also succeeds for zombies, unlike merely checking exit status.
    assert!(
        !Command::new("sh")
            .args(["-c", "kill -0 \"$1\" 2>/dev/null", "sh"])
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success(),
        "child {pid} is still alive or unreaped"
    );
}

/// An independently owned helper holds the same output descriptors as SSH.
/// This models inherited descendant pipes without creating orphan test children.
struct PipeHolder(Child);

impl Drop for PipeHolder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn check_inherited_outputs() {
    use std::os::fd::OwnedFd;

    let (stdout_reader, stdout_writer) = UnixStream::pair().unwrap();
    let (stderr_reader, stderr_writer) = UnixStream::pair().unwrap();
    let mut holder = PipeHolder(
        Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(
                stdout_writer.try_clone().unwrap(),
            )))
            .stderr(Stdio::from(OwnedFd::from(
                stderr_writer.try_clone().unwrap(),
            )))
            .spawn()
            .unwrap(),
    );
    let cancellation = DrainCancellation::new().unwrap();
    let mut child = StreamingChild {
        child: Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(stdout_writer)))
            .stderr(Stdio::from(OwnedFd::from(stderr_writer)))
            .spawn()
            .unwrap(),
        stderr: None,
        stdout: None,
        reaped: false,
        cancellation,
    };
    let pid = child.child.id();
    child.stderr = Some(drain_pipe(stderr_reader, &child.cancellation).unwrap());
    child.stdout = Some(drain_pipe(stdout_reader, &child.cancellation).unwrap());
    assert!(holder.0.try_wait().unwrap().is_none());
    let result = child.finish(Err(std::io::Error::other("injected transfer failure")));
    assert!(matches!(result, Err(Error::Io(_))));
    assert_reaped(pid);
    // Cleanup returned and joined both readers while their peers remain open.
    assert!(holder.0.try_wait().unwrap().is_none());
    let holder_pid = holder.0.id();
    drop(holder);
    assert_reaped(holder_pid);
}

struct FailingIo;
impl Read for FailingIo {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("injected read failure"))
    }
}
impl Write for FailingIo {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("injected write failure"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn check_early_errors() {
    // A failed local read must kill the child, reap it and join both readers,
    // even though the child would otherwise keep its pipes open for a minute.
    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 60"]);
    let mut child = StreamingChild::spawn(&mut command, true).unwrap();
    let pid = child.child.id();
    let progress = Recorder::default();
    let transfer = {
        let mut stdin = child.child.stdin.take().unwrap();
        copy_with_progress(&mut FailingIo, &mut stdin, None, &progress)
    };
    assert!(matches!(child.finish(transfer), Err(Error::Io(_))));
    assert_eq!(progress.done.get(), 0);
    assert_reaped(pid);

    let mut command = Command::new("sh");
    command.args(["-c", "printf payload; exec sleep 60"]);
    let mut child = StreamingChild::spawn(&mut command, false).unwrap();
    let pid = child.child.id();
    let transfer = {
        let mut stdout = child.child.stdout.take().unwrap();
        copy_with_progress(&mut stdout, &mut FailingIo, None, &progress)
    };
    assert!(matches!(child.finish(transfer), Err(Error::Io(_))));
    assert_reaped(pid);

    // Exercise cleanup before finish (e.g. a transfer setup error).
    let child = StreamingChild::spawn(&mut command, false).unwrap();
    let pid = child.child.id();
    drop(child);
    assert_reaped(pid);

    let scratch = Scratch::new();
    let session = scratch.session("printf payload");
    let directory = scratch.0.join("destination-directory");
    fs::create_dir(&directory).unwrap();
    let result = session.download_remote_file("mock remote", &directory, None, &progress);
    assert!(matches!(result, Err(Error::Io(_))));
    assert!(directory.is_dir());
    assert_no_download_temps(&scratch.0);
    assert_eq!(progress.done.get(), 0);
}
