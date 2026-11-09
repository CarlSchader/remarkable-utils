//! `--json` output: the library's reports as they serialise, with a
//! top-level `command` naming the subcommand (`init` also carries the
//! `origin` it initialised). The layout is the reports' own (see
//! `libremarkable_utils::hub::cycle`): documents by id (`rmu ls --json`
//! maps ids to names), enums tagged by `kind` (`state` for the lock).
//! Errors are `{"error": {"kind", "exit_code", "message", …}}`, with the
//! documents for the deletion guard. Fields are only ever added; the
//! tests pin each command's top-level fields.

use libremarkable_utils::Error;
use libremarkable_utils::hub::cycle::{CycleError, InitReport};
use libremarkable_utils::hub::model::DocId;
use serde::Serialize;
use serde_json::Value;

use super::{Failure, Report};

/// A report with the command that produced it.
#[derive(Serialize)]
struct Tagged<'a, T: Serialize> {
    command: &'static str,
    #[serde(flatten)]
    report: &'a T,
}

#[derive(Serialize)]
struct InitJson<'a> {
    origin: &'a str,
    #[serde(flatten)]
    report: &'a InitReport,
}

fn tagged<T: Serialize>(command: &'static str, report: &T) -> serde_json::Result<Value> {
    serde_json::to_value(Tagged { command, report })
}

/// The JSON for a report.
pub fn report(report: &Report) -> Value {
    match report {
        Report::Init { origin, report } => tagged("init", &InitJson { origin, report }),
        Report::Clone(report) => tagged("clone", report),
        Report::Sync(report) => tagged("sync", report),
        Report::DryRun(report) => tagged("sync --dry-run", report),
        Report::Backup(report) => tagged("backup", report),
        Report::Status(report) => tagged("status", report),
        Report::Remote(report) => tagged("remote", report),
        Report::SetUrl(report) => tagged("remote set-url", report),
        Report::Detach(report) => tagged("detach", report),
    }
    .expect("reports serialise")
}

/// The JSON for an error; `error` gives the deletion guard's documents.
pub fn error(failure: Failure, message: &str, error: Option<&Error>) -> Value {
    let details = match error {
        Some(Error::HubCycle(CycleError::DeletionsNeedConsent {
            from_origin,
            from_tablet,
            empty_snapshot,
        })) => Some(GuardRefusal {
            from_origin,
            from_tablet,
            empty_snapshot: *empty_snapshot,
        }),
        _ => None,
    };
    serde_json::to_value(ErrorJson {
        error: ErrorBody {
            kind: failure.kind(),
            exit_code: failure.code(),
            message: message.to_string(),
            details,
        },
    })
    .expect("errors serialise")
}

#[derive(Serialize)]
struct ErrorJson<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: &'static str,
    exit_code: u8,
    message: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    details: Option<GuardRefusal<'a>>,
}

#[derive(Serialize)]
struct GuardRefusal<'a> {
    from_origin: &'a [DocId],
    from_tablet: &'a [DocId],
    empty_snapshot: bool,
}
