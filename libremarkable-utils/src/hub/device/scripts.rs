//! Device-side shell scripts and the parsers for their output. Pure:
//! generation and parsing are unit-tested here, and [`super::SshTablet`]
//! only sends scripts and hands output back.
//!
//! The scripts target BusyBox `ash` and applets (the survey's list:
//! `stat -c` with `%y`/`%z`, `find -exec +`, `tar -T -`, `mv -T`,
//! `sha256sum`, `mktemp`, `flock`, `setsid`, `sync`); the contract tests
//! run them under BusyBox and GNU tools. Every interpolated value goes
//! through [`shell_quote`]. Scripts sent on stdin (`sh -s`) that change
//! anything wrap their body in a function: a truncated transfer is then
//! a syntax error and runs nothing, and no lock is taken before the
//! whole script has arrived.
//!
//! **Serialisation.** Lock operations and guarded scripts hold an
//! exclusive `flock` on [`TabletPaths::guard_flock`] from their guard
//! check to their last change, so a takeover can never fall between an
//! old run's guard and its mutation. The kernel drops the `flock` when
//! the script exits, however it ends. Only bounded, local work runs under
//! it, and waiters give up after [`FLOCK_WAIT_SECS`]; staging (a network
//! transfer into the run's own directory) checks the guard without it,
//! and the watchdog never takes it, so a stuck script cannot keep
//! xochitl down.

use super::apply::{ApplyStep, StagedPath};
use super::lock::{AcquireProbe, LockRequest, LockState, owner_fields, parse_owner, run_line};
use super::{DeviceError, StatEntry, StatListing, TabletPaths, WATCHDOG_UNIT};
use crate::hub::model::{DocId, StatSignature};
use crate::ssh::shell_quote;

/// Exit status of a guarded script whose watchdog fired.
pub(super) const EXIT_WATCHDOG_FIRED: i32 = 75;
/// Exit status of a guarded script whose lock is no longer ours.
pub(super) const EXIT_LOCK_LOST: i32 = 76;
/// Exit status of a lock takeover that found the lock changed.
pub(super) const EXIT_LOCK_CHANGED: i32 = 77;
/// Exit status of a script that could not get the guard `flock` in time.
pub(super) const EXIT_BUSY: i32 = 78;
/// Exit status of a disarm refused because xochitl is stopped.
pub(super) const EXIT_XOCHITL_STOPPED: i32 = 79;
/// Exit status of a guarded script on a tablet that rebooted since the
/// lock was taken.
pub(super) const EXIT_REBOOTED: i32 = 80;
/// Exit status of an apply refused because xochitl is running.
pub(super) const EXIT_XOCHITL_RUNNING: i32 = 81;
/// Exit status of a stop refused for want of this run's watchdog.
pub(super) const EXIT_NOT_ARMED: i32 = 82;

/// Seconds a script waits for the guard `flock`: long enough for any
/// legitimate holder (an apply, `systemctl stop xochitl`), bounded so a
/// holder stuck on a dead connection cannot block every later run.
const FLOCK_WAIT_SECS: u32 = 180;

/// `stat -c` format of the stat pass: raw mode (hex), size, mtime in
/// seconds and human-readable with nanoseconds, the same for ctime,
/// inode, name. The name comes last, so it may contain spaces.
pub(super) const STAT_FORMAT: &str = "%f %s %Y %y %Z %z %i %n";

/// Files per `stat`/`sha256sum` invocation, keeping argv far below the
/// kernel's limit however many files are listed.
const ARGS_PER_CALL: usize = 256;

const NANOS_PER_SEC: i64 = 1_000_000_000;

fn output_error(operation: &'static str, detail: impl Into<String>) -> DeviceError {
    DeviceError::Output {
        operation,
        detail: detail.into(),
    }
}

fn quoted_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    items
        .into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn q(text: &str) -> String {
    shell_quote(text)
}

// ---- guard and lock --------------------------------------------------------

/// Take the guard `flock` on fd 9 (held until the script exits), waiting
/// at most [`FLOCK_WAIT_SECS`] ([`EXIT_BUSY`]); BusyBox `flock` has no
/// `-w`. The file's directory must exist: in `ash`, a failed `exec`
/// redirection exits the shell at once.
fn take_flock(paths: &TabletPaths) -> String {
    format!(
        "exec 9>>{flock}\n\
         n=0\n\
         until flock -n 9; do\n\
         n=$((n + 1)); [ \"$n\" -le {wait} ] || {{ echo 'rmu: the tablet is busy with another hub operation' >&2; exit {busy}; }}\n\
         sleep 1\n\
         done\n",
        flock = q(&paths.guard_flock()),
        wait = FLOCK_WAIT_SECS,
        busy = EXIT_BUSY,
    )
}

/// Defines the shell functions guarded scripts use:
///
/// - `rmu_lock` takes the `flock` (see [`take_flock`]). A missing state
///   dir means the lock is gone: [`EXIT_LOCK_LOST`].
/// - `rmu_owned` exits [`EXIT_LOCK_LOST`] unless the lock names `run`.
/// - `rmu_guard` exits [`EXIT_WATCHDOG_FIRED`] if the watchdog marker
///   exists, checks `rmu_owned`, then exits [`EXIT_REBOOTED`] unless the
///   record carries the current boot id (a reboot took the run's
///   watchdog and may have started xochitl again).
/// - `rmu_pid_ok` accepts a recorded sleeper process group only if it is
///   a number greater than 1 (a pid file saying `1` must never signal
///   every process).
pub(super) fn guard_fns(paths: &TabletPaths, run: &str) -> String {
    format!(
        "rmu_lock() {{\n\
         [ -d {state} ] || {{ echo 'rmu: the hub state directory is gone' >&2; exit {lost}; }}\n\
         {take_flock}}}\n\
         rmu_owned() {{\n\
         grep -qxF {line} {owner} 2>/dev/null || {{ echo 'rmu: the hub lock is not held by this run' >&2; exit {lost}; }}\n\
         }}\n\
         rmu_guard() {{\n\
         if [ -e {marker} ]; then echo 'rmu: the hub watchdog fired' >&2; exit {fired}; fi\n\
         rmu_owned\n\
         read -r rmu_boot < {boot_id} || exit 1\n\
         grep -qxF \"boot_id=$rmu_boot\" {owner} 2>/dev/null || {{ echo 'rmu: the tablet rebooted since this run took its lock' >&2; exit {rebooted}; }}\n\
         }}\n\
         rmu_pid_ok() {{\n\
         case \"$1\" in ''|*[!0-9]*) return 1 ;; esac\n\
         [ \"$1\" -gt 1 ]\n\
         }}\n",
        state = q(&paths.state_dir),
        take_flock = take_flock(paths),
        line = q(&run_line(run)),
        owner = q(&paths.lock_owner()),
        marker = q(&paths.watchdog_marker()),
        boot_id = q(&paths.boot_id),
        fired = EXIT_WATCHDOG_FIRED,
        lost = EXIT_LOCK_LOST,
        rebooted = EXIT_REBOOTED,
    )
}

/// A script for `sh -s`: the guard functions, then `check` and `body`
/// under the `flock`, inside a function, so nothing (the `flock` above
/// all) happens before the whole script has arrived.
fn main_with(paths: &TabletPaths, run: &str, check: &str, body: &str) -> String {
    format!(
        "{}rmu_main() {{\nrmu_lock\n{check}\n{body}\n}}\nrmu_main </dev/null\n",
        guard_fns(paths, run)
    )
}

/// The guard under the `flock`, then `body`.
pub(super) fn guarded(paths: &TabletPaths, run: &str, body: &str) -> String {
    main_with(paths, run, "rmu_guard", body)
}

/// Variables and the `flock` shared by the lock scripts.
fn lock_prelude(paths: &TabletPaths) -> String {
    format!(
        "d={state}; l={lock}; o={owner}; m={marker}\n\
         mkdir -p -- \"$d\" || exit 1\n\
         {take_flock}\
         read -r boot < {boot_id} || exit 1\n",
        state = q(&paths.state_dir),
        lock = q(&paths.lock_dir()),
        owner = q(&paths.lock_owner()),
        marker = q(&paths.watchdog_marker()),
        take_flock = take_flock(paths),
        boot_id = q(&paths.boot_id),
    )
}

/// Write the owner record (inside the lock directory, atomically and
/// durably: a power cut must not leave an empty record) and clear a
/// leftover watchdog marker.
fn write_owner(request: &LockRequest) -> String {
    format!(
        "{{ printf '%s' {fields}; printf 'boot_id=%s\\n' \"$boot\"; }} > \"$o.tmp\" && sync && mv -f -- \"$o.tmp\" \"$o\" && sync",
        fields = q(&owner_fields(request)),
    )
}

/// The record's version: a checksum of its bytes (empty if missing).
const OWNER_VERSION: &str = "{ cat -- \"$o\" 2>/dev/null; true; } | sha256sum";

