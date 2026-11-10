//! A read-only look at a tablet's hub state for the other `rmu` commands
//! (`upload`, `rm`, `sync`, ...): is an `rmu hub` run holding the lock,
//! and is the tablet attached to a hub at all.
//!
//! The probe script prints marker-delimited sections and never fails, so
//! it can ride along in another command's round trip (the document
//! listing every mutating command starts with). It reads the lock with
//! the hub's own format ([`parse_owner`]) and judges staleness with the
//! hub's rule ([`decide`]): a lock whose run cannot change anything any
//! more (the tablet rebooted, or the watchdog fired) does not hold other
//! commands back.
//!
//! The look is check-then-act, so each write also carries
//! [`HubProbe::write_guard`] in its own command, right before its commit
//! point: under the hub's guard `flock` it asserts that the lock is still
//! as the probe saw it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::lock::{LockDecision, LockOwner, LockState, StaleReason, decide, parse_owner};
use super::scripts::{EXIT_BUSY, take_flock};
use super::{DeviceError, TabletPaths};
use crate::ssh::shell_quote;

/// Exit status of a write whose guard found the hub lock changed since
/// the probe ([`HubProbe::write_guard`]): look again.
pub const EXIT_HUB_LOCK_CHANGED: i32 = 83;
/// Exit status of a write whose guard could not get the hub's guard
/// `flock` in time (a hub operation kept it).
pub const EXIT_HUB_BUSY: i32 = EXIT_BUSY;

const LOCK: &str = "__hub_lock__";
const BOOT: &str = "__hub_boot__";
const FIRED: &str = "__hub_fired__";
const ATTACHED: &str = "__hub_attached__";

/// What the probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubProbe {
    pub lock: LockState,
    /// The owner record as read (empty for a free lock, or a missing
    /// record), which a stale lock's write guard compares.
    pub lock_record: String,
    /// The tablet's current boot id (`None` if it could not be read).
    pub boot_id: Option<String>,
    /// The watchdog marker is present.
    pub watchdog_fired: bool,
    /// The tablet has a hub state file: it is attached to a hub.
    pub attached: bool,
}

/// The hub lock, as it concerns another command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubLock {
    Free,
    /// Held by a run that can no longer change anything (its guard
    /// fails): other commands may go ahead; the next hub run takes the
    /// lock over.
    Stale(StaleReason),
    /// Held by a live run, or one rmu cannot judge (`None`: the owner
    /// record is missing or unreadable): other writes must wait.
    Held(Option<LockOwner>),
}

impl HubProbe {
    /// The lock's verdict for other commands. An unreadable boot id
    /// never makes a lock stale.
    pub fn hub_lock(&self) -> HubLock {
        let LockState::Held(owner) = &self.lock else {
            return HubLock::Free;
        };
        // The hub's own rule, with no run of ours and no --break-lock:
        // only a reboot or a fired watchdog frees the lock. An unknown
        // boot id counts as the owner's.
        let boot = self
            .boot_id
            .as_deref()
            .or(owner.as_ref().map(|owner| owner.boot_id.as_str()))
            .unwrap_or_default();
        match decide(owner.as_ref(), "", boot, self.watchdog_fired, false) {
            LockDecision::TakeOver(reason) => HubLock::Stale(reason),
            LockDecision::Adopt | LockDecision::Refuse => HubLock::Held(owner.clone()),
        }
    }

    /// A snippet for a write's own shell command, right before its
    /// commit point (the `mv`, `rm`, `tar -x`, `cp` or restart): it exits
    /// [`EXIT_HUB_LOCK_CHANGED`] unless the lock is still as this probe
    /// saw it (absent; or, for a stale lock, its owner record unchanged
    /// or the lock gone). When the hub's state directory exists, it first
    /// takes the hub's guard `flock`, which a hub run holds to take the
    /// lock and to apply, and keeps it to the end of the command: the
    /// check and the write then cannot interleave with either
    /// ([`EXIT_HUB_BUSY`] after the hub's wait). Without that directory
    /// no hub has run on the tablet. `None` for a held lock: the write
    /// must not start.
    pub fn write_guard(&self, paths: &TabletPaths) -> Option<String> {
        let lock = shell_quote(&paths.lock_dir());
        let check = match self.hub_lock() {
            HubLock::Held(_) => return None,
            HubLock::Free => format!("[ ! -e {lock} ]"),
            HubLock::Stale(_) => format!(
                "{{ [ ! -e {lock} ] || [ \"$(cat -- {owner} 2>/dev/null)\" = {record} ]; }}",
                owner = shell_quote(&paths.lock_owner()),
                // `$(...)` drops trailing newlines.
                record = shell_quote(self.lock_record.trim_end_matches('\n')),
            ),
        };
        Some(format!(
            "if [ -d {state} ]; then\n\
             {flock}fi\n\
             {check} || {{ echo 'rmu: an rmu hub run took the tablet lock' >&2; exit {changed}; }}\n",
            state = shell_quote(&paths.state_dir),
            flock = take_flock(paths),
            changed = EXIT_HUB_LOCK_CHANGED,
        ))
    }
}

