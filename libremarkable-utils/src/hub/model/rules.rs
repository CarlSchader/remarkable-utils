//! Mirror rules, selected by the repository's mirror-rules version: the
//! allowlist of file kinds, the view-state, derived and model-specific
//! JSON paths, normalisation for "changed?" comparisons and the
//! view-state overlay used on apply.
//!
//! The path-mapping methods of [`MirrorRules`] (`classify`,
//! `parse_repo_path`, `xochitl_to_repo`, `repo_to_xochitl`) live with the
//! layout they map, in `layout.rs`; `group_tree` in `grouping.rs` and
//! `validate_snapshot` in `snapshot.rs`.

use std::fmt;

use serde_json::Value;

use super::{FileKind, ModelError, Result};

/// Newest mirror-rules version this rmu supports.
pub const SUPPORTED_MIRROR_RULES: u32 = 1;

/// One segment of a [`JsonPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg {
    /// An object key.
    Key(&'static str),
    /// Every element of an array. When overlaying, elements are matched
    /// by their `id` field (page entries).
    Each,
}

/// A JSON path such as `cPages.pages[*].scrollTime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonPath(&'static [Seg]);

impl JsonPath {
    pub fn segments(&self) -> &'static [Seg] {
        self.0
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .enumerate()
            .try_for_each(|(index, seg)| match seg {
                Seg::Key(key) if index == 0 => f.write_str(key),
                Seg::Key(key) => write!(f, ".{key}"),
                Seg::Each => f.write_str("[*]"),
            })
    }
}

macro_rules! json_path {
    (@seg *) => {
        Seg::Each
    };
    (@seg $key:literal) => {
        Seg::Key($key)
    };
    ($($seg:tt),+ $(,)?) => {
        JsonPath(&[$(json_path!(@seg $seg)),+])
    };
}

/// The two JSON files whose fields the rules distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JsonFile {
    Metadata,
    Content,
}

/// JSON path lists for `.metadata` and `.content`.
#[derive(Debug, Clone, Copy)]
struct PerFile {
    metadata: &'static [JsonPath],
    content: &'static [JsonPath],
}

impl PerFile {
    fn get(&self, file: JsonFile) -> &'static [JsonPath] {
        match file {
            JsonFile::Metadata => self.metadata,
            JsonFile::Content => self.content,
        }
    }
}

/// What a repository mirrors and how documents are compared, selected
/// by the repository's mirror-rules version (never rmu's newest: see
/// "Rules version" in `docs/hub-design.md`).
#[derive(Debug, Clone, Copy)]
pub struct MirrorRules {
    version: u32,
    kinds: &'static [FileKind],
    view_state: PerFile,
    derived: PerFile,
    model_specific: PerFile,
}

static RULES_V1: MirrorRules = MirrorRules {
    version: 1,
    kinds: &[
        FileKind::Metadata,
        FileKind::Content,
        FileKind::Pagedata,
        FileKind::Template,
        FileKind::PayloadPdf,
        FileKind::PayloadEpub,
        FileKind::Page,
        FileKind::PageMetadata,
        FileKind::Highlights,
    ],
    view_state: PerFile {
        metadata: &[
            json_path!("lastOpened"),
            json_path!("lastOpenedPage"),
            json_path!("new"),
            json_path!("metadatamodified"),
            json_path!("modified"),
            json_path!("synced"),
            json_path!("version"),
        ],
        content: &[
            json_path!("cPages", "lastOpened"),
            json_path!("cPages", "pages", *, "scrollTime"),
            json_path!("cPages", "pages", *, "verticalScroll"),
            json_path!("customZoomCenterX"),
            json_path!("customZoomCenterY"),
            json_path!("customZoomOrientation"),
            json_path!("customZoomPageHeight"),
            json_path!("customZoomPageWidth"),
            json_path!("customZoomScale"),
            json_path!("zoomMode"),
            json_path!("extraMetadata"),
            json_path!("lastOpenedPage"),
            json_path!("transform"),
            json_path!("keyboardMetadata"),
        ],
    },
    derived: PerFile {
        metadata: &[json_path!("lastModified")],
        content: &[
            json_path!("pageCount"),
            json_path!("sizeInBytes"),
            json_path!("cPages", "original"),
            json_path!("documentMetadata"),
            json_path!("cPages", "uuids"),
        ],
    },
    model_specific: PerFile {
        metadata: &[],
        content: &[
            json_path!("customZoomCenterX"),
            json_path!("customZoomCenterY"),
            json_path!("customZoomOrientation"),
            json_path!("customZoomPageHeight"),
            json_path!("customZoomPageWidth"),
            json_path!("customZoomScale"),
        ],
    },
};