/// Try to take the lock with `mkdir`. Prints `acquired` and the boot id,
/// or `held`, the boot id, `fired`/`clear`, the record's version and the
/// record itself.
pub(super) fn acquire_script(paths: &TabletPaths, request: &LockRequest) -> String {
    format!(
        "rmu_acquire() {{\n\
         {prelude}if mkdir -- \"$l\" 2>/dev/null; then\n\
         {write} || {{ rm -rf -- \"$l\"; exit 1; }}\n\
         rm -f -- \"$m\"\n\
         printf 'acquired\\n%s\\n' \"$boot\"\n\
         else\n\
         printf 'held\\n%s\\n' \"$boot\"\n\
         if [ -e \"$m\" ]; then echo fired; else echo clear; fi\n\
         {version}\n\
         cat -- \"$o\" 2>/dev/null\n\
         true\n\
         fi\n\
         }}\n\
         rmu_acquire </dev/null\n",
        prelude = lock_prelude(paths),
        write = write_owner(request),
        version = OWNER_VERSION,
    )
}

fn split_line(text: &str) -> (&str, &str) {
    text.split_once('\n').unwrap_or((text, ""))
}

fn boot_line(line: &str) -> Result<String, DeviceError> {
    let boot = line.trim();
    if super::lock::valid_boot_id(boot) {
        Ok(boot.to_string())
    } else {
        Err(output_error("lock", format!("invalid boot id {boot:?}")))
    }
}

pub(super) fn parse_acquire(output: &str) -> Result<AcquireProbe, DeviceError> {
    let (state, rest) = split_line(output);
    let (boot, rest) = split_line(rest);
    match state {
        "acquired" => Ok(AcquireProbe::Acquired {
            boot_id: boot_line(boot)?,
        }),
        "held" => {
            let (fired, rest) = split_line(rest);
            let (version, owner) = split_line(rest);
            let watchdog_fired = match fired {
                "fired" => true,
                "clear" => false,
                other => return Err(output_error("lock", format!("unexpected {other:?}"))),
            };
            let version = version.split(' ').next().unwrap_or_default();
            if version.len() != 64 {
                return Err(output_error(
                    "lock",
                    format!("invalid record version {version:?}"),
                ));
            }
            Ok(AcquireProbe::Held {
                boot_id: boot_line(boot)?,
                watchdog_fired,
                owner_text: owner.to_string(),
                version: version.to_string(),
            })
        }
        other => Err(output_error("lock", format!("unexpected {other:?}"))),
    }
}

/// Replace the owner record, but only if its version is still `version`
/// (exits [`EXIT_LOCK_CHANGED`] otherwise). Prints the boot id.
pub(super) fn takeover_script(paths: &TabletPaths, request: &LockRequest, version: &str) -> String {
    format!(
        "rmu_takeover() {{\n\
         {prelude}[ -d \"$l\" ] || exit {changed}\n\
         v=$({owner_version})\n\
         [ \"${{v%% *}}\" = {version} ] || exit {changed}\n\
         {write} || exit 1\n\
         rm -f -- \"$m\"\n\
         printf '%s\\n' \"$boot\"\n\
         }}\n\
         rmu_takeover </dev/null\n",
        prelude = lock_prelude(paths),
        changed = EXIT_LOCK_CHANGED,
        owner_version = OWNER_VERSION,
        version = q(version),
        write = write_owner(request),
    )
}

pub(super) fn parse_boot(output: &str) -> Result<String, DeviceError> {
    boot_line(split_line(output).0)
}

/// Remove the lock if it is ours; exits [`EXIT_LOCK_LOST`] otherwise.
pub(super) fn release_script(paths: &TabletPaths, run: &str) -> String {
    main_with(
        paths,
        run,
        "rmu_owned",
        &format!("rm -rf -- {}", q(&paths.lock_dir())),
    )
}

/// Prints `free`, or `held` and the owner record.
pub(super) fn lock_state_script(paths: &TabletPaths) -> String {
    format!(
        "if [ -d {lock} ]; then printf 'held\\n'; cat -- {owner} 2>/dev/null; true; else printf 'free\\n'; fi\n",
        lock = q(&paths.lock_dir()),
        owner = q(&paths.lock_owner()),
    )
}

pub(super) fn parse_lock_state(output: &str) -> Result<LockState, DeviceError> {
    match split_line(output) {
        ("free", "") => Ok(LockState::Free),
        ("held", owner) => Ok(LockState::Held(parse_owner(owner))),
        _ => Err(output_error("lock state", "unexpected output")),
    }
}

/// Prints `true` or `false`.
pub(super) fn marker_script(paths: &TabletPaths) -> String {
    format!(
        "if [ -e {} ]; then echo true; else echo false; fi\n",
        q(&paths.watchdog_marker())
    )
}

pub(super) fn parse_bool(operation: &'static str, output: &str) -> Result<bool, DeviceError> {
    match output.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(output_error(
            operation,
            format!("expected true or false, got {other:?}"),
        )),
    }
}

// ---- watchdog --------------------------------------------------------------
//
// State files: `watchdog.unit` holds `<unit> <run>` for the systemd
// watchdog, `watchdog.pid` holds `<process group> <boot id> <run>` for
// the sleep fallback, and `watchdog.token` the token the current sleeper
// must find (or `disarmed`). Recording the arming run keeps one run's
// disarm from stopping another run's watchdog.

/// What the watchdog does when it fires: try to leave the marker (so a
/// live run stops before xochitl is back), then reset-failed + start,
/// whether or not the marker could be written (`true`, not the special
/// builtin `:`, so a failed redirection cannot end the shell).
pub(super) fn fire_command(paths: &TabletPaths) -> String {
    format!(
        "true > {} 2>/dev/null; systemctl reset-failed xochitl.service; systemctl start xochitl.service",
        q(&paths.watchdog_marker())
    )
}

/// The transient unit of one arming: a fresh name per arming, so the new
/// timer exists before the old one is stopped.
pub(super) fn watchdog_unit_name(token: &str) -> String {
    format!("{WATCHDOG_UNIT}-{token}")
}

/// The primary watchdog: a transient systemd timer.
pub(super) fn systemd_watchdog_command(paths: &TabletPaths, secs: u64, unit: &str) -> String {
    format!(
        "systemd-run --quiet --collect --unit={} --on-active={secs}s /bin/sh -c {}",
        q(unit),
        q(&fire_command(paths))
    )
}

/// Stop the systemd unit whose name is in the shell variable `variable`,
/// if any.
fn stop_recorded_unit(variable: &str) -> String {
    format!(
        "if [ -n \"${variable}\" ]; then systemctl stop \"${variable}.timer\" >/dev/null 2>&1; \
         systemctl reset-failed \"${variable}.timer\" \"${variable}.service\" >/dev/null 2>&1; fi\n",
    )
}

/// Kill the sleeper recorded in `oldp`/`oldb` if its process group is
/// valid and from this boot (after a reboot the number may belong to an
/// unrelated process). No `--`: BusyBox `ash`'s `kill` builtin rejects
/// it, and `rmu_pid_ok` makes `-<group>` unambiguous.
const KILL_OLD_SLEEPER: &str = "if rmu_pid_ok \"$oldp\" && [ \"$oldb\" = \"$boot\" ]; then kill -TERM \"-$oldp\" 2>/dev/null; fi\n";

/// Read the recorded watchdogs: `old`/`oldr` (unit, run) and
/// `oldp`/`oldb`/`oldpr` (process group, boot id, run).
fn read_old_watchdog(paths: &TabletPaths) -> String {
    format!(
        "old=; oldr=; [ -f {unit} ] && read -r old oldr < {unit}\n\
         oldp=; oldb=; oldpr=; [ -f {pid} ] && read -r oldp oldb oldpr < {pid}\n",
        unit = q(&paths.watchdog_unit()),
        pid = q(&paths.watchdog_pid()),
    )
}

/// The fallback without `systemd-run`: a detached `sleep` in its own
/// session. When it wakes it fires unless the token file holds another
/// token (a later arming) or `disarmed`; a missing token file (the state
/// dir was lost) still fires, since it may be all that brings xochitl
/// back. Disarming writes `disarmed` before killing the recorded process
/// group, so a sleeper that survived a missed `kill` never fires. It must
/// not inherit the guard `flock` (fd 9). Arming fails unless the new
/// sleeper is alive, and only then is the old one killed.
///
/// Limitation: on systems whose sshd puts each session in its own
/// systemd unit and kills its whole cgroup on disconnect, the sleeper
/// dies with the session despite `setsid`. The systemd timer has no such
/// problem and is used wherever `systemd-run` exists (every known model).
pub(super) fn sleep_watchdog_command(
    paths: &TabletPaths,
    secs: u64,
    token: &str,
    run: &str,
) -> String {
    let body = format!(
        "sleep {secs}; t=; [ -f {tokens} ] && read -r t < {tokens}; [ -z \"$t\" ] || [ \"$t\" = {token} ] || exit 0; rm -f -- {tokens} {pid} 2>/dev/null; {fire}",
        tokens = q(&paths.watchdog_token()),
        token = q(token),
        pid = q(&paths.watchdog_pid()),
        fire = fire_command(paths),
    );
    format!(
        "command -v setsid >/dev/null 2>&1 || {{ echo 'rmu: neither systemd-run nor setsid is available for the watchdog' >&2; exit 1; }}\n\
         printf '%s\\n' {token} > {tokens} || exit 1\n\
         setsid sh -c {body} </dev/null >/dev/null 2>&1 9>&- &\n\
         p=$!\n\
         kill -0 \"$p\" 2>/dev/null || {{ echo 'rmu: the watchdog sleeper did not start' >&2; exit 1; }}\n\
         printf '%s %s %s\\n' \"$p\" \"$boot\" {run} > {pid} || exit 1\n",
        token = q(token),
        tokens = q(&paths.watchdog_token()),
        body = q(&body),
        run = q(run),
        pid = q(&paths.watchdog_pid()),
    )
}