/// The last probe, trusted for a while ([`Self::get`]). The clock is
/// injectable, so the expiry is testable.
pub struct ProbeCache {
    ttl: Duration,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
    last: Mutex<Option<(Instant, HubProbe)>>,
}

impl ProbeCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Instant::now)
    }

    pub fn with_clock(ttl: Duration, clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self {
            ttl,
            clock: Box::new(clock),
            last: Mutex::new(None),
        }
    }

    /// The last probe, if it is younger than the TTL.
    pub fn get(&self) -> Option<HubProbe> {
        let now = (self.clock)();
        self.last
            .lock()
            .expect("probe cache mutex")
            .as_ref()
            .filter(|(at, _)| now.saturating_duration_since(*at) < self.ttl)
            .map(|(_, probe)| probe.clone())
    }

    pub fn store(&self, probe: HubProbe) {
        *self.last.lock().expect("probe cache mutex") = Some(((self.clock)(), probe));
    }

    pub fn forget(&self) {
        *self.last.lock().expect("probe cache mutex") = None;
    }
}

/// Print the probe's sections, each `\n<marker> <name>\n` then its body.
/// Read-only, and it never exits, so it can precede another script.
pub fn probe_script(paths: &TabletPaths, marker: &str) -> String {
    let marker = shell_quote(marker);
    let section = |name: &str| format!("printf '\\n%s %s\\n' {marker} {name}\n");
    format!(
        "{lock}if [ -d {lock_dir} ]; then printf 'held\\n'; cat -- {owner} 2>/dev/null; else printf 'free\\n'; fi\n\
         {boot}cat -- {boot_id} 2>/dev/null\n\
         {fired}if [ -e {fired_marker} ]; then echo true; else echo false; fi\n\
         {attached}if [ -e {state} ]; then echo true; else echo false; fi\n",
        lock = section(LOCK),
        boot = section(BOOT),
        fired = section(FIRED),
        attached = section(ATTACHED),
        lock_dir = shell_quote(&paths.lock_dir()),
        owner = shell_quote(&paths.lock_owner()),
        boot_id = shell_quote(&paths.boot_id),
        fired_marker = shell_quote(&paths.watchdog_marker()),
        state = shell_quote(&paths.state_file()),
    )
}

/// The body of section `name` in `output` (sections as printed by
/// [`probe_script`]); `None` if it is missing.
fn section<'a>(marker: &str, output: &'a str, name: &str) -> Option<&'a str> {
    let header = format!("\n{marker} {name}\n");
    let start = output.find(&header)? + header.len();
    let body = &output[start..];
    // The newline before the next header belongs to that header.
    let end = body.find(&format!("\n{marker} ")).unwrap_or(body.len());
    Some(&body[..end])
}

fn probe_error(detail: impl Into<String>) -> DeviceError {
    DeviceError::Output {
        operation: "hub probe",
        detail: detail.into(),
    }
}

/// The first line of a one-line section (output after the probe may
/// follow it without a marker).
fn first_line<'a>(marker: &str, output: &'a str, name: &str) -> Option<&'a str> {
    section(marker, output, name).map(|body| body.lines().next().unwrap_or_default().trim())
}

fn flag(marker: &str, output: &str, name: &str) -> Result<bool, DeviceError> {
    match first_line(marker, output, name) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        other => Err(probe_error(format!("{name}: {other:?}"))),
    }
}

