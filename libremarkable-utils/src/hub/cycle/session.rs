//! The tablet side of one run: the lock, the watchdog, stopping and
//! starting xochitl, and the error path that brings it back.

use std::time::{Duration, Instant};

use super::origin::OriginCtx;
use super::{Hub, TakeOver, Warning};
use crate::error::{Error, Result};
use crate::hub::device::{DeviceError, HeldLock, LockRequest, Tablet};
use crate::hub::model::TabletState;

/// The tablet side of one run: the lock, whether xochitl may be stopped,
/// and the error path that brings it back.
pub(super) struct Session<'t> {
    pub(super) tablet: &'t dyn Tablet,
    watchdog: Duration,
    lock: Option<HeldLock>,
    /// Set before a stop is attempted (its reply may be lost), cleared by
    /// a start that succeeded.
    pub(super) maybe_stopped: bool,
    /// When the last stop was attempted, while xochitl has not started
    /// since.
    stopped_at: Option<Instant>,
    /// Keeping xochitl stopped past step 6 (single-stop): the watchdog
    /// timeout meanwhile, cleared by a start.
    deferred: Option<Duration>,
    /// Device time of the session's first start. Every run reads the
    /// tablet once, stopped, before any start, so this is the read's
    /// racy-entry reference; `None` while xochitl has not started since.
    started_at: Option<i64>,
}

impl<'t> Session<'t> {
    fn new(tablet: &'t dyn Tablet, watchdog: Duration) -> Self {
        Self {
            tablet,
            watchdog,
            lock: None,
            maybe_stopped: false,
            stopped_at: None,
            deferred: None,
            started_at: None,
        }
    }

    /// Take the lock, retrying once with the same request after a
    /// failure that may have been a lost acknowledgement (the retry then
    /// adopts the lock). Not after the tablet stopped answering: the
    /// retry would wait for another timeout. A lock taken then stays
    /// until `--break-lock` (or a reboot), as after a crash between the
    /// lock and the first watchdog.
    pub(super) fn acquire(
        &mut self,
        request: &LockRequest,
        break_lock: bool,
    ) -> Result<Option<TakeOver>> {
        let lock = match self.tablet.acquire_lock(request, break_lock) {
            Err(error) if is_transport(&error) => self.tablet.acquire_lock(request, break_lock)?,
            other => other?,
        };
        let took_over = lock.took_over().map(|(owner, reason)| TakeOver {
            previous: owner.cloned(),
            reason,
        });
        self.lock = Some(lock);
        Ok(took_over)
    }

    pub(super) fn lock(&self) -> &HeldLock {
        self.lock.as_ref().expect("the lock is taken first")
    }

    /// Arm (or re-arm) the watchdog: for the run's timeout, or the
    /// shorter one while xochitl stays stopped past step 6.
    pub(super) fn arm(&self) -> Result<()> {
        let timeout = self
            .deferred
            .map_or(self.watchdog, |cap| cap.min(self.watchdog));
        self.tablet.arm_watchdog(self.lock(), timeout)
    }

    pub(super) fn stop(&mut self) -> Result<()> {
        self.arm()?;
        self.maybe_stopped = true;
        self.stopped_at = Some(Instant::now());
        self.tablet.stop_xochitl(self.lock())
    }

    /// Start xochitl; the device time just before the start (the first
    /// one is kept, [`Self::started_at`]).
    pub(super) fn start(&mut self) -> Result<i64> {
        let time = self.tablet.start_xochitl(self.lock())?;
        self.maybe_stopped = false;
        self.deferred = None;
        self.stopped_at = None;
        self.started_at.get_or_insert(time);
        Ok(time)
    }

    /// Device time of the first start after the read (see the field).
    pub(super) fn started_at(&self) -> Option<i64> {
        self.started_at
    }

    /// Keep xochitl stopped past step 6, re-arming the watchdog for `cap`
    /// now and at every later arming until the next start.
    pub(super) fn defer_start(&mut self, cap: Duration) -> Result<()> {
        debug_assert!(self.maybe_stopped, "only a stopped xochitl stays stopped");
        self.deferred = Some(cap);
        self.arm()
    }