/// Arm (or re-arm) the watchdog: guard, arm the new one (the systemd
/// timer if `systemd-run` exists, else the sleep fallback), record it
/// with this run, and only then remove the previous one (any run's), so
/// a failed arming leaves the old watchdog in place.
pub(super) fn arm_script(paths: &TabletPaths, run: &str, secs: u64, token: &str) -> String {
    let unit = watchdog_unit_name(token);
    guarded(
        paths,
        run,
        &format!(
            "read -r boot < {boot_id} || exit 1\n\
             {read_old}if command -v systemd-run >/dev/null 2>&1; then\n\
             {systemd} || exit 1\n\
             printf '%s %s\\n' {unit} {run} > {unit_file} || exit 1\n\
             printf 'disarmed\\n' > {tokens}; rm -f -- {pid}\n\
             else\n\
             {sleep}rm -f -- {unit_file}\n\
             fi\n\
             {stop_old}{kill_old}true",
            boot_id = q(&paths.boot_id),
            read_old = read_old_watchdog(paths),
            systemd = systemd_watchdog_command(paths, secs, &unit),
            unit = q(&unit),
            run = q(run),
            unit_file = q(&paths.watchdog_unit()),
            tokens = q(&paths.watchdog_token()),
            pid = q(&paths.watchdog_pid()),
            sleep = sleep_watchdog_command(paths, secs, token, run),
            stop_old = stop_recorded_unit("old"),
            kill_old = KILL_OLD_SLEEPER,
        ),
    )
}

/// Remove the watchdog this run armed, of either kind; another run's is
/// left alone. Refused ([`EXIT_XOCHITL_STOPPED`]) while xochitl is
/// stopped. A sleeper's token becomes `disarmed` before the kill.
pub(super) fn disarm_script(paths: &TabletPaths, run: &str) -> String {
    guarded(
        paths,
        run,
        &format!(
            "systemctl is-active --quiet xochitl.service || {{ echo 'rmu: refusing to disarm the watchdog while xochitl is stopped' >&2; exit {stopped}; }}\n\
             read -r boot < {boot_id} || exit 1\n\
             {read_old}if [ -n \"$old\" ] && [ \"$oldr\" = {run} ]; then\n\
             rm -f -- {unit_file}\n\
             {stop_old}fi\n\
             if [ -n \"$oldp\" ] && [ \"$oldpr\" = {run} ]; then\n\
             printf 'disarmed\\n' > {tokens}; rm -f -- {pid}\n\
             {kill_old}fi\n\
             true",
            stopped = EXIT_XOCHITL_STOPPED,
            boot_id = q(&paths.boot_id),
            read_old = read_old_watchdog(paths),
            run = q(run),
            tokens = q(&paths.watchdog_token()),
            unit_file = q(&paths.watchdog_unit()),
            pid = q(&paths.watchdog_pid()),
            stop_old = stop_recorded_unit("old"),
            kill_old = KILL_OLD_SLEEPER,
        ),
    )
}

// ---- xochitl and guard checks -----------------------------------------------

/// Stop xochitl, refused ([`EXIT_NOT_ARMED`]) unless a watchdog armed by
/// this run is pending: its systemd timer is active, or its sleeper's
/// process group (from this boot) is alive.
pub(super) fn stop_script(paths: &TabletPaths, run: &str) -> String {
    guarded(
        paths,
        run,
        &format!(
            "read -r boot < {boot_id} || exit 1\n\
             u=; ur=; [ -f {unit_file} ] && read -r u ur < {unit_file}\n\
             p=; pb=; pr=; [ -f {pid} ] && read -r p pb pr < {pid}\n\
             if {{ [ -n \"$u\" ] && [ \"$ur\" = {run} ] && systemctl is-active --quiet \"$u.timer\"; }} \\\n\
             || {{ [ \"$pr\" = {run} ] && [ \"$pb\" = \"$boot\" ] && rmu_pid_ok \"$p\" && kill -0 \"-$p\" 2>/dev/null; }}; then :\n\
             else echo 'rmu: refusing to stop xochitl without a watchdog armed by this run' >&2; exit {not_armed}; fi\n\
             systemctl stop xochitl.service",
            boot_id = q(&paths.boot_id),
            unit_file = q(&paths.watchdog_unit()),
            pid = q(&paths.watchdog_pid()),
            run = q(run),
            not_armed = EXIT_NOT_ARMED,
        ),
    )
}

/// Whether xochitl may be started: if the state dir exists, under the
/// `flock`, the lock must be ours or free (only another run's lock
/// refuses). A lost state dir (or a fired watchdog) never blocks it.
fn start_check(paths: &TabletPaths) -> String {
    format!(
        "if [ -d {state} ]; then rmu_lock; [ ! -d {lock} ] || rmu_owned; fi",
        state = q(&paths.state_dir),
        lock = q(&paths.lock_dir()),
    )
}

/// Prints the device time (unix seconds), then reset-failed + start
/// (see [`start_check`]).
pub(super) fn start_script(paths: &TabletPaths, run: &str) -> String {
    format!(
        "{fns}rmu_main() {{\n\
         {check}\n\
         date +%s\n\
         systemctl reset-failed xochitl.service 2>/dev/null\n\
         systemctl start xochitl.service\n\
         }}\n\
         rmu_main </dev/null\n",
        fns = guard_fns(paths, run),
        check = start_check(paths),
    )
}

/// Only the check of [`start_script`] (after a lost reply).
pub(super) fn start_allowed_script(paths: &TabletPaths, run: &str) -> String {
    format!(
        "{fns}rmu_main() {{\n{check}\n}}\nrmu_main </dev/null\n",
        fns = guard_fns(paths, run),
        check = start_check(paths),
    )
}

pub(super) const IS_ACTIVE_SCRIPT: &str = "systemctl is-active xochitl.service\n";

/// Unix seconds as nanoseconds.
pub(super) fn parse_device_time(operation: &'static str, output: &str) -> Result<i64, DeviceError> {
    let line = split_line(output).0.trim();
    line.parse::<i64>()
        .ok()
        .and_then(|secs| secs.checked_mul(NANOS_PER_SEC))
        .ok_or_else(|| output_error(operation, format!("invalid device time {line:?}")))
}

/// The survey's pairing check: prints only `true` or `false` (the token
/// never leaves the device). A configuration that cannot be read fails
/// rather than reading as unpaired: the `sed` runs on its own, not in a
/// pipeline whose status is `grep`'s.
pub(super) fn cloud_paired_script(paths: &TabletPaths) -> String {
    format!(
        "t=$(sed -n 's/^devicetoken=//p' {conf}) || {{ echo 'rmu: cannot read the xochitl configuration' >&2; exit 1; }}\n\
         printf '%s\\n' \"$t\" | grep -qvE '^(|@Invalid\\(\\)|@ByteArray\\(\\)|\"\")$' && echo true || echo false\n",
        conf = q(&paths.xochitl_conf)
    )
}

// ---- stat pass ---------------------------------------------------------------

/// Stat every non-directory entry of the data dir in one `find`. Names
/// with a newline or a carriage return cannot be listed line by line
/// (a trailing `\r` would vanish in line splitting and alias another
/// name); they are pruned and counted (`skip` lines). Any diagnostic from
/// `find`/`stat` fails the pass: a partial listing must never read as
/// deleted files.
pub(super) fn stat_script(paths: &TabletPaths) -> String {
    format!(
        "rmu_stat() {{\n\
         cd {dir} || exit 9\n\
         err=$(mktemp) || exit 1\n\
         nl='\n'\n\
         cr=$(printf '\\r')\n\
         find . \\( -name \"*$nl*\" -o -name \"*$cr*\" \\) -prune -exec printf 'skip\\n' \\; -o ! -type d -exec stat -c {format} -- {{}} + 2>\"$err\"\n\
         status=$?\n\
         if [ \"$status\" -ne 0 ] || [ -s \"$err\" ]; then cat \"$err\" >&2; rm -f \"$err\"; exit 1; fi\n\
         rm -f \"$err\"\n\
         }}\n\
         rmu_stat </dev/null\n",
        dir = q(&paths.xochitl_dir),
        format = q(STAT_FORMAT),
    )
}

/// Nanoseconds from `%Y` (whole seconds) and the fraction of `%y`/`%z`
/// (`YYYY-MM-DD HH:MM:SS[.fffffffff] ±ZZZZ`, local time). Using `%Y` for
/// the seconds avoids time-zone arithmetic; the seconds field of the
/// human-readable time must agree with it (zone offsets are whole
/// minutes), which catches a misaligned line.
fn stat_nanos(secs: &str, clock: &str) -> Option<i64> {
    let secs: i64 = secs.parse().ok()?;
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let field: i64 = hms.rsplit(':').next()?.parse().ok()?;
    if hms.split(':').count() != 3 || field != secs.rem_euclid(60) {
        return None;
    }
    if fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let nanos: i64 = format!("{fraction:0<9}").parse().ok()?;
    secs.checked_mul(NANOS_PER_SEC)?.checked_add(nanos)
}