/// Parse the probe's sections out of `output` (other sections around
/// them are ignored).
pub fn parse_probe(marker: &str, output: &str) -> Result<HubProbe, DeviceError> {
    let lock = section(marker, output, LOCK).ok_or_else(|| probe_error("no lock section"))?;
    let (lock, lock_record) = match lock.split_once('\n').unwrap_or((lock, "")) {
        ("free", "") => (LockState::Free, String::new()),
        ("held", owner) => (LockState::Held(parse_owner(owner)), owner.to_string()),
        _ => return Err(probe_error(format!("lock: {lock:?}"))),
    };
    let boot_id = first_line(marker, output, BOOT)
        .filter(|boot| !boot.is_empty())
        .map(str::to_string);
    Ok(HubProbe {
        lock,
        lock_record,
        boot_id,
        watchdog_fired: flag(marker, output, FIRED)?,
        attached: flag(marker, output, ATTACHED)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "===RMU:0123===";
    const BOOT_ID: &str = "0f0e0d0c-0000-4000-8000-0000000000b1";

    fn owner_record(boot: &str) -> String {
        format!(
            "run=0123456789abcdef0123456789abcdef\nhost=laptop\npid=4242\nstarted=1790000000\nboot_id={boot}\n"
        )
    }

    /// Probe output as the script prints it, after some other output.
    fn output(lock: &str, boot: &str, fired: bool, attached: bool) -> String {
        format!(
            "listing noise\n\n{MARKER} x.metadata\n{{}}\n\n{MARKER} {LOCK}\n{lock}\n{MARKER} {BOOT}\n{boot}\n\n{MARKER} {FIRED}\n{fired}\n\n{MARKER} {ATTACHED}\n{attached}\n"
        )
    }

    fn probe(lock: &str, boot: &str, fired: bool, attached: bool) -> HubProbe {
        parse_probe(MARKER, &output(lock, boot, fired, attached)).unwrap()
    }

    #[test]
    fn a_free_lock() {
        let probe = probe("free", BOOT_ID, false, true);
        assert_eq!(probe.lock, LockState::Free);
        assert_eq!(probe.boot_id.as_deref(), Some(BOOT_ID));
        assert!(probe.attached && !probe.watchdog_fired);
        assert_eq!(probe.hub_lock(), HubLock::Free);
    }

    #[test]
    fn a_live_lock_holds_other_commands_back() {
        let probe = probe(
            &format!("held\n{}", owner_record(BOOT_ID)),
            BOOT_ID,
            false,
            true,
        );
        match probe.hub_lock() {
            HubLock::Held(Some(owner)) => {
                assert_eq!((owner.host.as_str(), owner.pid), ("laptop", 4242));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stale_locks_do_not() {
        let rebooted = probe(
            &format!("held\n{}", owner_record("an-earlier-boot")),
            BOOT_ID,
            false,
            true,
        );
        assert_eq!(
            rebooted.hub_lock(),
            HubLock::Stale(StaleReason::BootChanged)
        );
        let fired = probe(
            &format!("held\n{}", owner_record(BOOT_ID)),
            BOOT_ID,
            true,
            true,
        );
        assert_eq!(fired.hub_lock(), HubLock::Stale(StaleReason::WatchdogFired));
        let fired_unknown = probe("held\n", BOOT_ID, true, true);
        assert_eq!(
            fired_unknown.hub_lock(),
            HubLock::Stale(StaleReason::WatchdogFired)
        );
    }

    #[test]
    fn what_cannot_be_judged_holds() {
        // No owner record (power cut while it was taken): only the
        // watchdog or --break-lock frees it.
        assert_eq!(
            probe("held\n", BOOT_ID, false, true).hub_lock(),
            HubLock::Held(None)
        );
        assert_eq!(
            probe("held\ngarbage\n", BOOT_ID, false, false).hub_lock(),
            HubLock::Held(None)
        );
        // An unreadable boot id never reads as a reboot.
        let blind = probe(&format!("held\n{}", owner_record("x")), "", false, true);
        assert_eq!(blind.boot_id, None);
        assert!(matches!(blind.hub_lock(), HubLock::Held(Some(_))));
    }

    #[test]
    fn probes_expire_after_the_ttl() {
        use std::sync::Arc;
        let start = Instant::now();
        let offset = Arc::new(Mutex::new(Duration::ZERO));
        let clock = {
            let offset = offset.clone();
            move || start + *offset.lock().unwrap()
        };
        let cache = ProbeCache::with_clock(Duration::from_secs(30), clock);
        assert_eq!(cache.get(), None);
        let probe = probe("free", BOOT_ID, false, true);
        cache.store(probe.clone());
        *offset.lock().unwrap() = Duration::from_secs(29);
        assert_eq!(cache.get(), Some(probe.clone()), "still fresh");
        *offset.lock().unwrap() = Duration::from_secs(30);
        assert_eq!(cache.get(), None, "expired");
        cache.store(probe.clone());
        assert_eq!(cache.get(), Some(probe));
        cache.forget();
        assert_eq!(cache.get(), None);
    }

    fn paths(state: &str) -> TabletPaths {
        TabletPaths {
            xochitl_dir: "/x".into(),
            state_dir: state.into(),
            xochitl_conf: "/c".into(),
            boot_id: "/proc/sys/kernel/random/boot_id".into(),
        }
    }

    #[test]
    fn write_guards_assert_what_the_probe_saw() {
        let paths = paths("/state dir");
        let free = probe("free", BOOT_ID, false, true)
            .write_guard(&paths)
            .unwrap();
        assert!(free.contains("[ ! -e '/state dir/lock' ] || {"), "{free}");
        assert!(
            free.contains("if [ -d '/state dir' ]; then\nexec 9>>'/state dir/guard.flock'"),
            "{free}"
        );
        assert!(
            free.contains(&format!("exit {EXIT_HUB_LOCK_CHANGED}; }}")),
            "{free}"
        );
        let stale = probe(
            &format!("held\n{}", owner_record("an-earlier-boot")),
            BOOT_ID,
            false,
            true,
        );
        let guard = stale.write_guard(&paths).unwrap();
        assert!(
            guard.contains(&format!(
                "{{ [ ! -e '/state dir/lock' ] || [ \"$(cat -- '/state dir/lock/owner' 2>/dev/null)\" = {} ]; }}",
                shell_quote(owner_record("an-earlier-boot").trim_end_matches('\n'))
            )),
            "{guard}"
        );
        let held = probe(
            &format!("held\n{}", owner_record(BOOT_ID)),
            BOOT_ID,
            false,
            true,
        );
        assert_eq!(held.write_guard(&paths), None);
    }

    /// The guard, run by a real shell (and BusyBox `ash`), passes or
    /// fails as the lock changed; a write after it runs only if it passed.
    #[cfg(unix)]
    #[test]
    fn write_guards_run() {
        let busybox = std::env::var_os("RMU_TEST_BUSYBOX");
        std::iter::once(None)
            .chain(busybox.as_deref().map(Some))
            .for_each(write_guards_run_in);
    }

    #[cfg(unix)]
    fn write_guards_run_in(busybox: Option<&std::ffi::OsStr>) {
        let scratch = super::super::harness::Scratch::new("guard");
        let state = scratch.0.join("state");
        let paths = paths(&state.display().to_string());
        let target = scratch.0.join("written");
        let run = |guard: &str| {
            let script = format!(
                "{guard}: > {}\n",
                shell_quote(&target.display().to_string())
            );
            let mut shell = match busybox {
                Some(busybox) => {
                    let mut shell = std::process::Command::new(busybox);
                    shell.arg("sh");
                    shell
                }
                None => std::process::Command::new("sh"),
            };
            let status = shell.args(["-c", &script]).status().unwrap().code();
            let written = target.exists();
            let _ = std::fs::remove_file(&target);
            (status, written)
        };
        let free = probe("free", BOOT_ID, false, false)
            .write_guard(&paths)
            .unwrap();
        // No state dir (no hub ever ran): no flock, the check passes.
        assert_eq!(run(&free), (Some(0), true));
        std::fs::create_dir_all(&state).unwrap();
        assert_eq!(run(&free), (Some(0), true), "with the flock");
        // A hub run took the lock since the probe.
        std::fs::create_dir_all(state.join("lock")).unwrap();
        std::fs::write(state.join("lock/owner"), owner_record(BOOT_ID)).unwrap();
        assert_eq!(run(&free), (Some(EXIT_HUB_LOCK_CHANGED), false));
        // A stale lock: unchanged passes, replaced fails, gone passes.
        let stale_record = owner_record("an-earlier-boot");
        std::fs::write(state.join("lock/owner"), &stale_record).unwrap();
        let stale = probe(&format!("held\n{stale_record}"), BOOT_ID, false, false)
            .write_guard(&paths)
            .unwrap();
        assert_eq!(run(&stale), (Some(0), true));
        std::fs::write(state.join("lock/owner"), owner_record(BOOT_ID)).unwrap();
        assert_eq!(run(&stale), (Some(EXIT_HUB_LOCK_CHANGED), false));
        std::fs::remove_dir_all(state.join("lock")).unwrap();
        assert_eq!(run(&stale), (Some(0), true));
    }

    #[test]
    fn missing_or_garbled_sections_are_errors() {
        assert!(parse_probe(MARKER, "listing only\n").is_err());
        assert!(parse_probe(MARKER, &output("maybe", BOOT_ID, false, false)).is_err());
        let truncated = output("free", BOOT_ID, false, false);
        let truncated = &truncated[..truncated.find(ATTACHED).unwrap()];
        assert!(parse_probe(MARKER, truncated).is_err());
    }

    #[test]
    fn the_script_reads_the_hub_paths_quoted() {
        let paths = TabletPaths {
            xochitl_dir: "/x".into(),
            state_dir: "/state dir/it's".into(),
            xochitl_conf: "/c".into(),
            boot_id: "/proc/sys/kernel/random/boot_id".into(),
        };
        let script = probe_script(&paths, MARKER);
        [
            "[ -d '/state dir/it'\\''s/lock' ]",
            "cat -- '/state dir/it'\\''s/lock/owner' 2>/dev/null",
            "cat -- /proc/sys/kernel/random/boot_id 2>/dev/null",
            "[ -e '/state dir/it'\\''s/watchdog-fired' ]",
            "[ -e '/state dir/it'\\''s/hub.json' ]",
        ]
        .iter()
        .for_each(|expected| assert!(script.contains(expected), "{expected}\n{script}"));
        assert!(
            !script.contains("exit"),
            "it must not end the script it precedes"
        );
    }

    /// The script, run by a real shell (and BusyBox `ash` when
    /// `RMU_TEST_BUSYBOX` names one), prints what the parser reads.
    #[cfg(unix)]
    #[test]
    fn the_script_and_the_parser_agree() {
        let busybox = std::env::var_os("RMU_TEST_BUSYBOX");
        std::iter::once(None)
            .chain(busybox.as_deref().map(Some))
            .for_each(script_and_parser_agree);
    }

    #[cfg(unix)]
    fn script_and_parser_agree(busybox: Option<&std::ffi::OsStr>) {
        let scratch = super::super::harness::Scratch::new("probe");
        let state = scratch.0.join("state");
        std::fs::create_dir_all(state.join("lock")).unwrap();
        std::fs::write(state.join("lock/owner"), owner_record(BOOT_ID)).unwrap();
        std::fs::write(state.join("hub.json"), "{}").unwrap();
        std::fs::write(scratch.0.join("boot"), format!("{BOOT_ID}\n")).unwrap();
        let paths = TabletPaths {
            xochitl_dir: "/x".into(),
            state_dir: state.display().to_string(),
            xochitl_conf: "/c".into(),
            boot_id: scratch.0.join("boot").display().to_string(),
        };
        let run = |paths: &TabletPaths| {
            let script = format!("echo before\n{}echo after", probe_script(paths, MARKER));
            let mut shell = match busybox {
                Some(busybox) => {
                    let mut shell = std::process::Command::new(busybox);
                    shell.arg("sh");
                    shell
                }
                None => std::process::Command::new("sh"),
            };
            let out = shell.args(["-c", &script]).output().unwrap();
            assert!(out.status.success());
            parse_probe(MARKER, &String::from_utf8(out.stdout).unwrap()).unwrap()
        };
        let probe = run(&paths);
        assert!(probe.attached);
        assert!(matches!(probe.hub_lock(), HubLock::Held(Some(_))));
        std::fs::remove_dir_all(state.join("lock")).unwrap();
        std::fs::remove_file(state.join("hub.json")).unwrap();
        let probe = run(&paths);
        assert_eq!((probe.hub_lock(), probe.attached), (HubLock::Free, false));
        let gone = TabletPaths {
            state_dir: scratch.0.join("missing").display().to_string(),
            boot_id: scratch.0.join("missing/boot").display().to_string(),
            ..paths
        };
        let probe = run(&gone);
        assert_eq!(probe.boot_id, None);
        assert_eq!((probe.hub_lock(), probe.attached), (HubLock::Free, false));
    }
}
