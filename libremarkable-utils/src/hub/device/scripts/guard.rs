//! The guard, lock, watchdog and xochitl scripts: everything that
//! decides who may change the tablet and keeps xochitl coming back.

use super::{
    EXIT_BUSY, EXIT_LOCK_CHANGED, EXIT_LOCK_LOST, EXIT_NOT_ARMED, EXIT_REBOOTED,
    EXIT_WATCHDOG_FIRED, EXIT_XOCHITL_STOPPED, FLOCK_WAIT_SECS, NANOS_PER_SEC, output_error, q,
};
use crate::hub::device::lock::{
    self, AcquireProbe, LockRequest, LockState, owner_fields, parse_owner, run_line,
};
use crate::hub::device::{DeviceError, TabletPaths, WATCHDOG_UNIT};
use crate::xochitl_service::{RESET_FAILED, START};

// ---- guard and lock --------------------------------------------------------

/// Take the guard `flock` on fd 9 (held until the script exits), waiting
/// at most [`FLOCK_WAIT_SECS`] ([`EXIT_BUSY`]); BusyBox `flock` has no
/// `-w`. The file's directory must exist: in `ash`, a failed `exec`
/// redirection exits the shell at once.
pub(in crate::hub::device) fn take_flock(paths: &TabletPaths) -> String {
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
pub(in crate::hub::device) fn guard_fns(paths: &TabletPaths, run: &str) -> String {
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
pub(in crate::hub::device) fn guarded(paths: &TabletPaths, run: &str, body: &str) -> String {
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
pub(in crate::hub::device) fn acquire_script(paths: &TabletPaths, request: &LockRequest) -> String {
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
    if lock::valid_boot_id(boot) {
        Ok(boot.to_string())
    } else {
        Err(output_error("lock", format!("invalid boot id {boot:?}")))
    }
}

pub(in crate::hub::device) fn parse_acquire(output: &str) -> Result<AcquireProbe, DeviceError> {
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
pub(in crate::hub::device) fn takeover_script(
    paths: &TabletPaths,
    request: &LockRequest,
    version: &str,
) -> String {
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

pub(in crate::hub::device) fn parse_boot(output: &str) -> Result<String, DeviceError> {
    boot_line(split_line(output).0)
}

/// Remove the lock if it is ours; exits [`EXIT_LOCK_LOST`] otherwise.
pub(in crate::hub::device) fn release_script(paths: &TabletPaths, run: &str) -> String {
    main_with(
        paths,
        run,
        "rmu_owned",
        &format!("rm -rf -- {}", q(&paths.lock_dir())),
    )
}

/// Prints `free`, or `held` and the owner record.
pub(in crate::hub::device) fn lock_state_script(paths: &TabletPaths) -> String {
    format!(
        "if [ -d {lock} ]; then printf 'held\\n'; cat -- {owner} 2>/dev/null; true; else printf 'free\\n'; fi\n",
        lock = q(&paths.lock_dir()),
        owner = q(&paths.lock_owner()),
    )
}

pub(in crate::hub::device) fn parse_lock_state(output: &str) -> Result<LockState, DeviceError> {
    match split_line(output) {
        ("free", "") => Ok(LockState::Free),
        ("held", owner) => Ok(LockState::Held(parse_owner(owner))),
        _ => Err(output_error("lock state", "unexpected output")),
    }
}

/// Prints `true` or `false`.
pub(in crate::hub::device) fn marker_script(paths: &TabletPaths) -> String {
    format!(
        "if [ -e {} ]; then echo true; else echo false; fi\n",
        q(&paths.watchdog_marker())
    )
}

pub(in crate::hub::device) fn parse_bool(
    operation: &'static str,
    output: &str,
) -> Result<bool, DeviceError> {
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
pub(in crate::hub::device) fn fire_command(paths: &TabletPaths) -> String {
    format!(
        "true > {} 2>/dev/null; {RESET_FAILED}; {START}",
        q(&paths.watchdog_marker())
    )
}

/// The transient unit of one arming: a fresh name per arming, so the new
/// timer exists before the old one is stopped.
pub(in crate::hub::device) fn watchdog_unit_name(token: &str) -> String {
    format!("{WATCHDOG_UNIT}-{token}")
}

/// The primary watchdog: a transient systemd timer.
pub(in crate::hub::device) fn systemd_watchdog_command(
    paths: &TabletPaths,
    secs: u64,
    unit: &str,
) -> String {
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
pub(in crate::hub::device) fn sleep_watchdog_command(
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
pub(in crate::hub::device) fn arm_script(
    paths: &TabletPaths,
    run: &str,
    secs: u64,
    token: &str,
) -> String {
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
pub(in crate::hub::device) fn disarm_script(paths: &TabletPaths, run: &str) -> String {
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
pub(in crate::hub::device) fn stop_script(paths: &TabletPaths, run: &str) -> String {
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
pub(in crate::hub::device) fn start_script(paths: &TabletPaths, run: &str) -> String {
    format!(
        "{fns}rmu_main() {{\n\
         {check}\n\
         date +%s\n\
         {RESET_FAILED} 2>/dev/null\n\
         {START}\n\
         }}\n\
         rmu_main </dev/null\n",
        fns = guard_fns(paths, run),
        check = start_check(paths),
    )
}

/// Only the check of [`start_script`] (after a lost reply).
pub(in crate::hub::device) fn start_allowed_script(paths: &TabletPaths, run: &str) -> String {
    format!(
        "{fns}rmu_main() {{\n{check}\n}}\nrmu_main </dev/null\n",
        fns = guard_fns(paths, run),
        check = start_check(paths),
    )
}

/// Unix seconds as nanoseconds.
pub(in crate::hub::device) fn parse_device_time(
    operation: &'static str,
    output: &str,
) -> Result<i64, DeviceError> {
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
pub(in crate::hub::device) fn cloud_paired_script(paths: &TabletPaths) -> String {
    format!(
        "t=$(sed -n 's/^devicetoken=//p' {conf}) || {{ echo 'rmu: cannot read the xochitl configuration' >&2; exit 1; }}\n\
         printf '%s\\n' \"$t\" | grep -qvE '^(|@Invalid\\(\\)|@ByteArray\\(\\)|\"\")$' && echo true || echo false\n",
        conf = q(&paths.xochitl_conf)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::device::scripts::fixtures::*;

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
        let record = lock::owner_record(&request(), "b00t");
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
}
