//! The data scripts: the stat pass, downloads, reads and hashes, the
//! state file, staging and the apply.

use super::guard::{guard_fns, guarded, parse_device_time};
use super::{EXIT_XOCHITL_RUNNING, NANOS_PER_SEC, output_error, q};
use crate::hub::device::apply::{ApplyStep, StagedPath};
use crate::hub::device::{DeviceError, StatEntry, StatListing, TabletPaths};
use crate::hub::model::{DocId, StatSignature};
use crate::ssh::shell_quote;

/// `stat -c` format of the stat pass: raw mode (hex), size, mtime in
/// seconds and human-readable with nanoseconds, the same for ctime,
/// inode, name. The name comes last, so it may contain spaces.
pub(in crate::hub::device) const STAT_FORMAT: &str = "%f %s %Y %y %Z %z %i %n";

/// Files per `stat`/`sha256sum` invocation, keeping argv far below the
/// kernel's limit however many files are listed.
const ARGS_PER_CALL: usize = 256;

fn quoted_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    items
        .into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- stat pass ---------------------------------------------------------------

/// Stat every non-directory entry of the data dir in one `find`. Names
/// with a newline or a carriage return cannot be listed line by line
/// (a trailing `\r` would vanish in line splitting and alias another
/// name); they are pruned and counted (`skip` lines). Any diagnostic from
/// `find`/`stat` fails the pass: a partial listing must never read as
/// deleted files.
pub(in crate::hub::device) fn stat_script(paths: &TabletPaths) -> String {
    stat_script_in(&paths.xochitl_dir, false)
}

/// The stat pass over the staging root: every file interrupted runs left
/// staged, as `<run>/<xochitl path>`. A missing staging root lists
/// nothing.
pub(in crate::hub::device) fn staged_script(paths: &TabletPaths) -> String {
    stat_script_in(&paths.staging_root(), true)
}