    /// Whether xochitl is being kept stopped past step 6.
    pub(super) fn deferring(&self) -> bool {
        self.deferred.is_some() && self.maybe_stopped
    }

    /// How long xochitl has been stopped, if it has not started since
    /// this run stopped it: a budget never counts the time it ran.
    pub(super) fn stopped_for(&self) -> Option<Duration> {
        self.stopped_at.map(|at| at.elapsed())
    }

    pub(super) fn write_state(&self, state: &TabletState) -> Result<()> {
        self.tablet.write_state(self.lock(), state)
    }

    /// Step 16 after success: disarm and release. Failures here leave
    /// nothing unsafe (a pending watchdog only restarts a running
    /// xochitl; a left lock goes stale when it fires), so they are
    /// warnings.
    fn finish(&mut self) -> Vec<Warning> {
        let Some(lock) = self.lock.take() else {
            return Vec::new();
        };
        let disarm = self.tablet.disarm_watchdog(&lock).err();
        // A tablet that stopped answering would not answer the release.
        let release = match &disarm {
            Some(error @ Error::Unreachable(_)) => Some(format!("not tried: {error}")),
            _ => self.tablet.release_lock(&lock).err().map(|e| e.to_string()),
        };
        [
            disarm.map(|e| Warning::DisarmFailed {
                error: e.to_string(),
            }),
            release.map(|error| Warning::ReleaseFailed { error }),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The error path: bring xochitl back if this run may have stopped
    /// it, disarm only once it runs, release the lock. Everything is best
    /// effort; with the connection gone the watchdog brings xochitl back.
    /// A tablet that stopped answering ends it: every further try would
    /// only wait for another timeout.
    fn abort(&mut self) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        let gone = |result: &Result<()>| matches!(result, Err(Error::Unreachable(_)));
        let start = self
            .maybe_stopped
            .then(|| self.tablet.start_xochitl(&lock).map(drop));
        if start.as_ref().is_some_and(gone) {
            return;
        }
        let disarm = start
            .is_none_or(|start| start.is_ok())
            .then(|| self.tablet.disarm_watchdog(&lock));
        if disarm.as_ref().is_some_and(gone) {
            return;
        }
        let _ = self.tablet.release_lock(&lock);
    }
}

/// A failure the same request may get past (see [`Session::acquire`]).
fn is_transport(error: &Error) -> bool {
    !matches!(
        error,
        Error::Unreachable(_)
            | Error::HubDevice(
                DeviceError::Locked(_)
                    | DeviceError::LockContended
                    | DeviceError::Busy
                    | DeviceError::InvalidRequest(_)
            )
    )
}

impl<'a> Hub<'a> {
    /// Run `body` on a tablet session: on error, the error path; on
    /// success, disarm and release, then (if `body` hands its origin back,
    /// whose cache `flock` is still held) `git maintenance run --auto` on
    /// the cache, with the tablet free again. Warnings returned.
    pub(super) fn session<T>(
        &self,
        tablet: &dyn Tablet,
        body: impl FnOnce(&mut Session) -> Result<(T, Option<OriginCtx>)>,
    ) -> Result<(T, Vec<Warning>)> {
        let mut session = Session::new(tablet, self.options.watchdog);
        let result = match body(&mut session) {
            Ok((value, origin)) => {
                let mut warnings = session.finish();
                if let Some(origin) = origin {
                    warnings.extend(self.maintain(&origin));
                }
                Ok((value, warnings))
            }
            Err(error) => {
                session.abort();
                Err(error)
            }
        };
        self.progress.finished();
        result
    }

    /// `git maintenance run --auto` on the cache (the user's thresholds
    /// decide whether anything happens); a failure is only a warning.
    fn maintain(&self, origin: &OriginCtx) -> Option<Warning> {
        self.progress.step("Maintaining the local cache");
        origin
            .cache
            .repo
            .maintenance_auto()
            .err()
            .map(|error| Warning::MaintenanceFailed {
                error: error.to_string(),
            })
    }
}