/// One line of `stat -c` [`STAT_FORMAT`] output.
pub(super) fn parse_stat_line(line: &str) -> Option<StatEntry> {
    let fields: Vec<&str> = line.splitn(12, ' ').collect();
    let [
        mode,
        size,
        mtime,
        _mdate,
        mclock,
        _mzone,
        ctime,
        _cdate,
        cclock,
        _czone,
        inode,
        name,
    ] = fields.as_slice()
    else {
        return None;
    };
    let path = name.strip_prefix("./").unwrap_or(name);
    if path.is_empty() {
        return None;
    }
    Some(StatEntry {
        path: path.to_string(),
        mode: u32::from_str_radix(mode, 16).ok()?,
        stat: StatSignature {
            size: size.parse().ok()?,
            mtime_ns: stat_nanos(mtime, mclock)?,
            ctime_ns: stat_nanos(ctime, cclock)?,
            inode: inode.parse().ok()?,
        },
    })
}

fn parse_stat_lines<'a>(
    operation: &'static str,
    lines: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<StatEntry>, DeviceError> {
    let mut entries = lines
        .into_iter()
        .map(|line| {
            parse_stat_line(line)
                .ok_or_else(|| output_error(operation, format!("unparsable stat line {line:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    // Every name is listed once: a repeat means the output is not what
    // it seems (e.g. two names that differ only where a line split cut).
    if let Some(pair) = entries.windows(2).find(|pair| pair[0].path == pair[1].path) {
        return Err(output_error(
            operation,
            format!("{} listed twice", pair[0].path),
        ));
    }
    Ok(entries)
}

pub(super) fn parse_stat_output(output: &str) -> Result<StatListing, DeviceError> {
    let (skipped, lines): (Vec<&str>, Vec<&str>) = output
        .lines()
        .filter(|line| !line.is_empty())
        .partition(|line| *line == "skip");
    Ok(StatListing {
        entries: parse_stat_lines("stat pass", lines)?,
        unlisted: skipped.len(),
    })
}

// ---- downloads, reads, hashes --------------------------------------------------

/// Stream a tar of the files named on stdin (one per line, see
/// [`download_list`]); `--no-recursion` so a directory name never pulls
/// in more than asked.
pub(super) fn download_command(paths: &TabletPaths) -> String {
    format!(
        "cd {} && tar -cf - --no-recursion -T -",
        q(&paths.xochitl_dir)
    )
}

/// The file list for [`download_command`]. Every name gets a `./`
/// prefix, so a name starting with `-` is never read as an option.
pub(super) fn download_list<'a>(files: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    files
        .into_iter()
        .map(|file| format!("./{file}\n"))
        .collect::<String>()
        .into_bytes()
}

/// A regular file and not a symlink; anything else that exists fails.
const REGULAR_OR_FAIL: &str = "elif [ -e \"$f\" ] || [ -L \"$f\" ]; then printf 'rmu: not a regular file: %s\\n' \"$f\" >&2; exit 1\n";

/// For each file in order: `present <size>` and exactly that many bytes,
/// or `absent`; then `end`. Anything that exists but is not a regular
/// file fails.
pub(super) fn read_files_script<'a>(
    paths: &TabletPaths,
    files: impl IntoIterator<Item = &'a str>,
) -> String {
    format!(
        "rmu_read() {{\n\
         cd {dir} || exit 9\n\
         for f in {list}; do\n\
         if [ -f \"$f\" ] && [ ! -L \"$f\" ]; then\n\
         n=$(wc -c < \"$f\") || exit 1\n\
         n=$((n))\n\
         printf 'present %s\\n' \"$n\"\n\
         head -c \"$n\" -- \"./$f\" || exit 1\n\
         {not_regular}\
         else\n\
         printf 'absent\\n'\n\
         fi\n\
         done\n\
         printf 'end\\n'\n\
         }}\n\
         rmu_read </dev/null\n",
        dir = q(&paths.xochitl_dir),
        list = quoted_list(files),
        not_regular = REGULAR_OR_FAIL,
    )
}

/// Parse [`read_files_script`] output for `count` files, in order.
pub(super) fn parse_read_frames(
    output: &[u8],
    count: usize,
) -> Result<Vec<Option<Vec<u8>>>, DeviceError> {
    let error = |detail: &str| output_error("read files", detail);
    let header = |rest: &[u8]| -> Result<(String, usize), DeviceError> {
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| error("truncated output"))?;
        Ok((String::from_utf8_lossy(&rest[..end]).into_owned(), end + 1))
    };
    let (frames, offset) = (0..count).try_fold(
        (Vec::with_capacity(count), 0usize),
        |(mut frames, offset), _| {
            let (line, used) = header(&output[offset..])?;
            let offset = offset + used;
            if line == "absent" {
                frames.push(None);
                return Ok((frames, offset));
            }
            let size: usize = line
                .strip_prefix("present ")
                .and_then(|n| n.trim().parse().ok())
                .ok_or_else(|| error(&format!("unexpected header {line:?}")))?;
            let end = offset
                .checked_add(size)
                .ok_or_else(|| error("file size out of range"))?;
            let body = output
                .get(offset..end)
                .ok_or_else(|| error("truncated file body"))?;
            frames.push(Some(body.to_vec()));
            Ok((frames, end))
        },
    )?;
    if &output[offset..] == b"end\n" {
        Ok(frames)
    } else {
        Err(error("missing end of output"))
    }
}

/// `sha256sum` over the existing files, in batches; `absent <name>` for
/// missing ones. Anything that exists but is not a regular file (a
/// symlink included) fails.
pub(super) fn sha256_script(paths: &TabletPaths, files: &[&str]) -> String {
    let batches = files
        .chunks(ARGS_PER_CALL)
        .map(|batch| {
            format!(
                "set --\n\
                 for f in {list}; do\n\
                 if [ -f \"$f\" ] && [ ! -L \"$f\" ]; then set -- \"$@\" \"./$f\"\n\
                 {not_regular}\
                 else printf 'absent %s\\n' \"$f\"; fi\n\
                 done\n\
                 [ \"$#\" -eq 0 ] || sha256sum -- \"$@\" || exit 1\n",
                list = quoted_list(batch.iter().copied()),
                not_regular = REGULAR_OR_FAIL,
            )
        })
        .collect::<String>();
    format!(
        "rmu_hash() {{\n\
         cd {dir} || exit 9\n\
         {batches}}}\n\
         rmu_hash </dev/null\n",
        dir = q(&paths.xochitl_dir),
    )
}

/// Parse [`sha256_script`] output: every requested file answered exactly
/// once, by name.
pub(super) fn parse_sha256<'a>(
    output: &str,
    files: &[&'a str],
) -> Result<Vec<(&'a str, Option<String>)>, DeviceError> {
    let error = |detail: String| output_error("sha256sum", detail);
    let answers = output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            if let Some(name) = line.strip_prefix("absent ") {
                return Ok((name, None));
            }
            match line.split_once("  ") {
                Some((hash, name))
                    if hash.len() == 64
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
                {
                    // Names are passed as `./<name>` (so `-` is not stdin).
                    Ok((
                        name.strip_prefix("./").unwrap_or(name),
                        Some(hash.to_string()),
                    ))
                }
                _ => Err(error(format!("unexpected line {line:?}"))),
            }
        })
        .try_fold(std::collections::BTreeMap::new(), |mut map, answer| {
            let (name, hash) = answer?;
            if map.insert(name, hash).is_some() {
                return Err(error(format!("{name} answered twice")));
            }
            Ok(map)
        })?;
    let result = files
        .iter()
        .map(|file| {
            answers
                .get(file)
                .map(|hash| (*file, hash.clone()))
                .ok_or_else(|| error(format!("no answer for {file}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if answers.len() != files.len() {
        return Err(error(
            "answers for files that were not requested".to_string(),
        ));
    }
    Ok(result)
}

// ---- state, staging, apply -----------------------------------------------------

pub(super) fn read_state_script(paths: &TabletPaths) -> String {
    format!(
        "f={}\n\
         if [ -e \"$f\" ]; then printf 'present\\n'; cat -- \"$f\" || exit 1; else printf 'absent\\n'; fi\n",
        q(&paths.state_file())
    )
}

/// `None` if the tablet has no state file, else its bytes.
pub(super) fn parse_state_output(output: &[u8]) -> Result<Option<&[u8]>, DeviceError> {
    if let Some(body) = output.strip_prefix(b"present\n") {
        Ok(Some(body))
    } else if output == b"absent\n" {
        Ok(None)
    } else {
        Err(output_error("read state", "unexpected output"))
    }
}

/// Receive `size` bytes on stdin into a private sibling of the state
/// file, verify the length, `sync` (so the rename cannot outlive the
/// data), then, under the guard, rename it into place and `sync` again
/// (atomic and durable). The guard is also checked before the
/// transfer, to fail fast.
pub(super) fn write_state_command(paths: &TabletPaths, run: &str, size: usize) -> String {
    format!(
        "{fns}rmu_guard\n\
         tmp=$(mktemp -- {template}) || exit 1\n\
         trap 'rm -f -- \"$tmp\"' EXIT\n\
         trap 'exit 1' HUP INT TERM\n\
         cat > \"$tmp\" || exit 1\n\
         [ \"$(wc -c < \"$tmp\")\" -eq {size} ] || {{ echo 'rmu: incomplete state upload' >&2; exit 1; }}\n\
         sync\n\
         rmu_lock\n\
         rmu_guard\n\
         mv -f -- \"$tmp\" {file} || exit 1\n\
         sync\n",
        fns = guard_fns(paths, run),
        template = q(&format!("{}/.rmu-state-XXXXXX", paths.state_dir)),
        file = q(&paths.state_file()),
    )
}

/// Extract a tar from stdin into the run's staging directory, then give
/// the staged files fresh (nanosecond) mtimes, which they keep when moved
/// into place. Not under the `flock` (a network transfer); it only
/// writes into this run's own directory.
pub(super) fn stage_command(paths: &TabletPaths, run: &str, staging: &str) -> String {
    format!(
        "{fns}rmu_guard\n\
         mkdir -p -- {staging} && cd {staging} && tar -xf - && find . -type f -exec touch -c -- {{}} +\n",
        fns = guard_fns(paths, run),
        staging = q(staging),
    )
}

pub(super) fn clear_staging_script(paths: &TabletPaths, run: &str) -> String {
    guarded(
        paths,
        run,
        &format!("rm -rf -- {}", q(&paths.staging_root())),
    )
}

fn render_step(step: &ApplyStep) -> String {
    match step {
        ApplyStep::Guard => "rmu_guard".to_string(),
        ApplyStep::MakeDir(dir) => format!("mkdir -p -- {} || exit 1", q(dir)),
        ApplyStep::Move(path) => {
            format!("mv -fT -- \"$S\"/{path} {path} || exit 1", path = q(path))
        }
        ApplyStep::Remove(path) => format!("rm -f -- {} || exit 1", q(path)),
        // The `.*` glob stays outside the quotes; `rm -f` ignores an
        // unmatched literal.
        ApplyStep::DeleteSet(doc) => {
            format!("rm -rf -- {doc} {doc}.* || exit 1", doc = q(doc.as_str()))
        }
        ApplyStep::Done(doc) => format!("printf 'rmu-done %s\\n' {}", q(doc.as_str())),
    }
}

/// The apply, under the guard `flock`, refused ([`EXIT_XOCHITL_RUNNING`])
/// while xochitl runs. Before changing anything: staging must be on the
/// data dir's filesystem (BusyBox `mv` would silently copy across
/// filesystems, which is not atomic), every staged file must be a
/// regular file of its planned size (a partial upload is never applied),
/// executables get their mode, and `sync` (so no rename can outlive its
/// file's data in a power cut). Then the steps in order, `sync` again,
/// the device time, and a stat of every written file; `rmu-complete`
/// last.
pub(super) fn apply_script(
    paths: &TabletPaths,
    run: &str,
    staging: &str,
    steps: &[ApplyStep],
    staged: &[StagedPath],
) -> String {
    let stats = staged
        .chunks(ARGS_PER_CALL)
        .map(|batch| {
            format!(
                "stat -c {} -- {} || exit 1\n",
                q(STAT_FORMAT),
                quoted_list(batch.iter().map(|file| file.path.as_str()))
            )
        })
        .collect::<String>();
    let checks = staged
        .iter()
        .map(|file| {
            format!(
                "rmu_staged {} {}{}\n",
                q(&file.path),
                file.size,
                if file.executable {
                    format!(" && chmod 755 -- \"$S\"/{} || exit 1", q(&file.path))
                } else {
                    String::new()
                }
            )
        })
        .collect::<String>();
    let staged = if staged.is_empty() {
        String::new()
    } else {
        format!(
            "[ \"$(stat -c %d -- \"$S\")\" = \"$(stat -c %d -- .)\" ] || {{ echo 'rmu: staging is not on the data filesystem' >&2; exit 1; }}\n\
             rmu_staged() {{\n\
             [ -f \"$S/$1\" ] && [ ! -L \"$S/$1\" ] && [ \"$(wc -c < \"$S/$1\")\" -eq \"$2\" ] || {{ printf 'rmu: not staged as planned: %s\\n' \"$1\" >&2; exit 1; }}\n\
             }}\n\
             {checks}"
        )
    };
    let body = steps
        .iter()
        .map(|step| format!("{}\n", render_step(step)))
        .collect::<String>();
    format!(
        "{fns}rmu_apply() {{\n\
         rmu_lock\n\
         rmu_guard\n\
         if systemctl is-active --quiet xochitl.service; then echo 'rmu: refusing to apply while xochitl is running' >&2; exit {running}; fi\n\
         cd {dir} || exit 9\n\
         S={staging}\n\
         {staged}sync\n\
         {body}sync\n\
         printf 'rmu-time %s\\n' \"$(date +%s)\"\n\
         {stats}printf 'rmu-complete\\n'\n\
         }}\n\
         rmu_apply </dev/null\n",
        fns = guard_fns(paths, run),
        running = EXIT_XOCHITL_RUNNING,
        dir = q(&paths.xochitl_dir),
        staging = q(staging),
    )
}

/// What the apply script printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ApplyOutput {
    pub applied: Vec<DocId>,
    pub recorded_at_ns: Option<i64>,
    pub written: Vec<StatEntry>,
    pub complete: bool,
}

pub(super) fn parse_apply_output(output: &str) -> Result<ApplyOutput, DeviceError> {
    let error = |detail: String| output_error("apply", detail);
    let (out, stat_lines) = output.lines().filter(|line| !line.is_empty()).try_fold(
        (ApplyOutput::default(), Vec::new()),
        |(mut out, mut stats), line| {
            if out.complete {
                return Err(error(format!("output after completion: {line:?}")));
            }
            if let Some(doc) = line.strip_prefix("rmu-done ") {
                out.applied
                    .push(DocId::new(doc).map_err(|_| error(format!("invalid document {doc:?}")))?);
            } else if let Some(time) = line.strip_prefix("rmu-time ") {
                out.recorded_at_ns = Some(
                    parse_device_time("apply", time)
                        .map_err(|_| error(format!("invalid time {time:?}")))?,
                );
            } else if line == "rmu-complete" {
                out.complete = true;
            } else {
                stats.push(line);
            }
            Ok((out, stats))
        },
    )?;
    Ok(ApplyOutput {
        written: parse_stat_lines("apply", stat_lines)?,
        ..out
    })
}

/// The documents an apply finished, from whatever output arrived: only
/// complete (newline-terminated) `rmu-done` lines count.
pub(super) fn applied_documents(output: &str) -> Vec<DocId> {
    let complete = output.rsplit_once('\n').map_or("", |(lines, _)| lines);
    complete
        .lines()
        .filter_map(|line| line.strip_prefix("rmu-done "))
        .filter_map(|doc| DocId::new(doc).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::device::apply::{ApplyPlan, DocApply, StagedFile};
    use crate::hub::model::{DocFile, PageId};

    const RUN: &str = "0123456789abcdef0123456789abcdef";
    const DOC: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const VERSION: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Directories with every character a shell might trip over.
    fn hostile() -> TabletPaths {
        TabletPaths {
            xochitl_dir: "/data/it's $(rm -rf /) `x` \"q\" dir".into(),
            state_dir: "/state/a b;c&d|e\nf".into(),
            xochitl_conf: "/conf/x'y.conf".into(),
            boot_id: "/proc/boot id".into(),
        }
    }

    fn request() -> LockRequest {
        LockRequest::new("laptop", 1, 2).with_run(RUN)
    }

    /// Every path a script may mention, longest first.
    fn known_paths(paths: &TabletPaths, extra: &[&str]) -> Vec<String> {
        let mut known: Vec<String> = [
            paths.xochitl_dir.clone(),
            paths.state_dir.clone(),
            paths.xochitl_conf.clone(),
            paths.boot_id.clone(),
            paths.state_file(),
            paths.lock_dir(),
            paths.lock_owner(),
            paths.guard_flock(),
            paths.watchdog_marker(),
            paths.watchdog_unit(),
            paths.watchdog_token(),
            paths.watchdog_pid(),
            paths.staging_root(),
            format!("{}/.rmu-state-XXXXXX", paths.state_dir),
        ]
        .into_iter()
        .chain(extra.iter().map(|path| path.to_string()))
        .collect();
        known.sort_by_key(|path| std::cmp::Reverse(path.len()));
        known
    }

    /// No hostile fragment survives once every quoted path is removed:
    /// paths only ever appear in their quoted form.
    fn assert_quoted_with(script: &str, paths: &TabletPaths, extra: &[&str]) {
        let stripped = known_paths(paths, extra)
            .iter()
            .fold(script.to_string(), |text, path| {
                text.replace(&shell_quote(path), "<path>")
            });
        ["$(rm -rf /)", "a b;c&d|e\nf", "x'y", "boot id", "`x`"]
            .into_iter()
            .for_each(|fragment| {
                assert!(
                    !stripped.contains(fragment),
                    "unquoted {fragment:?} in:\n{script}"
                );
            });
    }

    fn assert_quoted(script: &str, paths: &TabletPaths) {
        assert_quoted_with(script, paths, &[]);
    }

    fn position(script: &str, needle: &str) -> usize {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in:\n{script}"))
    }

    #[test]
    fn guard_takes_the_flock_then_checks_marker_and_ownership() {
        let paths = TabletPaths::default();
        let fns = guard_fns(&paths, RUN);
        // The directory is checked first: in ash a failed `exec`
        // redirection exits the shell before any `||` runs.
        assert!(fns.contains(
            "rmu_lock() {\n[ -d /home/root/.local/share/rmu ] || { echo 'rmu: the hub state directory is gone' >&2; exit 76; }\n\
             exec 9>>/home/root/.local/share/rmu/guard.flock\nn=0\nuntil flock -n 9; do\n\
             n=$((n + 1)); [ \"$n\" -le 180 ] || { echo 'rmu: the tablet is busy with another hub operation' >&2; exit 78; }\n\
             sleep 1\ndone\n}\n"
        ));
        assert!(fns.contains(&format!(
            "grep -qxF 'run={RUN}' /home/root/.local/share/rmu/lock/owner 2>/dev/null || {{ echo 'rmu: the hub lock is not held by this run' >&2; exit 76; }}"
        )));
        assert!(fns.contains(
            "if [ -e /home/root/.local/share/rmu/watchdog-fired ]; then echo 'rmu: the hub watchdog fired' >&2; exit 75; fi\nrmu_owned\n\
             read -r rmu_boot < /proc/sys/kernel/random/boot_id || exit 1\n\
             grep -qxF \"boot_id=$rmu_boot\" /home/root/.local/share/rmu/lock/owner 2>/dev/null || { echo 'rmu: the tablet rebooted since this run took its lock' >&2; exit 80; }\n"
        ));
        // A recorded pid must be a number above 1 before it is signalled.
        assert!(fns.contains("case \"$1\" in ''|*[!0-9]*) return 1 ;; esac\n[ \"$1\" -gt 1 ]\n"));
        // Wrapped in a function: nothing runs, the flock above all, before
        // the whole script has arrived.
        assert!(
            guarded(&paths, RUN, "body")
                .ends_with("rmu_main() {\nrmu_lock\nrmu_guard\nbody\n}\nrmu_main </dev/null\n")
        );

        assert_quoted(&guarded(&hostile(), RUN, "true"), &hostile());
    }

    #[test]
    fn lock_scripts_hold_the_flock_and_quote_everything() {
        let paths = hostile();
        let hostile_host = LockRequest::new("h'ost $(x)", 1, 2).with_run(RUN);
        [
            acquire_script(&paths, &hostile_host),
            takeover_script(&paths, &hostile_host, VERSION),
            release_script(&paths, RUN),
            lock_state_script(&paths),
            marker_script(&paths),
        ]
        .iter()
        .for_each(|script| assert_quoted(script, &paths));
        let defaults = TabletPaths::default();
        [
            acquire_script(&defaults, &request()),
            takeover_script(&defaults, &request(), VERSION),
            release_script(&defaults, RUN),
        ]
        .iter()
        .for_each(|script| {
            // Every lock script runs inside a function and takes the flock
            // before changing anything.
            assert!(script.trim_end().ends_with("</dev/null"), "{script}");
            let flock = script.rfind("until flock -n 9").unwrap();
            let change = script
                .find("mkdir -- \"$l\"")
                .or_else(|| script.find("[ -d \"$l\" ]"))
                .or_else(|| script.find("rm -rf --"))
                .unwrap();
            assert!(flock < change, "{script}");
        });
        let takeover = takeover_script(&defaults, &request(), VERSION);
        assert!(takeover.contains(&format!("[ \"${{v%% *}}\" = {VERSION} ] || exit 77")));
        assert!(takeover.contains(OWNER_VERSION));
    }

    #[test]
    fn acquire_output_parses() {
        let boot = "6f1d3c2a-0000-4000-8000-00000000b007";
        assert_eq!(
            parse_acquire(&format!("acquired\n{boot}\n")).unwrap(),
            AcquireProbe::Acquired {
                boot_id: boot.into()
            }
        );
        assert_eq!(
            parse_acquire(&format!(
                "held\n{boot}\nfired\n{VERSION}  -\nrun=x\nhost=y\n"
            ))
            .unwrap(),
            AcquireProbe::Held {
                boot_id: boot.into(),
                watchdog_fired: true,
                owner_text: "run=x\nhost=y\n".into(),
                version: VERSION.into(),
            }
        );
        assert_eq!(
            parse_acquire(&format!("held\n{boot}\nclear\n{VERSION}  -\n")).unwrap(),
            AcquireProbe::Held {
                boot_id: boot.into(),
                watchdog_fired: false,
                owner_text: String::new(),
                version: VERSION.into(),
            }
        );
        assert!(parse_acquire("").is_err());
        assert!(parse_acquire("acquired\n\n").is_err());
        assert!(parse_acquire(&format!("held\n{boot}\nmaybe\n{VERSION}  -\n")).is_err());
        assert!(parse_acquire(&format!("held\n{boot}\nclear\nshort  -\n")).is_err());
        assert!(parse_acquire(&format!("held\nnot a boot id\nclear\n{VERSION}  -\n")).is_err());
        assert_eq!(parse_boot(&format!("{boot}\n")).unwrap(), boot);
    }

    #[test]
    fn lock_state_parses() {
        assert_eq!(parse_lock_state("free\n").unwrap(), LockState::Free);
        assert_eq!(parse_lock_state("held\n").unwrap(), LockState::Held(None));
        let record = super::super::lock::owner_record(&request(), "b00t");
        let parsed = parse_lock_state(&format!("held\n{record}")).unwrap();
        assert!(matches!(parsed, LockState::Held(Some(owner)) if owner.run == RUN));
        assert!(parse_lock_state("free\nextra").is_err());
        assert!(parse_lock_state("").is_err());
    }

    #[test]
    fn watchdog_systemd_variant() {
        let paths = TabletPaths::default();
        assert_eq!(watchdog_unit_name("tok"), "rmu-hub-watchdog-tok");
        assert_eq!(
            systemd_watchdog_command(&paths, 300, "rmu-hub-watchdog-tok"),
            "systemd-run --quiet --collect --unit=rmu-hub-watchdog-tok --on-active=300s /bin/sh -c \
             'true > /home/root/.local/share/rmu/watchdog-fired 2>/dev/null; systemctl reset-failed xochitl.service; systemctl start xochitl.service'"
        );
    }

    #[test]
    fn watchdog_sleep_variant() {
        let paths = TabletPaths::default();
        assert_eq!(
            sleep_watchdog_command(&paths, 90, "tok", RUN),
            format!(
                "command -v setsid >/dev/null 2>&1 || {{ echo 'rmu: neither systemd-run nor setsid is available for the watchdog' >&2; exit 1; }}\n\
                 printf '%s\\n' tok > /home/root/.local/share/rmu/watchdog.token || exit 1\n\
                 setsid sh -c 'sleep 90; t=; [ -f /home/root/.local/share/rmu/watchdog.token ] \
                 && read -r t < /home/root/.local/share/rmu/watchdog.token; [ -z \"$t\" ] || [ \"$t\" = tok ] || exit 0; \
                 rm -f -- /home/root/.local/share/rmu/watchdog.token /home/root/.local/share/rmu/watchdog.pid 2>/dev/null; \
                 true > /home/root/.local/share/rmu/watchdog-fired 2>/dev/null; \
                 systemctl reset-failed xochitl.service; systemctl start xochitl.service' </dev/null >/dev/null 2>&1 9>&- &\n\
                 p=$!\n\
                 kill -0 \"$p\" 2>/dev/null || {{ echo 'rmu: the watchdog sleeper did not start' >&2; exit 1; }}\n\
                 printf '%s %s %s\\n' \"$p\" \"$boot\" {RUN} > /home/root/.local/share/rmu/watchdog.pid || exit 1\n"
            )
        );
    }

    #[test]
    fn rearming_arms_the_new_watchdog_before_removing_the_old() {
        let paths = TabletPaths::default();
        let arm = arm_script(&paths, RUN, 120, "tok");
        let guard = position(&arm, "rmu_main() {\nrmu_lock\nrmu_guard\n");
        let read_old = position(
            &arm,
            "old=; oldr=; [ -f /home/root/.local/share/rmu/watchdog.unit ] && read -r old oldr <",
        );
        let systemd = position(
            &arm,
            "systemd-run --quiet --collect --unit=rmu-hub-watchdog-tok --on-active=120s",
        );
        let sleep = position(&arm, "setsid sh -c");
        let stop_old = position(&arm, "systemctl stop \"$old.timer\"");
        let kill_old = position(&arm, "kill -TERM \"-$oldp\"");
        assert!(guard < read_old && read_old < systemd && systemd < sleep);
        assert!(sleep < stop_old && stop_old < kill_old);
        assert!(arm.contains("if command -v systemd-run >/dev/null 2>&1; then"));
        // Arming records this run with the watchdog.
        assert!(arm.contains(&format!(
            "printf '%s %s\\n' rmu-hub-watchdog-tok {RUN} > /home/root/.local/share/rmu/watchdog.unit"
        )));
        // The old sleeper is only killed if its group is valid and from
        // this boot, and only once the new sleeper is alive.
        assert!(arm.contains("if rmu_pid_ok \"$oldp\" && [ \"$oldb\" = \"$boot\" ]"));
        assert!(position(&arm, "kill -0 \"$p\"") < kill_old);
        // Disarm: refused while xochitl is stopped, only this run's
        // watchdog, token marked before any kill.
        let disarm = disarm_script(&paths, RUN);
        let guard = position(&disarm, "rmu_main() {\nrmu_lock\nrmu_guard\n");
        let active = position(
            &disarm,
            "systemctl is-active --quiet xochitl.service || { echo 'rmu: refusing to disarm the watchdog while xochitl is stopped' >&2; exit 79; }",
        );
        assert!(guard < active);
        assert!(disarm.contains(&format!(
            "if [ -n \"$old\" ] && [ \"$oldr\" = {RUN} ]; then"
        )));
        assert!(disarm.contains(&format!(
            "if [ -n \"$oldp\" ] && [ \"$oldpr\" = {RUN} ]; then"
        )));
        assert!(
            position(
                &disarm,
                "printf 'disarmed\\n' > /home/root/.local/share/rmu/watchdog.token"
            ) < position(&disarm, "kill -TERM")
        );
        // Nested quoting (the fire command inside `sh -c`) is exercised by
        // running these scripts in the contract tests.
        assert_quoted(&disarm_script(&hostile(), RUN), &hostile());
    }

    #[test]
    fn start_checks_ownership_and_resets_failed_state_first() {
        let script = start_script(&TabletPaths::default(), RUN);
        // Refused only by another run's lock; a lost state dir or a fired
        // watchdog never blocks it.
        let owned = position(
            &script,
            "rmu_main() {\nif [ -d /home/root/.local/share/rmu ]; then rmu_lock; \
             [ ! -d /home/root/.local/share/rmu/lock ] || rmu_owned; fi\n",
        );
        let date = position(&script, "date +%s");
        let reset = position(&script, "systemctl reset-failed xochitl.service");
        let start = position(&script, "systemctl start xochitl.service");
        assert!(owned < date && date < reset && reset < start);
        assert!(!script.contains("rmu_lock\nrmu_guard\n"));
        let stop = stop_script(&TabletPaths::default(), RUN);
        assert!(stop.contains("rmu_main() {\nrmu_lock\nrmu_guard\n"));
        // Only with this run's pending watchdog, of either kind.
        assert!(
            position(&stop, "systemctl is-active --quiet \"$u.timer\"")
                < position(&stop, "systemctl stop xochitl.service")
        );
        assert!(stop.contains(&format!(
            "[ \"$pr\" = {RUN} ] && [ \"$pb\" = \"$boot\" ] && rmu_pid_ok \"$p\" && kill -0 \"-$p\""
        )));
        assert!(stop.contains("exit 82; fi\nsystemctl stop xochitl.service\n}\n"));
        assert!(start_allowed_script(&TabletPaths::default(), RUN).contains("rmu_owned; fi\n}\n"));
        assert_eq!(
            parse_device_time("t", "1790000000\n").unwrap(),
            1_790_000_000 * NANOS_PER_SEC
        );
        assert!(parse_device_time("t", "soon\n").is_err());
    }

    #[test]
    fn cloud_pairing_is_the_survey_one_liner_and_fails_closed() {
        assert_eq!(
            cloud_paired_script(&TabletPaths::default()),
            "t=$(sed -n 's/^devicetoken=//p' /home/root/.config/remarkable/xochitl.conf) || { echo 'rmu: cannot read the xochitl configuration' >&2; exit 1; }\n\
             printf '%s\\n' \"$t\" | grep -qvE '^(|@Invalid\\(\\)|@ByteArray\\(\\)|\"\")$' && echo true || echo false\n"
        );
        assert!(parse_bool("p", "true\n").unwrap());
        assert!(!parse_bool("p", "false\n").unwrap());
        assert!(parse_bool("p", "token=secret\n").is_err());
        assert_quoted(&cloud_paired_script(&hostile()), &hostile());
    }

    #[test]
    fn stat_lines_parse_nanoseconds() {
        let line = "81a4 1234 1790000001 2026-09-21 14:13:21.123456789 +0200 1790000002 \
                    2026-09-21 14:13:22.000000001 +0200 98765 ./aaaaaaaa-0000-4000-8000-000000000001.metadata";
        let entry = parse_stat_line(line).unwrap();
        assert_eq!(entry.path, "aaaaaaaa-0000-4000-8000-000000000001.metadata");
        assert_eq!(entry.mode, 0o100_644);
        assert_eq!(
            entry.stat,
            StatSignature {
                size: 1234,
                mtime_ns: 1_790_000_001_123_456_789,
                ctime_ns: 1_790_000_002_000_000_001,
                inode: 98765,
            }
        );
        assert!(entry.is_regular());
    }

    #[test]
    fn stat_lines_with_coarse_times_and_odd_names() {
        // Whole-second firmware: no fraction at all.
        let coarse = parse_stat_line(
            "81ed 0 1790000001 2026-09-21 12:13:21 +0000 1790000001 2026-09-21 12:13:21 +0000 7 ./x",
        )
        .unwrap();
        assert_eq!(coarse.stat.mtime_ns, 1_790_000_001_000_000_000);
        assert!(coarse.is_executable());
        // A short fraction is padded on the right.
        let short = parse_stat_line(
            "81a4 0 1790000001 2026-09-21 12:13:21.5 +0000 1790000001 2026-09-21 12:13:21.25 +0000 7 x",
        )
        .unwrap();
        assert_eq!(short.stat.mtime_ns, 1_790_000_001_500_000_000);
        assert_eq!(short.stat.ctime_ns, 1_790_000_001_250_000_000);
        // Spaces (leading, inner, trailing), quotes, dollar signs, symlinks.
        let odd = parse_stat_line(
            "a1ff 9 1790000001 2026-09-21 12:13:21.000000000 +0000 1790000001 2026-09-21 12:13:21.000000000 +0000 7 ./ a  'b' $c ",
        )
        .unwrap();
        assert_eq!(odd.path, " a  'b' $c ");
        assert!(!odd.is_regular());
        // Pre-epoch times keep the seconds check consistent.
        let old = parse_stat_line(
            "81a4 0 -1 1969-12-31 23:59:59.900000000 +0000 0 1970-01-01 00:00:00.000000000 +0000 7 x",
        )
        .unwrap();
        assert_eq!(old.stat.mtime_ns, -NANOS_PER_SEC + 900_000_000);
    }

    #[test]
    fn misaligned_stat_lines_are_refused() {
        [
            "",
            "81a4 1 2 3",
            // Seconds field disagrees with %Y.
            "81a4 0 1790000001 2026-09-21 12:13:22.0 +0000 1790000001 2026-09-21 12:13:21.0 +0000 7 x",
            // Ten fraction digits.
            "81a4 0 1790000001 2026-09-21 12:13:21.0123456789 +0000 1790000001 2026-09-21 12:13:21 +0000 7 x",
            "zz 0 1790000001 2026-09-21 12:13:21 +0000 1790000001 2026-09-21 12:13:21 +0000 7 x",
            "81a4 -1 1790000001 2026-09-21 12:13:21 +0000 1790000001 2026-09-21 12:13:21 +0000 7 x",
            "81a4 0 1790000001 2026-09-21 12:13:21 +0000 1790000001 2026-09-21 12:13:21 +0000 7 ./",
        ]
        .into_iter()
        .for_each(|line| assert_eq!(parse_stat_line(line), None, "{line:?}"));
    }

    #[test]
    fn stat_output_counts_unlisted_and_sorts() {
        let output = "skip\n\
            81a4 1 1790000001 2026-09-21 12:13:21.1 +0000 1790000001 2026-09-21 12:13:21.1 +0000 2 ./b\n\
            skip\n\
            81a4 1 1790000001 2026-09-21 12:13:21.1 +0000 1790000001 2026-09-21 12:13:21.1 +0000 1 ./a/c.rm\n";
        let listing = parse_stat_output(output).unwrap();
        assert_eq!(listing.unlisted, 2);
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["a/c.rm", "b"]
        );
        assert!(parse_stat_output("garbage\n").is_err());
        assert_eq!(parse_stat_output("").unwrap(), StatListing::default());
        // A name listed twice (e.g. `x` and a `x\r` that line splitting
        // cut) means the listing is not what it seems.
        let line = "81a4 1 1790000001 2026-09-21 12:13:21.1 +0000 1790000001 2026-09-21 12:13:21.1 +0000 2 ./x";
        assert!(parse_stat_output(&format!("{line}\n{line}\r\n")).is_err());
        // Names with a newline or a carriage return are pruned.
        let script = stat_script(&TabletPaths::default());
        assert!(script.contains("cr=$(printf '\\r')\n"));
        assert!(script.contains("find . \\( -name \"*$nl*\" -o -name \"*$cr*\" \\) -prune"));
        let script = stat_script(&hostile());
        assert_quoted(&script, &hostile());
        assert!(script.contains(&format!("stat -c {}", shell_quote(STAT_FORMAT))));
    }

    #[test]
    fn download_lists_are_dot_prefixed() {
        assert_eq!(
            download_list(["a.metadata", "-x", "d/p.rm"]),
            b"./a.metadata\n./-x\n./d/p.rm\n"
        );
        assert_eq!(download_list([]), b"");
        assert_quoted(&download_command(&hostile()), &hostile());
    }

    #[test]
    fn read_frames_parse() {
        let output = b"present 3\nabcabsent\npresent  0\nend\n";
        assert_eq!(
            parse_read_frames(output, 3).unwrap(),
            vec![Some(b"abc".to_vec()), None, Some(Vec::new())]
        );
        // Binary bodies with newlines.
        assert_eq!(
            parse_read_frames(b"present 2\n\n\nend\n", 1).unwrap(),
            vec![Some(b"\n\n".to_vec())]
        );
        assert!(parse_read_frames(b"present 5\nabcend\n", 1).is_err());
        assert!(parse_read_frames(b"present 3\nabc", 1).is_err());
        assert!(parse_read_frames(b"absent\nabsent\nend\n", 1).is_err());
        assert!(parse_read_frames(b"weird\nend\n", 1).is_err());
        assert!(parse_read_frames(format!("present {}\nx", usize::MAX).as_bytes(), 1).is_err());
        assert_eq!(parse_read_frames(b"end\n", 0).unwrap(), vec![]);
        let script = read_files_script(&hostile(), ["it's.metadata", "$(x).content"]);
        assert!(script.contains("for f in 'it'\\''s.metadata' '$(x).content'; do"));
        // Exactly the announced bytes, even if the file grew meanwhile.
        assert!(script.contains("head -c \"$n\" -- \"./$f\""));
        assert_quoted(&script, &hostile());
    }

    #[test]
    fn sha256_output_parses_by_name() {
        let files = ["a b", "missing", "c"];
        let output = format!("absent missing\n{VERSION}  a b\n{VERSION}  c\n");
        assert_eq!(
            parse_sha256(&output, &files).unwrap(),
            vec![
                ("a b", Some(VERSION.to_string())),
                ("missing", None),
                ("c", Some(VERSION.to_string()))
            ]
        );
        assert!(parse_sha256(&format!("{VERSION}  a b\n"), &files).is_err());
        assert!(parse_sha256(&format!("{VERSION}  a b\n{VERSION}  a b\n"), &["a b"]).is_err());
        assert!(parse_sha256(&format!("{VERSION}  x\n"), &[]).is_err());
        assert!(parse_sha256("ABC  a\n", &["a"]).is_err());
        let many: Vec<String> = (0..600).map(|i| format!("f{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let script = sha256_script(&TabletPaths::default(), &many);
        assert_eq!(script.matches("sha256sum --").count(), 3);
        assert!(script.contains("if [ -f \"$f\" ] && [ ! -L \"$f\" ]; then"));
    }

    #[test]
    fn state_write_checks_the_guard_under_the_flock_before_the_rename() {
        assert_eq!(parse_state_output(b"absent\n").unwrap(), None);
        assert_eq!(
            parse_state_output(b"present\n{}").unwrap(),
            Some(&b"{}"[..])
        );
        assert!(parse_state_output(b"").is_err());
        let script = write_state_command(&TabletPaths::default(), RUN, 42);
        let receive = position(&script, "cat > \"$tmp\"");
        let length = position(&script, "-eq 42 ]");
        let synced = position(&script, "\nsync\nrmu_lock\n");
        let lock = position(&script, "rmu_lock\nrmu_guard\nmv -f -- \"$tmp\"");
        let sync = script.rfind("\nsync\n").unwrap();
        assert!(receive < length && length < synced && synced < lock && lock < sync);
        assert_quoted(&write_state_command(&hostile(), RUN, 1), &hostile());
        assert_quoted(&read_state_script(&hostile()), &hostile());
    }

    #[test]
    fn staging_checks_the_guard_without_holding_the_flock() {
        let script = stage_command(&TabletPaths::default(), RUN, "/st/run");
        assert!(script.contains("rmu_guard\nmkdir -p -- /st/run && cd /st/run && tar -xf - && find . -type f -exec touch -c -- {} +\n"));
        assert!(!script.contains("rmu_lock\n"));
        assert_quoted(&clear_staging_script(&hostile(), RUN), &hostile());
    }

    #[test]
    fn apply_script_orders_steps_and_checks_the_marker() {
        let page = DocFile::Page(PageId::new("p1").unwrap());
        let plan = ApplyPlan {
            docs: vec![
                DocApply::Write {
                    doc: DocId::new(DOC).unwrap(),
                    write: vec![
                        StagedFile::regular(DocFile::Metadata, 12),
                        StagedFile::regular(page, 3),
                        StagedFile {
                            file: DocFile::Content,
                            size: 7,
                            mode: crate::git::FileMode::Executable,
                        },
                    ],
                    remove: vec![],
                },
                DocApply::Delete {
                    doc: DocId::new("bbbbbbbb-0000-4000-8000-000000000002").unwrap(),
                },
            ],
        };
        let paths = TabletPaths::default();
        let script = apply_script(
            &paths,
            RUN,
            "/st/run",
            &plan.steps().unwrap(),
            &plan.staged(),
        );
        let body = script.split_once("rmu_apply() {\n").unwrap().1;
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines,
            vec![
                "rmu_lock",
                "rmu_guard",
                "if systemctl is-active --quiet xochitl.service; then echo 'rmu: refusing to apply while xochitl is running' >&2; exit 81; fi",
                "cd /home/root/.local/share/remarkable/xochitl || exit 9",
                "S=/st/run",
                "[ \"$(stat -c %d -- \"$S\")\" = \"$(stat -c %d -- .)\" ] || { echo 'rmu: staging is not on the data filesystem' >&2; exit 1; }",
                "rmu_staged() {",
                "[ -f \"$S/$1\" ] && [ ! -L \"$S/$1\" ] && [ \"$(wc -c < \"$S/$1\")\" -eq \"$2\" ] || { printf 'rmu: not staged as planned: %s\\n' \"$1\" >&2; exit 1; }",
                "}",
                &format!(
                    "rmu_staged {DOC}.content 7 && chmod 755 -- \"$S\"/{DOC}.content || exit 1"
                ),
                &format!("rmu_staged {DOC}.metadata 12"),
                &format!("rmu_staged {DOC}/p1.rm 3"),
                "sync",
                "rmu_guard",
                &format!("mkdir -p -- {DOC} || exit 1"),
                &format!("mv -fT -- \"$S\"/{DOC}/p1.rm {DOC}/p1.rm || exit 1"),
                &format!("mv -fT -- \"$S\"/{DOC}.content {DOC}.content || exit 1"),
                &format!("mv -fT -- \"$S\"/{DOC}.metadata {DOC}.metadata || exit 1"),
                &format!("printf 'rmu-done %s\\n' {DOC}"),
                "rmu_guard",
                "rm -f -- bbbbbbbb-0000-4000-8000-000000000002.metadata || exit 1",
                "rm -rf -- bbbbbbbb-0000-4000-8000-000000000002 bbbbbbbb-0000-4000-8000-000000000002.* || exit 1",
                "printf 'rmu-done %s\\n' bbbbbbbb-0000-4000-8000-000000000002",
                "sync",
                "printf 'rmu-time %s\\n' \"$(date +%s)\"",
                &format!(
                    "stat -c '%f %s %Y %y %Z %z %i %n' -- {DOC}.content {DOC}.metadata {DOC}/p1.rm || exit 1"
                ),
                "printf 'rmu-complete\\n'",
                "}",
                "rmu_apply </dev/null",
            ]
        );
        // The guard functions are defined before the body runs.
        assert!(script.starts_with("rmu_lock() {\n"));
    }

    #[test]
    fn apply_script_quotes_hostile_directories() {
        let plan = ApplyPlan {
            docs: vec![DocApply::Write {
                doc: DocId::new(DOC).unwrap(),
                write: vec![StagedFile::regular(DocFile::Metadata, 2)],
                remove: vec![],
            }],
        };
        let paths = hostile();
        let staging = format!("{}/x", paths.staging_root());
        let script = apply_script(
            &paths,
            RUN,
            &staging,
            &plan.steps().unwrap(),
            &plan.staged(),
        );
        assert_quoted_with(&script, &paths, &[&staging]);
        assert!(script.contains(&format!("S={}\n", shell_quote(&staging))));
        // No staging check and no stat call without written files.
        let deletion = ApplyPlan {
            docs: vec![DocApply::Delete {
                doc: DocId::new(DOC).unwrap(),
            }],
        };
        let script = apply_script(
            &paths,
            RUN,
            &staging,
            &deletion.steps().unwrap(),
            &deletion.staged(),
        );
        assert!(!script.contains("stat -c"));
        assert!(!script.contains("not staged"));
    }

    #[test]
    fn apply_output_parses() {
        let output = format!(
            "rmu-done {DOC}\nrmu-time 1790000000\n\
             81a4 1 1790000001 2026-09-21 12:13:21.1 +0000 1790000001 2026-09-21 12:13:21.1 +0000 2 {DOC}.metadata\n\
             rmu-complete\n"
        );
        let parsed = parse_apply_output(&output).unwrap();
        assert_eq!(parsed.applied, vec![DocId::new(DOC).unwrap()]);
        assert_eq!(parsed.recorded_at_ns, Some(1_790_000_000 * NANOS_PER_SEC));
        assert_eq!(parsed.written.len(), 1);
        assert!(parsed.complete);
        let partial = parse_apply_output(&format!("rmu-done {DOC}\n")).unwrap();
        assert!(!partial.complete);
        assert!(parse_apply_output("rmu-done nope\n").is_err());
        assert!(parse_apply_output("rmu-complete\nrmu-done x\n").is_err());
        assert!(parse_apply_output("garbage\n").is_err());
    }

    #[test]
    fn applied_documents_survive_truncated_output() {
        let doc = DocId::new(DOC).unwrap();
        assert_eq!(
            applied_documents(&format!("rmu-done {DOC}\n")),
            vec![doc.clone()]
        );
        // A cut-off last line does not count, garbage around it is ignored.
        assert_eq!(
            applied_documents(&format!("rmu-done {DOC}\n81a4 garbage\nrmu-done bbbb")),
            vec![doc]
        );
        assert_eq!(applied_documents(&format!("rmu-done {DOC}")), vec![]);
        assert_eq!(applied_documents(""), vec![]);
    }
}