impl MirrorRules {
    /// The rules for a repository's mirror-rules version; an error for
    /// versions newer than this rmu supports (or invalid ones).
    pub fn for_version(version: u32) -> Result<&'static MirrorRules> {
        match version {
            0 => Err(ModelError::InvalidRules(0)),
            1 => Ok(&RULES_V1),
            found => Err(ModelError::UnsupportedRules {
                found,
                supported: SUPPORTED_MIRROR_RULES,
            }),
        }
    }

    /// The newest rules, for `init`.
    pub fn latest() -> &'static MirrorRules {
        &RULES_V1
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether files of this kind are mirrored.
    pub fn allows(&self, kind: FileKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// View-state paths: device-local, never a change, overlaid on apply.
    pub fn view_state(&self, file: JsonFile) -> &'static [JsonPath] {
        self.view_state.get(file)
    }

    /// Derived paths: ignored when deciding whether a document changed.
    pub fn derived(&self, file: JsonFile) -> &'static [JsonPath] {
        self.derived.get(file)
    }

    /// Model-specific view state, dropped for documents new to a tablet.
    pub fn model_specific(&self, file: JsonFile) -> &'static [JsonPath] {
        self.model_specific.get(file)
    }

    /// The value used for "changed?" comparisons and verification:
    /// `value` without its view-state and derived paths. Everything
    /// else, including unknown fields, is kept.
    pub fn normalise(&self, file: JsonFile, value: &Value) -> Value {
        self.view_state(file).iter().chain(self.derived(file)).fold(
            value.clone(),
            |mut out, path| {
                remove_path(&mut out, path.segments());
                out
            },
        )
    }

    /// The JSON to write on apply. For a document the tablet already
    /// has (`local` = the file re-read while xochitl is stopped), the
    /// tablet keeps its own view state: each view-state path takes
    /// `local`'s value, or is removed when `local` has none; page entries
    /// are matched by `id`, and pages the tablet does not have keep
    /// `target`'s values. For a document new to the tablet (`local` =
    /// `None`), model-specific fields are dropped instead.
    pub fn view_state_for_apply(
        &self,
        file: JsonFile,
        target: &Value,
        local: Option<&Value>,
    ) -> Value {
        match local {
            Some(local) => self
                .view_state(file)
                .iter()
                .fold(target.clone(), |mut out, path| {
                    overlay_path(&mut out, Some(local), path.segments());
                    out
                }),
            None => self
                .model_specific(file)
                .iter()
                .fold(target.clone(), |mut out, path| {
                    remove_path(&mut out, path.segments());
                    out
                }),
        }
    }
}

/// Remove the value at `path` (every array element for [`Seg::Each`]).
fn remove_path(value: &mut Value, path: &[Seg]) {
    match (path, value) {
        ([Seg::Key(key)], Value::Object(map)) => {
            map.remove(*key);
        }
        ([Seg::Key(key), rest @ ..], Value::Object(map)) => {
            if let Some(child) = map.get_mut(*key) {
                remove_path(child, rest);
            }
        }
        ([Seg::Each, rest @ ..], Value::Array(items)) => {
            items.iter_mut().for_each(|item| remove_path(item, rest));
        }
        _ => {}
    }
}

/// Make `target`'s value at `path` equal to `local`'s: copied when
/// `local` has one, removed when it does not. Array elements are
/// matched by `id`; unmatched target elements are left alone.
fn overlay_path(target: &mut Value, local: Option<&Value>, path: &[Seg]) {
    match (path, target) {
        ([Seg::Key(key)], Value::Object(map)) => match local.and_then(|l| l.get(*key)) {
            Some(value) => {
                map.insert((*key).to_string(), value.clone());
            }
            None => {
                map.remove(*key);
            }
        },
        ([Seg::Key(key), rest @ ..], Value::Object(map)) => {
            if let Some(child) = map.get_mut(*key) {
                overlay_path(child, local.and_then(|l| l.get(*key)), rest);
            }
        }
        ([Seg::Each, rest @ ..], Value::Array(items)) => {
            let local_items = local.and_then(Value::as_array);
            items.iter_mut().for_each(|item| {
                let counterpart = item.get("id").cloned().and_then(|id| {
                    local_items?
                        .iter()
                        .find(|candidate| candidate.get("id") == Some(&id))
                });
                if let Some(counterpart) = counterpart {
                    overlay_path(item, Some(counterpart), rest);
                }
            });
        }
        _ => {}
    }
}

