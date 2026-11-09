//! The hub lock: the owner record, the staleness decision and the
//! acquisition protocol shared by every [`super::Tablet`]. Pure.
//!
//! The lock is a directory created with `mkdir` (atomic) holding an
//! `owner` record of `key=value` lines:
//!
//! ```text
//! run=<random run id>
//! host=<computer running rmu>
//! pid=<its process id>
//! started=<unix seconds, that computer's clock>
//! boot_id=<the tablet's boot id when the lock was taken>
//! ```
//!
//! Unknown keys are ignored. A held lock is stale only if the tablet
//! rebooted since (boot id changed) or the watchdog fired; there is no
//! time-based staleness, since tablet clocks drift. The run id belongs
//! to the [`LockRequest`], so a retry after a lost acknowledgement finds
//! its own lock and adopts it instead of locking itself out.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::DeviceError;
use crate::error::Result;

/// Longest host name kept in the owner record, in characters.
const MAX_HOST_CHARS: usize = 255;

/// Who is asking for the lock. Create one per process (or run) and reuse
/// it for retries: its run id identifies the lock as ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRequest {
    pub host: String,
    pub pid: u32,
    pub started_unix: u64,
    run: String,
}

impl LockRequest {
    /// A request with a fresh run id.
    pub fn new(host: impl Into<String>, pid: u32, started_unix: u64) -> Self {
        Self {
            host: host.into(),
            pid,
            started_unix,
            run: new_run_id(),
        }
    }

    /// A request for the current process, started now.
    pub fn for_this_process(host: impl Into<String>) -> Self {
        Self::new(
            host,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
    }

    /// The run id written to the owner record.
    pub fn run(&self) -> &str {
        &self.run
    }

    #[cfg(test)]
    pub(super) fn with_run(mut self, run: &str) -> Self {
        self.run = run.to_string();
        self
    }
}

/// A parsed owner record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockOwner {
    pub run: String,
    pub host: String,
    pub pid: u32,
    pub started_unix: u64,
    pub boot_id: String,
}

impl fmt::Display for LockOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pid {} on {}, started at unix time {}",
            self.pid, self.host, self.started_unix
        )
    }
}

/// The lock as seen without taking it (`status`, and the other mutating
/// commands, which refuse while it is held).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "owner", rename_all = "snake_case")]
pub enum LockState {
    Free,
    /// Held; `None` if the owner record is missing or unreadable.
    Held(Option<LockOwner>),
}

/// Why a held lock was taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// The tablet rebooted since the lock was taken.
    BootChanged,
    /// The watchdog fired: that run ends at its next mutation.
    WatchdogFired,
    /// `--break-lock`.
    Broken,
}

/// What to do about a lock that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockDecision {
    /// It is our own (an earlier attempt whose acknowledgement was lost).
    Adopt,
    TakeOver(StaleReason),
    Refuse,
}

/// The lock, held by this run. Only [`super::Tablet::acquire_lock`]
/// creates one; mutating operations require it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLock {
    owner: LockOwner,
    took_over: Option<(Option<LockOwner>, StaleReason)>,
}

impl HeldLock {
    pub(super) fn new(
        owner: LockOwner,
        took_over: Option<(Option<LockOwner>, StaleReason)>,
    ) -> Self {
        Self { owner, took_over }
    }

    /// This run's id, as written to the owner record.
    pub fn run(&self) -> &str {
        &self.owner.run
    }

    pub fn owner(&self) -> &LockOwner {
        &self.owner
    }

    /// The previous owner (`None` if its record was unreadable) and why
    /// its lock was taken over, if it was.
    pub fn took_over(&self) -> Option<(Option<&LockOwner>, StaleReason)> {
        self.took_over
            .as_ref()
            .map(|(owner, reason)| (owner.as_ref(), *reason))
    }
}

/// Decide about an existing lock. `existing` is `None` when its owner
/// record is missing or unreadable: then only a fired watchdog or
/// `break_lock` frees it. Our own lock is adopted only if nothing
/// happened to it: after a reboot or a fired watchdog it is taken over
/// like anyone's, so the caller learns that its run ended.
pub fn decide(
    existing: Option<&LockOwner>,
    own_run: &str,
    current_boot: &str,
    watchdog_fired: bool,
    break_lock: bool,
) -> LockDecision {
    match existing {
        Some(owner) if owner.boot_id != current_boot => {
            LockDecision::TakeOver(StaleReason::BootChanged)
        }
        _ if watchdog_fired => LockDecision::TakeOver(StaleReason::WatchdogFired),
        Some(owner) if owner.run == own_run => LockDecision::Adopt,
        _ if break_lock => LockDecision::TakeOver(StaleReason::Broken),
        _ => LockDecision::Refuse,
    }
}

