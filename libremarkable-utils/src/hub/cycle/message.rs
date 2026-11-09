//! Commit messages and their `Rmu-*` trailers. Pure.
//!
//! Every commit rmu writes ends with a paragraph of trailers, one
//! `Key: value` per line: the device's name, id, model and firmware, and
//! for merge commits each fork (`Rmu-Fork: <original> <copy>`) and each
//! deletion from origin made with `--allow-deletions`
//! (`Rmu-Deletion-Consent: <uuid>`). Other tablets read the consent
//! trailers to apply such deletions without asking again, `status` reads
//! the fork trailers to list open conflict copies, and `clone` reads the
//! model trailers to warn about mixed models.

use std::collections::BTreeSet;

use crate::hub::model::{DeviceId, DeviceName, DocId};

pub(super) const DEVICE: &str = "Rmu-Device";
pub(super) const DEVICE_ID: &str = "Rmu-Device-Id";
pub(super) const MODEL: &str = "Rmu-Model";
pub(super) const FIRMWARE: &str = "Rmu-Firmware";
pub(super) const FORK: &str = "Rmu-Fork";
pub(super) const CONSENT: &str = "Rmu-Deletion-Consent";
pub(super) const QUARANTINED: &str = "Rmu-Quarantined";

/// Who wrote a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Author<'a> {
    pub name: &'a DeviceName,
    pub id: &'a DeviceId,
    pub model: Option<&'a str>,
    pub firmware: Option<&'a str>,
}