fn stat_script_in(dir: &str, missing_ok: bool) -> String {
    let missing = if missing_ok {
        format!("[ -d {} ] || exit 0\n", q(dir))
    } else {
        String::new()
    };
    format!(
        "rmu_stat() {{\n\
         {missing}\
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
        dir = q(dir),
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
pub(in crate::hub::device) fn parse_stat_line(line: &str) -> Option<StatEntry> {
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

pub(in crate::hub::device) fn parse_stat_output(output: &str) -> Result<StatListing, DeviceError> {
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
pub(in crate::hub::device) fn download_command(paths: &TabletPaths) -> String {
    format!(
        "cd {} && tar -cf - --no-recursion -T -",
        q(&paths.xochitl_dir)
    )
}

/// The file list for [`download_command`]. Every name gets a `./`
/// prefix, so a name starting with `-` is never read as an option.
pub(in crate::hub::device) fn download_list<'a>(
    files: impl IntoIterator<Item = &'a str>,
) -> Vec<u8> {
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
pub(in crate::hub::device) fn read_files_script<'a>(
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
pub(in crate::hub::device) fn parse_read_frames(
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
pub(in crate::hub::device) fn sha256_script(paths: &TabletPaths, files: &[&str]) -> String {
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
pub(in crate::hub::device) fn parse_sha256<'a>(
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

pub(in crate::hub::device) fn read_state_script(paths: &TabletPaths) -> String {
    format!(
        "f={}\n\
         if [ -e \"$f\" ]; then printf 'present\\n'; cat -- \"$f\" || exit 1; else printf 'absent\\n'; fi\n",
        q(&paths.state_file())
    )
}

/// `None` if the tablet has no state file, else its bytes.
pub(in crate::hub::device) fn parse_state_output(
    output: &[u8],
) -> Result<Option<&[u8]>, DeviceError> {
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
pub(in crate::hub::device) fn write_state_command(
    paths: &TabletPaths,
    run: &str,
    size: usize,
) -> String {
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
pub(in crate::hub::device) fn stage_command(
    paths: &TabletPaths,
    run: &str,
    staging: &str,
) -> String {
    format!(
        "{fns}rmu_guard\n\
         mkdir -p -- {staging} && cd {staging} && tar -xf - && find . -type f -exec touch -c -- {{}} +\n",
        fns = guard_fns(paths, run),
        staging = q(staging),
    )
}

/// Delete the hub state file under the guard, then flush.
pub(in crate::hub::device) fn remove_state_script(paths: &TabletPaths, run: &str) -> String {
    guarded(
        paths,
        run,
        &format!("rm -f -- {} || exit 1\nsync", q(&paths.state_file())),
    )
}

pub(in crate::hub::device) fn clear_staging_script(paths: &TabletPaths, run: &str) -> String {
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
pub(in crate::hub::device) fn apply_script(
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
pub(in crate::hub::device) struct ApplyOutput {
    pub applied: Vec<DocId>,
    pub recorded_at_ns: Option<i64>,
    pub written: Vec<StatEntry>,
    pub complete: bool,
}

pub(in crate::hub::device) fn parse_apply_output(output: &str) -> Result<ApplyOutput, DeviceError> {
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
pub(in crate::hub::device) fn applied_documents(output: &str) -> Vec<DocId> {
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
    use crate::hub::device::scripts::fixtures::*;
    use crate::hub::model::{DocFile, PageId};

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
        assert!(!script.contains("|| exit 0"));
        // The staged listing runs the same pass over the staging root,
        // and lists nothing when there is none.
        let staged = staged_script(&hostile());
        assert_quoted(&staged, &hostile());
        let root = shell_quote(&hostile().staging_root());
        assert!(staged.contains(&format!("[ -d {root} ] || exit 0\ncd {root} || exit 9\n")));
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
        let script = remove_state_script(&TabletPaths::default(), RUN);
        assert!(position(&script, "rmu_guard\nrm -f -- ") < script.rfind("\nsync").unwrap());
        assert_quoted(&remove_state_script(&hostile(), RUN), &hostile());
    }

    #[test]
    fn staging_checks_the_guard_without_holding_the_flock() {
        // Run in `behaviour::staging_does_not_wait_for_the_guard_flock`
        // and `behaviour::staged_files_get_fresh_mtimes`. Pinned: nothing
        // is extracted unless the cd into the run's own directory worked.
        let staging = format!("{}/x", hostile().staging_root());
        let script = stage_command(&hostile(), RUN, &staging);
        assert!(script.contains(&format!(
            "rmu_guard\nmkdir -p -- {s} && cd {s} && tar -xf - && find . -type f -exec touch -c -- {{}} +\n",
            s = q(&staging)
        )));
        assert_quoted_with(&script, &hostile(), &[&staging]);
        assert_quoted(&clear_staging_script(&hostile(), RUN), &hostile());
    }

    #[test]
    fn every_step_renders_quoted_and_stops_the_apply_on_failure() {
        // `-T`: a target that exists as a directory fails the move instead
        // of receiving the file; `|| exit 1`: a failed step never goes on
        // to report its document done.
        let doc = DocId::new(DOC).unwrap();
        let path = "it's $(x)/-a b";
        let quoted = q(path);
        [
            (ApplyStep::Guard, "rmu_guard".to_string()),
            (
                ApplyStep::MakeDir(path.into()),
                format!("mkdir -p -- {quoted} || exit 1"),
            ),
            (
                ApplyStep::Move(path.into()),
                format!("mv -fT -- \"$S\"/{quoted} {quoted} || exit 1"),
            ),
            (
                ApplyStep::Remove(path.into()),
                format!("rm -f -- {quoted} || exit 1"),
            ),
            (
                ApplyStep::DeleteSet(doc.clone()),
                format!("rm -rf -- {DOC} {DOC}.* || exit 1"),
            ),
            (
                ApplyStep::Done(doc),
                format!("printf 'rmu-done %s\\n' {DOC}"),
            ),
        ]
        .into_iter()
        .for_each(|(step, line)| assert_eq!(render_step(&step), line));
    }

    #[test]
    fn apply_script_orders_its_phases() {
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
        let steps = plan.steps().unwrap();
        let script = apply_script(&paths, RUN, "/st/run", &steps, &plan.staged());
        // The plan's steps (ordered by `ApplyPlan::steps`, run by the
        // contract tests), as one block between the two syncs.
        let block: String = steps
            .iter()
            .map(|step| format!("{}\n", render_step(step)))
            .collect();
        // Before anything changes: the guard under the flock, the refusal
        // while xochitl runs, the staging checks (with the modes), a sync;
        // afterwards a sync, the time, the stats and the completion line.
        let running = format!("exit {EXIT_XOCHITL_RUNNING}; fi\n");
        let cd = format!("cd {} || exit 9\n", q(&paths.xochitl_dir));
        let content = format!("rmu_staged {DOC}.content 7 && chmod 755 -- ");
        let metadata = format!("rmu_staged {DOC}.metadata 12\n");
        let page = format!("rmu_staged {DOC}/p1.rm 3\n");
        let steps = format!("\nsync\n{block}sync\n");
        let stats = format!(
            "stat -c {} -- {DOC}.content {DOC}.metadata {DOC}/p1.rm || exit 1\n",
            q(STAT_FORMAT)
        );
        let order = [
            "rmu_apply() {\nrmu_lock\nrmu_guard\n",
            "if systemctl is-active --quiet xochitl.service; then",
            &running,
            &cd,
            "staging is not on the data filesystem",
            &content,
            &metadata,
            &page,
            &steps,
            "printf 'rmu-time %s\\n'",
            &stats,
            "printf 'rmu-complete\\n'\n}\nrmu_apply </dev/null\n",
        ]
        .map(|needle| position(&script, needle));
        assert!(order.is_sorted(), "{order:?} in:\n{script}");
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

    /// The scripts run over the mock transport, with the host's tools and
    /// with BusyBox.
    #[cfg(target_os = "linux")]
    mod behaviour {
        use std::fs;
        use std::path::Path;
        use std::time::{Duration, Instant};

        use super::*;
        use crate::error::{Error, Result};
        use crate::hub::device::harness::Harness;
        use crate::hub::device::harness::local_ssh::on_both;
        use crate::hub::device::{ApplyReport, HeldLock, Tablet};
        use crate::hub::model::XochitlPath;

        const OTHER: &str = "bbbbbbbb-0000-4000-8000-000000000002";

        #[test]
        fn staging_does_not_wait_for_the_guard_flock() {
            on_both(true, |harness| {
                let t = harness.ssh_tablet();
                let lock = t.acquire_lock(&request(), false).unwrap();
                let file = XochitlPath::new(format!("{DOC}.metadata")).unwrap();
                let mut holder = harness.hold_guard_flock("exec sleep 10");
                let started = Instant::now();
                let staged = t.stage(&lock, std::slice::from_ref(&file), &mut |_, sink| {
                    sink(2, &mut &b"{}"[..])
                });
                let took = started.elapsed();
                holder.kill().unwrap();
                holder.wait().unwrap();
                staged.unwrap();
                assert!(took < Duration::from_secs(8), "{took:?}");
                assert_eq!(t.staged_files().unwrap().entries.len(), 1);
            });
        }

        #[test]
        fn staged_files_get_fresh_mtimes() {
            on_both(true, |harness| {
                let t = harness.ssh_tablet();
                let lock = t.acquire_lock(&request(), false).unwrap();
                let files: Vec<XochitlPath> = [format!("{DOC}.metadata"), format!("{DOC}/p1.rm")]
                    .into_iter()
                    .map(|path| XochitlPath::new(path).unwrap())
                    .collect();
                t.stage(&lock, &files, &mut |_, sink| sink(2, &mut &b"{}"[..]))
                    .unwrap();
                // The tar headers carry whole seconds; the staged files get
                // the device's own (nanosecond) times, kept when moved.
                let staged = t.staged_files().unwrap().entries;
                assert_eq!(staged.len(), 2);
                staged
                    .iter()
                    .for_each(|entry| assert!(entry.stat.fine_mtime(), "{entry:?}"));
            });
        }

        /// Stage `{}` for the `.metadata` of `docs` and stop xochitl.
        fn staged_metadata(t: &dyn Tablet, lock: &HeldLock, docs: &[&str]) -> Vec<DocApply> {
            let files: Vec<XochitlPath> = docs
                .iter()
                .map(|doc| XochitlPath::new(format!("{doc}.metadata")).unwrap())
                .collect();
            t.stage(lock, &files, &mut |_, sink| sink(2, &mut &b"{}"[..]))
                .unwrap();
            t.arm_watchdog(lock, Duration::from_secs(600)).unwrap();
            t.stop_xochitl(lock).unwrap();
            docs.iter()
                .map(|doc| DocApply::Write {
                    doc: DocId::new(*doc).unwrap(),
                    write: vec![StagedFile::regular(DocFile::Metadata, 2)],
                    remove: vec![],
                })
                .collect()
        }

        /// An apply that failed with no document reported done.
        fn failed_with_nothing_done(result: Result<ApplyReport>) {
            match result {
                Ok(report) => panic!("applied: {report:?}"),
                Err(Error::HubDevice(DeviceError::ApplyIncomplete { applied, .. })) => {
                    assert!(applied.is_empty(), "{applied:?}")
                }
                Err(_) => {}
            }
        }

        #[test]
        fn a_failing_step_stops_the_apply() {
            on_both(true, |harness| {
                let t = harness.ssh_tablet();
                let lock = t.acquire_lock(&request(), false).unwrap();
                let docs = staged_metadata(t, &lock, &[DOC, OTHER]);
                // The first target is a non-empty directory: `mv -T` cannot
                // replace it, and the apply stops there.
                harness.put(&format!("{DOC}.metadata/keep"), b"x");
                failed_with_nothing_done(t.apply(&lock, &ApplyPlan { docs }));
                assert_eq!(harness.get(&format!("{DOC}.metadata/keep")).unwrap(), b"x");
                assert!(harness.data_dir().join(format!("{DOC}.metadata")).is_dir());
                assert_eq!(harness.get(&format!("{OTHER}.metadata")), None);
            });
        }

        #[test]
        fn a_staged_symlink_is_never_applied() {
            on_both(true, |harness| {
                let t = harness.ssh_tablet();
                let lock = t.acquire_lock(&request(), false).unwrap();
                let docs = staged_metadata(t, &lock, &[DOC]);
                // Replaced by a symlink to a file of the planned size.
                let paths = t.paths();
                let target = Path::new(&paths.state_dir).join("elsewhere");
                fs::write(&target, b"{}").unwrap();
                let staged = Path::new(&paths.staging_dir(&lock)).join(format!("{DOC}.metadata"));
                fs::remove_file(&staged).unwrap();
                std::os::unix::fs::symlink(&target, &staged).unwrap();
                failed_with_nothing_done(t.apply(&lock, &ApplyPlan { docs }));
                let applied = harness.data_dir().join(format!("{DOC}.metadata"));
                assert!(fs::symlink_metadata(applied).is_err());
            });
        }

        #[test]
        fn a_missing_data_dir_stops_the_apply_before_any_step() {
            on_both(true, |harness| {
                let t = harness.ssh_tablet();
                let lock = t.acquire_lock(&request(), false).unwrap();
                t.arm_watchdog(&lock, Duration::from_secs(600)).unwrap();
                t.stop_xochitl(&lock).unwrap();
                fs::remove_dir_all(harness.data_dir()).unwrap();
                // A deletion that ran anywhere but the data dir would
                // report its document done.
                let plan = ApplyPlan {
                    docs: vec![DocApply::Delete {
                        doc: DocId::new(DOC).unwrap(),
                    }],
                };
                let result = t.apply(&lock, &plan);
                assert!(
                    matches!(
                        &result,
                        Err(Error::HubDevice(DeviceError::ApplyIncomplete { applied, reason }))
                            if applied.is_empty() && reason.starts_with("exit Some(9)")
                    ),
                    "{result:?}"
                );
            });
        }

        #[test]
        fn a_deletion_takes_the_document_and_its_siblings_only() {
            on_both(true, |harness| {
                let t = harness.ssh_tablet();
                let lock = t.acquire_lock(&request(), false).unwrap();
                [
                    format!("{DOC}.metadata"),
                    format!("{DOC}.content"),
                    format!("{DOC}.local"),
                    format!("{DOC}.thumbnails/p1.png"),
                    format!("{DOC}/p1.rm"),
                    format!("{DOC}x.metadata"),
                    format!("x{DOC}.content"),
                ]
                .iter()
                .for_each(|path| harness.put(path, b"x"));
                t.arm_watchdog(&lock, Duration::from_secs(600)).unwrap();
                t.stop_xochitl(&lock).unwrap();
                let plan = ApplyPlan {
                    docs: vec![DocApply::Delete {
                        doc: DocId::new(DOC).unwrap(),
                    }],
                };
                assert_eq!(
                    t.apply(&lock, &plan).unwrap().applied,
                    vec![DocId::new(DOC).unwrap()]
                );
                t.start_xochitl(&lock).unwrap();
                let left: Vec<String> = t
                    .stat_all()
                    .unwrap()
                    .entries
                    .into_iter()
                    .map(|entry| entry.path)
                    .collect();
                assert_eq!(
                    left,
                    [format!("{DOC}x.metadata"), format!("x{DOC}.content")]
                );
            });
        }
    }
}
