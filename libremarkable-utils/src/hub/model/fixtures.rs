//! Helpers shared by the model's tests.

use serde_json::{Value, json};

use super::{
    DocFile, DocId, ListingEntry, MirrorRules, MirroredFile, PageId, StatSignature, TreeListing,
};
use crate::git::Oid;

pub(super) use super::rules::rules_without_pdf;

// Fabricated ids, in the shapes xochitl writes.
pub(super) const DOC: &str = "8c1f0b9e-0000-4000-8000-000000000001";
pub(super) const DOC2: &str = "8c1f0b9e-0000-4000-8000-000000000002";
pub(super) const FOLDER: &str = "8c1f0b9e-0000-4000-8000-00000000000f";
pub(super) const PAGE: &str = "8c1f0b9e-0000-4000-8000-0000000000a1";
pub(super) const PAGE2: &str = "8c1f0b9e-0000-4000-8000-0000000000a2";
pub(super) const REPO: &str = "0f0e0d0c-0000-4000-8000-00000000b001";
pub(super) const DEVICE: &str = "0f0e0d0c-0000-4000-8000-00000000c001";

pub(super) fn rules() -> &'static MirrorRules {
    MirrorRules::for_version(1).unwrap()
}

pub(super) fn doc(uuid: &str) -> DocId {
    DocId::new(uuid).unwrap()
}

pub(super) fn page(id: &str) -> PageId {
    PageId::new(id).unwrap()
}

pub(super) fn oid(n: u32) -> Oid {
    format!("{n:040x}").parse().unwrap()
}

pub(super) fn mirrored(uuid: &str, file: DocFile) -> MirroredFile {
    MirroredFile {
        doc: doc(uuid),
        file,
    }
}

pub(super) fn sample_metadata() -> Value {
    json!({
        "createdTime": "1767225600000",
        "lastModified": "1767312000000",
        "lastOpened": "1767398400000",
        "lastOpenedPage": 4,
        "new": false,
        "parent": "",
        "pinned": false,
        "source": "",
        "type": "DocumentType",
        "visibleName": "Example notebook",
        "futureField": { "kept": true }
    })
}

pub(super) fn sample_content() -> Value {
    json!({
        "cPages": {
            "lastOpened": { "timestamp": "1:12", "value": PAGE2 },
            "original": { "timestamp": "1:1", "value": 2 },
            "pages": [
                { "id": PAGE,
                  "idx": { "timestamp": "1:2", "value": "ba" },
                  "redir": { "timestamp": "1:2", "value": 0 } },
                { "id": "8c1f0b9e-0000-4000-8000-0000000000a3",
                  "idx": { "timestamp": "1:5", "value": "bam" },
                  "modifed": "1767312000000",
                  "template": { "timestamp": "1:6", "value": "Blank" } },
                { "id": PAGE2,
                  "idx": { "timestamp": "1:2", "value": "bb" },
                  "redir": { "timestamp": "1:2", "value": 1 },
                  "scrollTime": { "timestamp": "1:11", "value": "2026-01-02T00:00:00Z" },
                  "verticalScroll": { "timestamp": "1:11", "value": 0 } }
            ],
            "uuids": [ { "first": "0f0e0d0c-0000-4000-8000-00000000a001", "second": 1 } ],
            "futureRegister": { "timestamp": "1:3", "value": 1 }
        },
        "coverPageNumber": 0,
        "customZoomCenterX": 0, "customZoomCenterY": 936,
        "customZoomOrientation": "portrait",
        "customZoomPageHeight": 1872, "customZoomPageWidth": 1404,
        "customZoomScale": 1,
        "documentMetadata": { "title": "Example", "authors": ["A. Author"] },
        "extraMetadata": { "LastPen": "Ballpointv2", "LastActiveTool": "primary" },
        "fileType": "pdf",
        "fontName": "", "lineHeight": -1, "margins": 100,
        "formatVersion": 2,
        "orientation": "portrait",
        "pageCount": 3,
        "pageTags": [], "tags": [],
        "sizeInBytes": "1048576",
        "textAlignment": "left", "textScale": 1,
        "zoomMode": "bestFit"
    })
}

pub(super) fn listing(entries: &[(&str, u32)]) -> TreeListing {
    entries
        .iter()
        .map(|(path, n)| {
            (
                path.replace("{d}", DOC)
                    .replace("{d2}", DOC2)
                    .replace("{f}", FOLDER)
                    .replace("{p}", PAGE),
                ListingEntry::regular(oid(*n)),
            )
        })
        .collect()
}

pub(super) fn stat(size: u64) -> StatSignature {
    StatSignature {
        size,
        mtime_ns: 1_767_225_600_000_000_000,
        ctime_ns: 1_767_225_600_000_000_000,
        inode: 1,
    }
}