/// A trailer value on one line: control characters become `?`.
fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn device_trailers(origin: &Author) -> Vec<(&'static str, String)> {
    [
        Some((DEVICE, origin.name.to_string())),
        Some((DEVICE_ID, origin.id.to_string())),
        origin.model.map(|model| (MODEL, model.to_string())),
        origin
            .firmware
            .map(|firmware| (FIRMWARE, firmware.to_string())),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn compose(subject: &str, body: &[String], trailers: &[(&'static str, String)]) -> String {
    let body = body
        .iter()
        .map(|line| one_line(line))
        .collect::<Vec<_>>()
        .join("\n");
    let trailers = trailers
        .iter()
        .map(|(key, value)| format!("{key}: {}", one_line(value)))
        .collect::<Vec<_>>()
        .join("\n");
    match body.is_empty() {
        true => format!("{}\n\n{trailers}\n", one_line(subject)),
        false => format!("{}\n\n{body}\n\n{trailers}\n", one_line(subject)),
    }
}

/// The message of a snapshot commit (the device ref).
pub(super) fn snapshot(origin: &Author, quarantined: &BTreeSet<DocId>) -> String {
    let trailers: Vec<_> = device_trailers(origin)
        .into_iter()
        .chain(quarantined.iter().map(|doc| (QUARANTINED, doc.to_string())))
        .collect();
    compose(
        &format!("rmu hub: snapshot of {}", origin.name),
        &[],
        &trailers,
    )
}

/// What a merge commit records besides the device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct MergeRecord {
    /// Human-readable notes, one per line of the body.
    pub notes: Vec<String>,
    /// `(original, copy)` of every fork this merge created.
    pub forks: Vec<(DocId, DocId)>,
    /// Deletions from origin made with `--allow-deletions`.
    pub consented: BTreeSet<DocId>,
}

impl MergeRecord {
    /// Whether the commit must carry this record (a fast-forward cannot).
    pub fn is_empty(&self) -> bool {
        self.forks.is_empty() && self.consented.is_empty()
    }
}

/// The message of a merge commit (or of a snapshot that fast-forwards
/// `main`).
pub(super) fn merge(origin: &Author, record: &MergeRecord) -> String {
    let trailers: Vec<_> = device_trailers(origin)
        .into_iter()
        .chain(
            record
                .forks
                .iter()
                .map(|(original, copy)| (FORK, format!("{original} {copy}"))),
        )
        .chain(
            record
                .consented
                .iter()
                .map(|doc| (CONSENT, doc.to_string())),
        )
        .collect();
    compose(
        &format!("rmu hub: sync {}", origin.name),
        &record.notes,
        &trailers,
    )
}

/// The `Rmu-*` trailers of a message: `Key: value` lines of its last
/// paragraph whose key starts with `Rmu-`.
pub(super) fn trailers(message: &str) -> Vec<(String, String)> {
    let last = message.trim_end().rsplit("\n\n").next().unwrap_or_default();
    last.lines()
        .filter_map(|line| line.split_once(": "))
        .filter(|(key, _)| key.starts_with("Rmu-") && !key.contains(char::is_whitespace))
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect()
}

/// The values of one trailer key.
pub(super) fn values<'m>(
    trailers: &'m [(String, String)],
    key: &'m str,
) -> impl Iterator<Item = &'m str> + 'm {
    trailers
        .iter()
        .filter(move |(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Document ids among trailer values (anything malformed is ignored:
/// trailers only ever widen what rmu would do without them by exempting
/// recorded deletions, so a value that is not an id must not count).
pub(super) fn doc_ids<'m>(values: impl Iterator<Item = &'m str>) -> BTreeSet<DocId> {
    values.filter_map(|value| DocId::new(value).ok()).collect()
}

/// `(original, copy)` pairs of `Rmu-Fork` trailers.
pub(super) fn forks(trailers: &[(String, String)]) -> Vec<(DocId, DocId)> {
    values(trailers, FORK)
        .filter_map(|value| {
            let (original, copy) = value.split_once(' ')?;
            Some((DocId::new(original).ok()?, DocId::new(copy.trim()).ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(n: u32) -> DocId {
        DocId::new(format!("8c1f0b9e-0000-4000-8000-{n:012x}")).unwrap()
    }

    fn origin<'a>(name: &'a DeviceName, id: &'a DeviceId) -> Author<'a> {
        Author {
            name,
            id,
            model: Some("reMarkable Fake"),
            firmware: Some("1.2.3"),
        }
    }

    #[test]
    fn merge_messages_round_trip_their_trailers() {
        let name = DeviceName::new("Desk tablet").unwrap();
        let id = DeviceId::new("0f0e0d0c-0000-4000-8000-00000000c001").unwrap();
        let record = MergeRecord {
            notes: vec![
                "a: renamed on both\nsides".into(),
                "Rmu-Fake: not a trailer".into(),
            ],
            forks: vec![(doc(1), doc(2))],
            consented: [doc(3), doc(4)].into(),
        };
        let message = merge(&origin(&name, &id), &record);
        assert!(message.starts_with("rmu hub: sync Desk tablet\n\n"));
        let parsed = trailers(&message);
        assert_eq!(values(&parsed, DEVICE).collect::<Vec<_>>(), ["Desk tablet"]);
        assert_eq!(
            values(&parsed, MODEL).collect::<Vec<_>>(),
            ["reMarkable Fake"]
        );
        assert_eq!(forks(&parsed), vec![(doc(1), doc(2))]);
        assert_eq!(doc_ids(values(&parsed, CONSENT)), [doc(3), doc(4)].into());
        // Notes live in the body: a note shaped like a trailer is not one.
        assert!(values(&parsed, "Rmu-Fake").next().is_none());
        assert!(message.contains("a: renamed on both?sides"));
        assert!(!record.is_empty());
        assert!(MergeRecord::default().is_empty());
    }

    #[test]
    fn snapshot_messages_carry_the_device_and_quarantine() {
        let name = DeviceName::new("T").unwrap();
        let id = DeviceId::new("0f0e0d0c-0000-4000-8000-00000000c001").unwrap();
        let mut bare = origin(&name, &id);
        bare.model = None;
        let message = snapshot(&bare, &[doc(9)].into());
        let parsed = trailers(&message);
        assert_eq!(
            values(&parsed, DEVICE_ID).collect::<Vec<_>>(),
            [id.as_str()]
        );
        assert!(values(&parsed, MODEL).next().is_none());
        assert_eq!(doc_ids(values(&parsed, QUARANTINED)), [doc(9)].into());
    }

    #[test]
    fn malformed_trailers_are_ignored() {
        let message = format!(
            "subject\n\nRmu-Fork: {} not-a-uuid\nRmu-Deletion-Consent: nope\nRmu-Fork: x\n\
             Not-Rmu: {}\nRmu-Deletion-Consent: {}\n",
            doc(1),
            doc(2),
            doc(3)
        );
        let parsed = trailers(&message);
        assert!(forks(&parsed).is_empty());
        assert_eq!(doc_ids(values(&parsed, CONSENT)), [doc(3)].into());
        assert!(trailers("only a subject").is_empty());
    }
}
