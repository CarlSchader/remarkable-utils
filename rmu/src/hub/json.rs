//! `--json` output, schema 1: the library's reports as they serialise,
//! with a top-level `schema` and `command` naming the subcommand (`init`
//! also carries the `origin` it initialised). The layout is the reports'
//! own (see `libremarkable_utils::hub::cycle`): documents by id, enums
//! tagged by `kind` (`state` for the lock). Errors are `{"schema",
//! "error": {"kind", "exit_code", "message"}}`, plus `from_origin`,
//! `from_tablet`, `empty_snapshot` and `names` for the deletion guard.
//!
//! **Unstable until device validation**: names will be settled then
//! (and `schema` bumped). Until then, golden tests pin every shape, so
//! a change in the library shows up here.

use std::collections::BTreeMap;

use libremarkable_utils::Error;
use libremarkable_utils::hub::cycle::{CycleError, InitReport};
use libremarkable_utils::hub::model::DocId;
use serde::Serialize;
use serde_json::Value;

use super::{Failure, Report};

/// The `schema` of every JSON object `rmu hub` prints.
pub const SCHEMA: u32 = 1;

/// A report with the command that produced it.
#[derive(Serialize)]
struct Tagged<'a, T: Serialize> {
    schema: u32,
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
    serde_json::to_value(Tagged {
        schema: SCHEMA,
        command,
        report,
    })
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
            names,
        })) => Some(GuardRefusal {
            from_origin,
            from_tablet,
            empty_snapshot: *empty_snapshot,
            names,
        }),
        _ => None,
    };
    serde_json::to_value(ErrorJson {
        schema: SCHEMA,
        error: ErrorBody {
            kind: failure,
            exit_code: failure.code(),
            message: message.to_string(),
            details,
        },
    })
    .expect("errors serialise")
}

#[derive(Serialize)]
struct ErrorJson<'a> {
    schema: u32,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: Failure,
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
    names: &'a BTreeMap<DocId, String>,
}