/// The v1 rules without PDF payloads, standing in for an older,
/// narrower rule set.
#[cfg(test)]
pub(super) fn rules_without_pdf() -> MirrorRules {
    use super::fixtures::rules;

    static KINDS: [FileKind; 8] = [
        FileKind::Metadata,
        FileKind::Content,
        FileKind::Pagedata,
        FileKind::Template,
        FileKind::PayloadEpub,
        FileKind::Page,
        FileKind::PageMetadata,
        FileKind::Highlights,
    ];
    MirrorRules {
        kinds: &KINDS,
        ..*rules()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::model::fixtures::*;
    use serde_json::json;

    #[test]
    fn rules_versions() {
        assert_eq!(rules().version(), 1);
        assert_eq!(MirrorRules::latest().version(), SUPPORTED_MIRROR_RULES);
        assert!(matches!(
            MirrorRules::for_version(0),
            Err(ModelError::InvalidRules(0))
        ));
        [2, u32::MAX].iter().for_each(|version| {
            assert!(matches!(
                MirrorRules::for_version(*version),
                Err(ModelError::UnsupportedRules { found, supported: 1 }) if found == *version
            ));
        });
        let v1 = rules();
        [
            FileKind::Metadata,
            FileKind::Content,
            FileKind::Pagedata,
            FileKind::Template,
            FileKind::PayloadPdf,
            FileKind::PayloadEpub,
            FileKind::Page,
            FileKind::PageMetadata,
            FileKind::Highlights,
        ]
        .iter()
        .for_each(|kind| assert!(v1.allows(*kind)));
    }

    #[test]
    fn json_path_lists_match_the_design() {
        let names =
            |paths: &[JsonPath]| -> Vec<String> { paths.iter().map(ToString::to_string).collect() };
        let r = rules();
        assert_eq!(
            names(r.view_state(JsonFile::Metadata)),
            [
                "lastOpened",
                "lastOpenedPage",
                "new",
                "metadatamodified",
                "modified",
                "synced",
                "version"
            ]
        );
        assert_eq!(names(r.derived(JsonFile::Metadata)), ["lastModified"]);
        assert_eq!(
            names(r.view_state(JsonFile::Content)),
            [
                "cPages.lastOpened",
                "cPages.pages[*].scrollTime",
                "cPages.pages[*].verticalScroll",
                "customZoomCenterX",
                "customZoomCenterY",
                "customZoomOrientation",
                "customZoomPageHeight",
                "customZoomPageWidth",
                "customZoomScale",
                "zoomMode",
                "extraMetadata",
                "lastOpenedPage",
                "transform",
                "keyboardMetadata"
            ]
        );
        assert_eq!(
            names(r.derived(JsonFile::Content)),
            [
                "pageCount",
                "sizeInBytes",
                "cPages.original",
                "documentMetadata",
                "cPages.uuids"
            ]
        );
        // Model-specific fields are all view state, and all customZoom*.
        let view = names(r.view_state(JsonFile::Content));
        names(r.model_specific(JsonFile::Content))
            .iter()
            .for_each(|path| {
                assert!(path.starts_with("customZoom"));
                assert!(view.contains(path));
            });
        assert_eq!(r.model_specific(JsonFile::Content).len(), 6);
        assert!(r.model_specific(JsonFile::Metadata).is_empty());
    }

    #[test]
    fn normalise_metadata_drops_view_state_and_derived_fields() {
        let normal = rules().normalise(JsonFile::Metadata, &sample_metadata());
        assert_eq!(
            normal,
            json!({
                "createdTime": "1767225600000",
                "parent": "",
                "pinned": false,
                "source": "",
                "type": "DocumentType",
                "visibleName": "Example notebook",
                "futureField": { "kept": true }
            })
        );
        // Older-firmware/rmu folder bookkeeping is view state too.
        let folder = json!({
            "createdTime": 1, "lastModified": 2, "metadatamodified": false,
            "modified": false, "parent": "", "pinned": false, "synced": false,
            "type": "CollectionType", "version": 0, "visibleName": "Books"
        });
        assert_eq!(
            rules().normalise(JsonFile::Metadata, &folder),
            json!({ "createdTime": 1, "parent": "", "pinned": false,
                    "type": "CollectionType", "visibleName": "Books" })
        );
    }

    #[test]
    fn normalise_content_drops_view_state_and_derived_fields() {
        let normal = rules().normalise(JsonFile::Content, &sample_content());
        assert_eq!(
            normal,
            json!({
                "cPages": {
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
                          "redir": { "timestamp": "1:2", "value": 1 } }
                    ],
                    "futureRegister": { "timestamp": "1:3", "value": 1 }
                },
                "coverPageNumber": 0,
                "fileType": "pdf",
                "fontName": "", "lineHeight": -1, "margins": 100,
                "formatVersion": 2,
                "orientation": "portrait",
                "pageTags": [], "tags": [],
                "textAlignment": "left", "textScale": 1
            })
        );
        // Idempotent.
        assert_eq!(rules().normalise(JsonFile::Content, &normal), normal);
    }

    #[test]
    fn reading_is_not_a_change_but_ink_is() {
        let r = rules();
        let base = sample_content();
        let mut read = base.clone();
        read["cPages"]["lastOpened"] = json!({ "timestamp": "1:40", "value": PAGE });
        read["cPages"]["pages"][2]["scrollTime"]["value"] = json!("2026-02-01T00:00:00Z");
        read["cPages"]["pages"][0]["verticalScroll"] = json!({ "timestamp": "1:41", "value": 12 });
        read["extraMetadata"]["LastPen"] = json!("Fineliner");
        read["sizeInBytes"] = json!("2000000");
        read["lastOpenedPage"] = json!(2);
        assert_eq!(
            r.normalise(JsonFile::Content, &read),
            r.normalise(JsonFile::Content, &base)
        );

        let mut inked = base.clone();
        inked["cPages"]["pages"][1]["modifed"] = json!("1767400000000");
        assert_ne!(
            r.normalise(JsonFile::Content, &inked),
            r.normalise(JsonFile::Content, &base)
        );
        let mut tagged = base.clone();
        tagged["tags"] = json!(["work"]);
        assert_ne!(
            r.normalise(JsonFile::Content, &tagged),
            r.normalise(JsonFile::Content, &base)
        );

        let mut opened = sample_metadata();
        opened["lastOpened"] = json!("1767500000000");
        opened["lastModified"] = json!(1767500000000_i64);
        opened["new"] = json!(true);
        assert_eq!(
            r.normalise(JsonFile::Metadata, &opened),
            r.normalise(JsonFile::Metadata, &sample_metadata())
        );
        let mut renamed = sample_metadata();
        renamed["visibleName"] = json!("Renamed");
        assert_ne!(
            r.normalise(JsonFile::Metadata, &renamed),
            r.normalise(JsonFile::Metadata, &sample_metadata())
        );
    }

    #[test]
    fn normalise_tolerates_other_shapes() {
        let r = rules();
        // Folder .content and the old flat page list are left alone.
        assert_eq!(r.normalise(JsonFile::Content, &json!([])), json!([]));
        let old = json!({ "fileType": "", "pages": [PAGE, PAGE2], "pageCount": 2 });
        assert_eq!(
            r.normalise(JsonFile::Content, &old),
            json!({ "fileType": "", "pages": [PAGE, PAGE2] })
        );
        // Unexpected types under known keys do not panic.
        let odd = json!({ "cPages": { "pages": "nope", "lastOpened": 3 }, "extraMetadata": 1 });
        assert_eq!(
            r.normalise(JsonFile::Content, &odd),
            json!({ "cPages": { "pages": "nope" } })
        );
        assert_eq!(r.normalise(JsonFile::Metadata, &json!(null)), json!(null));
    }

    #[test]
    fn overlay_keeps_the_tablets_view_state() {
        let r = rules();
        // Origin's merged version: another tablet read it elsewhere, and
        // a page was inserted.
        let mut target = sample_content();
        target["cPages"]["lastOpened"] = json!({ "timestamp": "1:90", "value": PAGE });
        target["cPages"]["pages"][0]["scrollTime"] =
            json!({ "timestamp": "1:91", "value": "2026-03-01T00:00:00Z" });
        target["cPages"]["pages"][2]["verticalScroll"] =
            json!({ "timestamp": "1:92", "value": 99 });
        target["cPages"]["pages"].as_array_mut().unwrap().push(
            json!({ "id": "8c1f0b9e-0000-4000-8000-0000000000a4",
                          "idx": { "timestamp": "1:93", "value": "bc" },
                          "scrollTime": { "timestamp": "1:94", "value": "2026-03-02T00:00:00Z" } }),
        );
        target["zoomMode"] = json!("fitWidth");
        target["extraMetadata"] = json!({ "LastPen": "Fineliner" });
        target["tags"] = json!(["merged"]);
        // The tablet's own version: no lastOpenedPage, no zoomMode.
        let mut local = sample_content();
        local.as_object_mut().unwrap().remove("zoomMode");
        local["cPages"]["pages"][0]["verticalScroll"] = json!({ "timestamp": "1:20", "value": 5 });

        let applied = r.view_state_for_apply(JsonFile::Content, &target, Some(&local));
        let pages = applied["cPages"]["pages"].as_array().unwrap();
        // Tablet's values win, matched by page id...
        assert_eq!(
            applied["cPages"]["lastOpened"],
            local["cPages"]["lastOpened"]
        );
        assert_eq!(pages[0].get("scrollTime"), None);
        assert_eq!(pages[0]["verticalScroll"]["value"], json!(5));
        assert_eq!(pages[2]["verticalScroll"]["value"], json!(0));
        assert_eq!(
            pages[2]["scrollTime"],
            local["cPages"]["pages"][2]["scrollTime"]
        );
        // ...a page the tablet does not have keeps origin's values...
        assert_eq!(
            pages[3]["scrollTime"]["value"],
            json!("2026-03-02T00:00:00Z")
        );
        // ...view state the tablet lacks is removed...
        assert_eq!(applied.get("zoomMode"), None);
        assert_eq!(applied["extraMetadata"], local["extraMetadata"]);
        assert_eq!(applied["customZoomCenterY"], json!(936));
        // ...and everything else is origin's.
        assert_eq!(applied["tags"], json!(["merged"]));
        assert_eq!(pages.len(), 4);
        assert_eq!(
            r.normalise(JsonFile::Content, &applied),
            r.normalise(JsonFile::Content, &target)
        );
    }

    #[test]
    fn overlay_metadata() {
        let r = rules();
        let mut target = sample_metadata();
        target["visibleName"] = json!("Renamed elsewhere");
        target["lastOpened"] = json!("1767999999999");
        let mut local = sample_metadata();
        local["new"] = json!(true);
        local["lastOpened"] = json!("0");
        local["synced"] = json!(true);
        let applied = r.view_state_for_apply(JsonFile::Metadata, &target, Some(&local));
        assert_eq!(applied["visibleName"], json!("Renamed elsewhere"));
        assert_eq!(applied["new"], json!(true));
        assert_eq!(applied["lastOpened"], json!("0"));
        assert_eq!(applied["synced"], json!(true));
        assert_eq!(applied["lastModified"], target["lastModified"]);
        assert_eq!(applied["futureField"], json!({ "kept": true }));
    }

    #[test]
    fn new_documents_drop_model_specific_fields_only() {
        let r = rules();
        let target = sample_content();
        let applied = r.view_state_for_apply(JsonFile::Content, &target, None);
        let mut expected = target.clone();
        [
            "customZoomCenterX",
            "customZoomCenterY",
            "customZoomOrientation",
            "customZoomPageHeight",
            "customZoomPageWidth",
            "customZoomScale",
        ]
        .iter()
        .for_each(|key| {
            expected.as_object_mut().unwrap().remove(*key);
        });
        assert_eq!(applied, expected);
        assert_eq!(applied["zoomMode"], json!("bestFit"));
        assert_eq!(
            r.view_state_for_apply(JsonFile::Metadata, &sample_metadata(), None),
            sample_metadata()
        );
    }
}