/// What an acquisition attempt found on the tablet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AcquireProbe {
    /// `mkdir` succeeded; the owner record is written.
    Acquired { boot_id: String },
    /// The lock exists: the current boot id, whether the watchdog marker
    /// is present, the owner record as read (possibly empty or garbled)
    /// and its version (a device-side checksum of its bytes, which the
    /// takeover compares, so even a corrupt record can be replaced).
    Held {
        boot_id: String,
        watchdog_fired: bool,
        owner_text: String,
        version: String,
    },
}

/// Attempts before giving up on a lock that keeps changing under us.
const ACQUIRE_ATTEMPTS: usize = 3;

/// The acquisition protocol shared by every [`super::Tablet`]: `probe`
/// tries `mkdir` (writing the record on success); an existing lock is
/// decided by [`decide`] and replaced with `take_over`, a
/// compare-and-swap on the record's version that writes this request's
/// record and returns the boot id, or `None` if the record changed
/// meanwhile (then the whole attempt is retried).
pub(super) fn acquire(
    request: &LockRequest,
    break_lock: bool,
    probe: &dyn Fn() -> Result<AcquireProbe>,
    take_over: &dyn Fn(&str) -> Result<Option<String>>,
) -> Result<HeldLock> {
    let owner = |boot_id: String| LockOwner {
        run: request.run.clone(),
        host: sanitize_host(&request.host),
        pid: request.pid,
        started_unix: request.started_unix,
        boot_id,
    };
    let attempt = |_| -> Result<Option<HeldLock>> {
        match probe()? {
            AcquireProbe::Acquired { boot_id } => Ok(Some(HeldLock::new(owner(boot_id), None))),
            AcquireProbe::Held {
                boot_id,
                watchdog_fired,
                owner_text,
                version,
            } => {
                let existing = parse_owner(&owner_text);
                let took_over = match decide(
                    existing.as_ref(),
                    &request.run,
                    &boot_id,
                    watchdog_fired,
                    break_lock,
                ) {
                    LockDecision::Refuse => {
                        return Err(DeviceError::Locked(existing.map_or_else(
                            || "owner record unreadable".to_string(),
                            |owner| owner.to_string(),
                        ))
                        .into());
                    }
                    LockDecision::Adopt => None,
                    LockDecision::TakeOver(reason) => Some((existing, reason)),
                };
                Ok(take_over(&version)?.map(|boot_id| HeldLock::new(owner(boot_id), took_over)))
            }
        }
    };
    if !valid_run(&request.run) {
        return Err(
            DeviceError::InvalidRequest(format!("invalid run id {:?}", request.run)).into(),
        );
    }
    (0..ACQUIRE_ATTEMPTS)
        .find_map(|n| attempt(n).transpose())
        .unwrap_or_else(|| Err(DeviceError::LockContended.into()))
}

/// A fresh run id: 32 lowercase hex digits.
pub(super) fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The guard's line identifying a run in the owner record.
pub(super) fn run_line(run: &str) -> String {
    format!("run={run}")
}

/// The owner record without its `boot_id` line, which the device appends
/// (it is read there, in the same invocation).
pub(super) fn owner_fields(request: &LockRequest) -> String {
    format!(
        "{}\nhost={}\npid={}\nstarted={}\n",
        run_line(&request.run),
        sanitize_host(&request.host),
        request.pid,
        request.started_unix
    )
}

/// The complete owner record.
pub(super) fn owner_record(request: &LockRequest, boot_id: &str) -> String {
    format!("{}boot_id={boot_id}\n", owner_fields(request))
}

/// A host name that fits on one line of the record.
fn sanitize_host(host: &str) -> String {
    let clean: String = host
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(MAX_HOST_CHARS)
        .collect();
    if clean.is_empty() {
        "unknown".to_string()
    } else {
        clean
    }
}

/// A boot id as read from the kernel: one non-empty token.
pub(super) fn valid_boot_id(boot_id: &str) -> bool {
    !boot_id.is_empty()
        && boot_id.len() <= 64
        && boot_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_run(run: &str) -> bool {
    run.len() == 32
        && run
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse an owner record; `None` if a required key is missing or invalid.
pub fn parse_owner(text: &str) -> Option<LockOwner> {
    let field = |key: &str| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    };
    let owner = LockOwner {
        run: field("run")?.to_string(),
        host: field("host")?.to_string(),
        pid: field("pid")?.parse().ok()?,
        started_unix: field("started")?.parse().ok()?,
        boot_id: field("boot_id")?.to_string(),
    };
    (valid_run(&owner.run) && valid_boot_id(&owner.boot_id)).then_some(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const RUN: &str = "0123456789abcdef0123456789abcdef";
    const BOOT: &str = "6f1d3c2a-0000-4000-8000-00000000b007";
    const OTHER_BOOT: &str = "22222222-0000-4000-8000-000000000000";

    fn request() -> LockRequest {
        LockRequest::new("laptop", 4242, 1_790_000_000).with_run(RUN)
    }

    fn owner(boot: &str) -> LockOwner {
        LockOwner {
            run: RUN.into(),
            host: "laptop".into(),
            pid: 4242,
            started_unix: 1_790_000_000,
            boot_id: boot.into(),
        }
    }

    #[test]
    fn record_round_trips() {
        let record = owner_record(&request(), BOOT);
        assert_eq!(
            record,
            format!("run={RUN}\nhost=laptop\npid=4242\nstarted=1790000000\nboot_id={BOOT}\n")
        );
        assert_eq!(parse_owner(&record), Some(owner(BOOT)));
    }

    #[test]
    fn hostile_hosts_stay_on_one_line() {
        let hostile =
            LockRequest::new("evil\nrun=ffffffffffffffffffffffffffffffff\r", 1, 2).with_run(RUN);
        let record = owner_record(&hostile, BOOT);
        assert_eq!(record.lines().count(), 5);
        let parsed = parse_owner(&record).unwrap();
        assert_eq!(parsed.run, RUN);
        assert_eq!(parsed.host, "evil?run=ffffffffffffffffffffffffffffffff?");
        assert!(owner_fields(&LockRequest::new("", 1, 2)).contains("host=unknown\n"));
        let long = LockRequest::new("h".repeat(1000), 1, 2);
        assert_eq!(
            parse_owner(&owner_record(&long, BOOT)).unwrap().host.len(),
            MAX_HOST_CHARS
        );
    }

    #[test]
    fn unknown_keys_are_ignored_and_missing_keys_refused() {
        let record = format!("future=1\n{}", owner_record(&request(), BOOT));
        assert_eq!(parse_owner(&record), Some(owner(BOOT)));
        [
            String::new(),
            format!("run={RUN}\nhost=h\npid=1\nstarted=2\n"),
            format!("run=XYZ\nhost=h\npid=1\nstarted=2\nboot_id={BOOT}\n"),
            format!("run={RUN}\nhost=h\npid=x\nstarted=2\nboot_id={BOOT}\n"),
            format!("run={RUN}\nhost=h\npid=1\nstarted=2\nboot_id=a b\n"),
        ]
        .iter()
        .for_each(|record| assert_eq!(parse_owner(record), None, "{record:?}"));
    }

    #[test]
    fn staleness_is_boot_change_or_fired_watchdog_only() {
        let held = owner(BOOT);
        let other_run = "ffffffffffffffffffffffffffffffff";
        let d = |existing, boot, fired, broken| decide(existing, other_run, boot, fired, broken);
        assert_eq!(d(Some(&held), BOOT, false, false), LockDecision::Refuse);
        assert_eq!(
            d(Some(&held), OTHER_BOOT, false, false),
            LockDecision::TakeOver(StaleReason::BootChanged)
        );
        assert_eq!(
            d(Some(&held), BOOT, true, false),
            LockDecision::TakeOver(StaleReason::WatchdogFired)
        );
        assert_eq!(
            d(Some(&held), BOOT, false, true),
            LockDecision::TakeOver(StaleReason::Broken)
        );
        // A boot change is reported even when the lock is also broken.
        assert_eq!(
            d(Some(&held), OTHER_BOOT, true, true),
            LockDecision::TakeOver(StaleReason::BootChanged)
        );
        // An unreadable record: no boot id to compare.
        assert_eq!(d(None, BOOT, false, false), LockDecision::Refuse);
        assert_eq!(
            d(None, BOOT, true, false),
            LockDecision::TakeOver(StaleReason::WatchdogFired)
        );
        assert_eq!(
            d(None, BOOT, false, true),
            LockDecision::TakeOver(StaleReason::Broken)
        );
        // Our own lock is adopted only if nothing happened to it.
        assert_eq!(
            decide(Some(&held), RUN, BOOT, false, false),
            LockDecision::Adopt
        );
        assert_eq!(
            decide(Some(&held), RUN, BOOT, false, true),
            LockDecision::Adopt
        );
        assert_eq!(
            decide(Some(&held), RUN, OTHER_BOOT, false, false),
            LockDecision::TakeOver(StaleReason::BootChanged)
        );
        assert_eq!(
            decide(Some(&held), RUN, BOOT, true, false),
            LockDecision::TakeOver(StaleReason::WatchdogFired)
        );
    }

    fn held(
        owner_text: String,
        boot: &'static str,
        fired: bool,
    ) -> impl Fn() -> Result<AcquireProbe> {
        move || {
            Ok(AcquireProbe::Held {
                boot_id: boot.into(),
                watchdog_fired: fired,
                owner_text: owner_text.clone(),
                version: "v1".into(),
            })
        }
    }

    #[test]
    fn acquire_takes_a_free_lock() {
        let acquired = acquire(
            &request(),
            false,
            &|| {
                Ok(AcquireProbe::Acquired {
                    boot_id: BOOT.into(),
                })
            },
            &|_| panic!("no takeover for a free lock"),
        )
        .unwrap();
        assert_eq!(acquired.owner(), &owner(BOOT));
        assert_eq!(acquired.run(), RUN);
        assert_eq!(acquired.took_over(), None);
    }

    #[test]
    fn acquire_refuses_a_live_lock() {
        let mine = LockRequest::new("laptop", 1, 2);
        let refused = acquire(
            &mine,
            false,
            &held(owner_record(&request(), BOOT), BOOT, false),
            &|_| panic!("a live lock is never taken over"),
        );
        assert!(matches!(
            refused,
            Err(crate::Error::HubDevice(DeviceError::Locked(text))) if text.contains("pid 4242 on laptop")
        ));
        let unreadable = acquire(
            &mine,
            false,
            &held("\u{fffd}garbage".into(), BOOT, false),
            &|_| panic!("an unreadable lock needs --break-lock"),
        );
        assert!(matches!(
            unreadable,
            Err(crate::Error::HubDevice(DeviceError::Locked(text))) if text.contains("unreadable")
        ));
    }

    #[test]
    fn acquire_takes_over_stale_locks_by_version() {
        let mine = LockRequest::new("laptop", 1, 2);
        let taken = acquire(
            &mine,
            false,
            &held(owner_record(&request(), BOOT), OTHER_BOOT, false),
            &|version| {
                assert_eq!(version, "v1");
                Ok(Some(OTHER_BOOT.to_string()))
            },
        )
        .unwrap();
        assert_eq!(taken.run(), mine.run());
        assert_eq!(taken.owner().boot_id, OTHER_BOOT);
        assert_eq!(
            taken.took_over(),
            Some((Some(&owner(BOOT)), StaleReason::BootChanged))
        );
        // A garbled record is replaced with --break-lock.
        let broken = acquire(
            &mine,
            true,
            &held("\0\u{fffd}".into(), BOOT, false),
            &|_| Ok(Some(BOOT.to_string())),
        )
        .unwrap();
        assert_eq!(broken.took_over(), Some((None, StaleReason::Broken)));
    }

    #[test]
    fn acquire_adopts_its_own_lock_unless_something_happened() {
        let adopted = acquire(
            &request(),
            false,
            &held(owner_record(&request(), BOOT), BOOT, false),
            &|_| Ok(Some(BOOT.to_string())),
        )
        .unwrap();
        assert_eq!(adopted.took_over(), None);
        let fired = acquire(
            &request(),
            false,
            &held(owner_record(&request(), BOOT), BOOT, true),
            &|_| Ok(Some(BOOT.to_string())),
        )
        .unwrap();
        assert_eq!(
            fired.took_over(),
            Some((Some(&owner(BOOT)), StaleReason::WatchdogFired))
        );
    }

    #[test]
    fn acquire_gives_up_on_a_contended_lock() {
        let attempts = Cell::new(0);
        let contended = acquire(
            &LockRequest::new("laptop", 1, 2),
            true,
            &held(owner_record(&request(), BOOT), BOOT, false),
            &|_| {
                attempts.set(attempts.get() + 1);
                Ok(None)
            },
        );
        assert!(matches!(
            contended,
            Err(crate::Error::HubDevice(DeviceError::LockContended))
        ));
        assert_eq!(attempts.get(), ACQUIRE_ATTEMPTS);
    }

    #[test]
    fn run_ids_are_per_request() {
        let request = LockRequest::for_this_process("h");
        assert!(valid_run(request.run()));
        assert_ne!(request.run(), LockRequest::for_this_process("h").run());
        assert_eq!(run_line(RUN), format!("run={RUN}"));
        let invalid = LockRequest::new("h", 1, 2).with_run("nope");
        assert!(acquire(&invalid, false, &|| panic!("never probed"), &|_| panic!()).is_err());
    }
}
