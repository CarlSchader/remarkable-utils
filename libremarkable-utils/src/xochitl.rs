//! On-device xochitl file formats and pure logical-tree operations.
//!
//! Everything in this module is pure (no I/O), which keeps path
//! resolution, conflict detection, and tree rendering unit-testable
//! without a device.
//!
//! ## Storage model
//!
//! xochitl keeps a flat directory of files per item UUID:
//! - `<uuid>.metadata` — JSON: `visibleName`, `parent` (UUID of the
//!   containing folder, `""` for root, `"trash"` for trashed items),
//!   and `type` (`DocumentType` or `CollectionType`).
//! - `<uuid>.content` — JSON: `fileType` (`pdf`/`epub`/... ) and layout
//!   settings.
//! - `<uuid>.<fileType>` — the document payload.
//! - `<uuid>/` — per-page annotation data.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};

/// Path on the tablet where xochitl stores documents.
pub const XOCHITL_DATA_DIR: &str = "/home/root/.local/share/remarkable/xochitl";

/// Pseudo-parent UUID of items in the device's trash (deleted via the
/// UI, restorable until the trash is emptied).
pub const TRASH_PARENT: &str = "trash";

/// Kind of a logical item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Folder,
    Document,
}

/// A logical item (folder or document) reconstructed from metadata.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub uuid: String,
    pub visible_name: String,
    /// UUID of the parent folder; `""` means root.
    pub parent: String,
    pub kind: ItemKind,
    /// Document type: `pdf`/`epub` (payload file exists on-device) or
    /// `notebook` (native handwritten; no payload file, see
    /// `docs/notebook-data.md`). `None` when `.content` is missing.
    pub file_type: Option<String>,
    /// Milliseconds since the epoch; `0` when absent/unparsable.
    pub created_time: i64,
    pub last_modified: i64,
    /// Payload size for documents, when it could be determined.
    pub size_bytes: Option<u64>,
}

impl Item {
    pub fn is_folder(&self) -> bool {
        self.kind == ItemKind::Folder
    }

    pub fn is_document(&self) -> bool {
        self.kind == ItemKind::Document
    }
}

/// Build an [`Item`] from parsed `.metadata` (and optional `.content`)
/// JSON. Returns `None` for unknown item types or non-object metadata.
pub fn item_from_metadata(
    uuid: &str,
    metadata: &Value,
    content: Option<&Value>,
    size_bytes: Option<u64>,
) -> Option<Item> {
    let kind = match metadata.get("type")?.as_str()? {
        "DocumentType" => ItemKind::Document,
        "CollectionType" => ItemKind::Folder,
        _ => return None,
    };
    let visible_name = metadata
        .get("visibleName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(uuid)
        .to_string();
    let parent = metadata
        .get("parent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let file_type = match kind {
        ItemKind::Document => {
            let declared = content
                .and_then(|c| c.get("fileType"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            match declared {
                Some(file_type) => Some(file_type),
                // Native notebooks have fileType "notebook" on current
                // firmware but "" on older firmware; normalize so
                // callers have one case to handle. Only infer when a
                // .content file actually exists.
                None if content.is_some() => Some("notebook".to_string()),
                None => None,
            }
        }
        ItemKind::Folder => None,
    };
    Some(Item {
        uuid: uuid.to_string(),
        visible_name,
        parent,
        kind,
        file_type,
        created_time: lenient_i64(metadata.get("createdTime")),
        last_modified: lenient_i64(metadata.get("lastModified")),
        size_bytes: match kind {
            ItemKind::Document => size_bytes,
            ItemKind::Folder => None,
        },
    })
}

/// xochitl stores timestamps sometimes as strings, sometimes as numbers.
fn lenient_i64(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// `.metadata` JSON for a new document.
pub fn document_metadata_json(visible_name: &str, parent_uuid: &str, now_ms: i64) -> String {
    let now = now_ms.to_string();
    serde_json::to_string_pretty(&json!({
        "createdTime": now,
        "lastModified": now,
        "lastOpened": "0",
        "lastOpenedPage": 0,
        "new": true,
        "parent": parent_uuid,
        "pinned": false,
        "source": "",
        "type": "DocumentType",
        "visibleName": visible_name,
    }))
    .expect("static JSON serializes")
}

/// `.metadata` JSON for a new folder.
pub fn folder_metadata_json(visible_name: &str, parent_uuid: &str, now_ms: i64) -> String {
    let now = now_ms.to_string();
    serde_json::to_string_pretty(&json!({
        "createdTime": now,
        "lastModified": now,
        "metadatamodified": false,
        "modified": false,
        "parent": parent_uuid,
        "pinned": false,
        "synced": false,
        "type": "CollectionType",
        "version": 0,
        "visibleName": visible_name,
    }))
    .expect("static JSON serializes")
}

/// `.content` JSON for a new document.
pub fn document_content_json(file_type: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "cPages": {
            "original": {
                "timestamp": "1:0",
                "value": -1,
            },
            "pages": [],
        },
        "coverPageNumber": 0,
        "documentMetadata": {},
        "extraMetadata": {},
        "fileType": file_type,
        "fontName": "",
        "formatVersion": 2,
        "lineHeight": -1,
        "margins": 100,
        "orientation": "portrait",
        "pageCount": 0,
        "pageTags": [],
        "sizeInBytes": "0",
        "textAlignment": "left",
        "textScale": 1,
    }))
    .expect("static JSON serializes")
}

fn sort_key(item: &Item) -> (u8, String, &str) {
    (
        if item.is_folder() { 0 } else { 1 },
        item.visible_name.to_lowercase(),
        item.uuid.as_str(),
    )
}

/// Map of parent UUID -> sorted children (folders first, then name).
pub fn children_map(items: &[Item], folders_only: bool) -> HashMap<&str, Vec<&Item>> {
    let mut map = items
        .iter()
        .filter(|item| !folders_only || item.is_folder())
        .fold(HashMap::<&str, Vec<&Item>>::new(), |mut map, item| {
            map.entry(item.parent.as_str()).or_default().push(item);
            map
        });
    map.values_mut()
        .for_each(|children| children.sort_by(|a, b| sort_key(a).cmp(&sort_key(b))));
    map
}

/// Resolve a reference (UUID or logical path like `Books/Math`) to an
/// item. Ambiguous paths are rejected instead of guessed.
pub fn resolve_item_ref<'a>(items: &'a [Item], item_ref: &str) -> Result<&'a Item> {
    if item_ref.is_empty() || item_ref == "/" || item_ref == "(root)" {
        return Err(Error::RootTarget);
    }
    if let Some(found) = items.iter().find(|item| item.uuid == item_ref) {
        return Ok(found);
    }
    let path = item_ref.trim_matches('/');
    if path.is_empty() {
        return Err(Error::RootTarget);
    }

    path.split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .try_fold(None::<&Item>, |current, part| {
            let parent = current.map_or("", |item| item.uuid.as_str());
            let mut matches = items
                .iter()
                .filter(|item| item.parent == parent && item.visible_name == part);
            match (matches.next(), matches.next()) {
                (Some(only), None) => Ok(Some(only)),
                (Some(_), Some(_)) => Err(Error::AmbiguousPath {
                    segment: part.to_string(),
                    path: item_ref.to_string(),
                }),
                (None, _) => Err(Error::PathNotFound(item_ref.to_string())),
            }
        })?
        .ok_or_else(|| Error::PathNotFound(item_ref.to_string()))
}

/// Resolve a folder reference to its UUID. `""`, `/`, and `(root)` all
/// mean the root pseudo-folder (returned as `""`).
pub fn resolve_folder_ref(items: &[Item], folder_ref: &str) -> Result<String> {
    if folder_ref.is_empty() || folder_ref == "/" || folder_ref == "(root)" {
        return Ok(String::new());
    }
    let item = resolve_item_ref(items, folder_ref)?;
    if !item.is_folder() {
        return Err(Error::NotAFolder(folder_ref.to_string()));
    }
    Ok(item.uuid.clone())
}

/// Whether a reference contains glob metacharacters (`*`, `?`, `[`).
pub fn is_glob(reference: &str) -> bool {
    reference.contains(['*', '?', '['])
}

/// Match one glob segment (no `/`) against one path segment.
/// Supports `*`, `?`, and character classes `[...]` / `[!...]` with
/// ranges; an unterminated `[` matches a literal `[` (shell rule).
fn segment_match(pattern: &[char], text: &[char]) -> bool {
    /// The parsed class: pattern remainder, negation, member ranges.
    type Class<'a> = (&'a [char], bool, Vec<(char, char)>);

    /// Parse a class at `pattern[0] == '['`; `None` when unterminated.
    fn parse_class(pattern: &[char]) -> Option<Class<'_>> {
        let (negated, mut i) = if pattern.first() == Some(&'!') {
            (true, 1)
        } else {
            (false, 0)
        };
        let mut ranges = Vec::new();
        // A `]` in first position is a literal member.
        let mut first = true;
        while let Some(&c) = pattern.get(i) {
            if c == ']' && !first {
                return Some((&pattern[i + 1..], negated, ranges));
            }
            first = false;
            if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|&e| e != ']') {
                ranges.push((c, pattern[i + 2]));
                i += 3;
            } else {
                ranges.push((c, c));
                i += 1;
            }
        }
        None
    }

    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => (0..=text.len()).any(|skip| segment_match(rest, &text[skip..])),
        Some(('?', rest)) => text
            .split_first()
            .is_some_and(|(_, text_rest)| segment_match(rest, text_rest)),
        Some(('[', class_rest)) => match parse_class(class_rest) {
            Some((rest, negated, ranges)) => text.split_first().is_some_and(|(&c, text_rest)| {
                let member = ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi);
                member != negated && segment_match(rest, text_rest)
            }),
            // Unterminated class: literal '['.
            None => text
                .split_first()
                .is_some_and(|(&c, text_rest)| c == '[' && segment_match(class_rest, text_rest)),
        },
        Some((&expected, rest)) => text
            .split_first()
            .is_some_and(|(&c, text_rest)| c == expected && segment_match(rest, text_rest)),
    }
}

/// Match glob pattern segments against path segments. `*`/`?`/classes
/// never cross a `/`. A bare `**` segment in the middle matches any
/// number of segments (including none, so `a/**/b` matches `a/b`); a
/// *trailing* `**` matches everything **inside** a folder but not the
/// folder itself (gitignore semantics — `rm 'Books/**'` empties Books
/// without deleting it).
fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", [])) => !path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => path.split_first().is_some_and(|(segment, path_rest)| {
            let pattern_chars: Vec<char> = first.chars().collect();
            let segment_chars: Vec<char> = segment.chars().collect();
            segment_match(&pattern_chars, &segment_chars) && match_segments(rest, path_rest)
        }),
    }
}

/// Split a reference/pattern into normalized path segments (leading
/// and trailing `/` and empty segments dropped).
fn path_segments(reference: &str) -> Vec<&str> {
    reference
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// All items **reachable from the root** whose logical path matches
/// the glob pattern (trash and orphan items never match, same as
/// exact path resolution). Duplicate-named siblings that both match
/// are both returned — a pattern means "everything that matches",
/// unlike an exact path, where ambiguity is rejected. Errors when
/// nothing matches.
pub fn resolve_glob<'a>(items: &'a [Item], pattern: &str) -> Result<Vec<&'a Item>> {
    let pattern_segments = path_segments(pattern);
    if pattern_segments.is_empty() {
        return Err(Error::RootTarget);
    }

    fn walk<'a>(
        children: &HashMap<&str, Vec<&'a Item>>,
        parent: &str,
        prefix: &mut Vec<&'a str>,
        pattern: &[&str],
        matched: &mut Vec<&'a Item>,
    ) {
        children
            .get(parent)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .for_each(|item| {
                prefix.push(item.visible_name.as_str());
                if match_segments(pattern, prefix) {
                    matched.push(item);
                }
                if item.is_folder() {
                    walk(children, &item.uuid, prefix, pattern, matched);
                }
                prefix.pop();
            });
    }

    let mut matched = Vec::new();
    walk(
        &children_map(items, false),
        "",
        &mut Vec::new(),
        &pattern_segments,
        &mut matched,
    );
    if matched.is_empty() {
        return Err(Error::PathNotFound(pattern.to_string()));
    }
    Ok(matched)
}

/// Expand a mixed list of exact references and glob patterns into
/// UUIDs against one listing. Exact resolution is tried first, so an
/// item whose name literally contains glob metacharacters stays
/// addressable; only when that fails and the reference contains
/// metacharacters is it treated as a pattern.
pub fn expand_refs(items: &[Item], item_refs: &[&str]) -> Result<Vec<String>> {
    item_refs.iter().try_fold(Vec::new(), |mut out, reference| {
        match resolve_item_ref(items, reference) {
            Ok(item) => out.push(item.uuid.clone()),
            Err(exact_err) => {
                if !is_glob(reference) {
                    return Err(exact_err);
                }
                out.extend(
                    resolve_glob(items, reference)?
                        .iter()
                        .map(|item| item.uuid.clone()),
                );
            }
        }
        Ok(out)
    })
}

/// Validate moving `uuids` into `destination_uuid`, returning the
/// items that actually need moving (already-in-place ones drop out).
/// Everything is checked before anything is written: self-moves,
/// folder cycles, and name conflicts in the destination — including
/// conflicts *among* the moved set itself.
pub fn plan_moves<'a>(
    items: &'a [Item],
    uuids: &[String],
    destination_uuid: &str,
) -> Result<Vec<&'a Item>> {
    let moving: Vec<&Item> = uuids
        .iter()
        .map(|uuid| {
            items
                .iter()
                .find(|item| &item.uuid == uuid)
                .ok_or_else(|| Error::PathNotFound(uuid.clone()))
        })
        .collect::<Result<_>>()?;
    let needed: Vec<&Item> = moving
        .into_iter()
        .filter(|item| item.parent != destination_uuid)
        .collect();

    needed.iter().try_fold(
        HashSet::<&str>::new(),
        |mut names, item| -> Result<HashSet<&str>> {
            if item.uuid == destination_uuid {
                return Err(Error::MoveIntoSelf);
            }
            if item.is_folder() && is_descendant(items, destination_uuid, &item.uuid) {
                return Err(Error::MoveIntoDescendant);
            }
            ensure_no_conflict(
                items,
                destination_uuid,
                &item.visible_name,
                Some(&item.uuid),
            )?;
            if !names.insert(item.visible_name.as_str()) {
                return Err(Error::NameConflict {
                    name: item.visible_name.clone(),
                    parent: display_parent(destination_uuid).to_string(),
                });
            }
            Ok(names)
        },
    )?;
    Ok(needed)
}

/// Find a uniquely-named child of `parent_uuid`, if any.
pub fn find_child<'a>(
    items: &'a [Item],
    parent_uuid: &str,
    name: &str,
    folders_only: bool,
) -> Result<Option<&'a Item>> {
    let mut matches = items.iter().filter(|item| {
        item.parent == parent_uuid
            && item.visible_name == name
            && (!folders_only || item.is_folder())
    });
    match (matches.next(), matches.next()) {
        (first, None) => Ok(first),
        _ => Err(Error::AmbiguousPath {
            segment: name.to_string(),
            path: display_parent(parent_uuid).to_string(),
        }),
    }
}

/// Error if `parent_uuid` already contains an item named `name`
/// (other than `ignore_uuid`).
pub fn ensure_no_conflict(
    items: &[Item],
    parent_uuid: &str,
    name: &str,
    ignore_uuid: Option<&str>,
) -> Result<()> {
    let conflict = items.iter().any(|item| {
        item.parent == parent_uuid
            && item.visible_name == name
            && Some(item.uuid.as_str()) != ignore_uuid
    });
    if conflict {
        return Err(Error::NameConflict {
            name: name.to_string(),
            parent: display_parent(parent_uuid).to_string(),
        });
    }
    Ok(())
}

/// All transitive descendants of `parent_uuid`.
pub fn descendants<'a>(items: &'a [Item], parent_uuid: &str) -> Vec<&'a Item> {
    fn walk<'a>(children: &HashMap<&str, Vec<&'a Item>>, parent: &str) -> Vec<&'a Item> {
        children
            .get(parent)
            .into_iter()
            .flatten()
            .flat_map(|item| std::iter::once(*item).chain(walk(children, &item.uuid)))
            .collect()
    }
    walk(&children_map(items, false), parent_uuid)
}

/// Whether `candidate_uuid` is `ancestor_uuid` or inside it.
pub fn is_descendant(items: &[Item], candidate_uuid: &str, ancestor_uuid: &str) -> bool {
    ancestor_chain(items, candidate_uuid).any(|uuid| uuid == ancestor_uuid)
}

/// Number of ancestors between `item` and the root.
pub fn depth(items: &[Item], item: &Item) -> usize {
    ancestor_chain(items, &item.parent).count()
}

/// Walk a UUID's parent chain toward the root: the UUID itself, its
/// parent, and so on. Stops at root/unknown parents; the length cap
/// guards against parent cycles in corrupt metadata.
fn ancestor_chain<'a>(items: &'a [Item], start_uuid: &'a str) -> impl Iterator<Item = &'a str> {
    let parents: HashMap<&str, &str> = items
        .iter()
        .map(|item| (item.uuid.as_str(), item.parent.as_str()))
        .collect();
    std::iter::successors(Some(start_uuid), move |current| {
        parents.get(*current).copied()
    })
    .take(items.len() + 1)
    .take_while(|current| !current.is_empty())
}

/// Absolute logical path of an item, e.g. `/Books/Math/Notes`.
pub fn build_path(items: &[Item], item: &Item) -> String {
    let by_uuid: HashMap<&str, &Item> = items
        .iter()
        .map(|item| (item.uuid.as_str(), item))
        .collect();
    let parts: Vec<&str> = std::iter::successors(Some(item), |current| {
        by_uuid.get(current.parent.as_str()).copied()
    })
    .take(items.len() + 1)
    .map(|item| item.visible_name.as_str())
    .collect();
    format!(
        "/{}",
        parts.iter().rev().copied().collect::<Vec<_>>().join("/")
    )
}

/// Render the logical tree as text lines.
///
/// Items whose parent UUID does not exist (e.g. `trash`) are listed in
/// a trailing "Orphan items" section.
pub fn render_tree(items: &[Item], show_uuid: bool, folders_only: bool) -> Vec<String> {
    fn label(item: &Item, show_uuid: bool) -> String {
        let suffix = if item.is_folder() {
            "/".to_string()
        } else {
            item.file_type
                .as_deref()
                .map(|t| format!(" ({t})"))
                .unwrap_or_default()
        };
        let uuid_part = if show_uuid {
            format!(" [{}]", item.uuid)
        } else {
            String::new()
        };
        format!("{}{}{}", item.visible_name, suffix, uuid_part)
    }

    fn walk(
        parent: &str,
        prefix: &str,
        children: &HashMap<&str, Vec<&Item>>,
        show_uuid: bool,
    ) -> Vec<String> {
        let kids = children.get(parent).map(Vec::as_slice).unwrap_or_default();
        kids.iter()
            .enumerate()
            .flat_map(|(index, child)| {
                let last = index == kids.len() - 1;
                let branch = if last { "└─ " } else { "├─ " };
                let line = format!("{prefix}{branch}{}", label(child, show_uuid));
                let subtree = if child.is_folder() {
                    let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
                    walk(&child.uuid, &child_prefix, children, show_uuid)
                } else {
                    Vec::new()
                };
                std::iter::once(line).chain(subtree)
            })
            .collect()
    }

    let children = children_map(items, folders_only);
    let known: HashSet<&str> = items.iter().map(|item| item.uuid.as_str()).collect();

    let mut orphan_parents: Vec<&str> = children
        .keys()
        .copied()
        .filter(|parent| !parent.is_empty() && !known.contains(parent))
        .collect();
    orphan_parents.sort_unstable();
    let orphan_section: Vec<String> = if orphan_parents.is_empty() {
        Vec::new()
    } else {
        [String::new(), "Orphan items:".to_string()]
            .into_iter()
            .chain(
                orphan_parents
                    .iter()
                    .flat_map(|parent| walk(parent, "", &children, show_uuid)),
            )
            .collect()
    };

    std::iter::once("(root)".to_string())
        .chain(walk("", "", &children, show_uuid))
        .chain(orphan_section)
        .collect()
}

fn display_parent(parent_uuid: &str) -> &str {
    if parent_uuid.is_empty() {
        "(root)"
    } else {
        parent_uuid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(uuid: &str, name: &str, parent: &str) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind: ItemKind::Folder,
            file_type: None,
            created_time: 0,
            last_modified: 0,
            size_bytes: None,
        }
    }

    fn doc(uuid: &str, name: &str, parent: &str, file_type: &str) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind: ItemKind::Document,
            file_type: Some(file_type.to_string()),
            created_time: 0,
            last_modified: 0,
            size_bytes: Some(1024),
        }
    }

    fn sample() -> Vec<Item> {
        vec![
            folder("b", "Books", ""),
            folder("m", "Math", "b"),
            doc("la", "Linear Algebra", "m", "pdf"),
            doc("ph", "Physics", "b", "epub"),
            folder("n", "Notes", ""),
        ]
    }

    #[test]
    fn resolve_by_path_and_uuid() {
        let items = sample();
        assert_eq!(resolve_item_ref(&items, "Books/Math").unwrap().uuid, "m");
        assert_eq!(
            resolve_item_ref(&items, "/Books/Math/Linear Algebra/")
                .unwrap()
                .uuid,
            "la"
        );
        assert_eq!(
            resolve_item_ref(&items, "ph").unwrap().visible_name,
            "Physics"
        );
    }

    #[test]
    fn resolve_missing_and_root() {
        let items = sample();
        assert!(matches!(
            resolve_item_ref(&items, "Books/Nope"),
            Err(Error::PathNotFound(_))
        ));
        assert!(matches!(
            resolve_item_ref(&items, "/"),
            Err(Error::RootTarget)
        ));
        assert!(matches!(
            resolve_item_ref(&items, ""),
            Err(Error::RootTarget)
        ));
    }

    #[test]
    fn resolve_rejects_ambiguity() {
        let mut items = sample();
        items.push(doc("ph2", "Physics", "b", "pdf"));
        assert!(matches!(
            resolve_item_ref(&items, "Books/Physics"),
            Err(Error::AmbiguousPath { .. })
        ));
    }

    #[test]
    fn folder_ref_root_and_type_check() {
        let items = sample();
        assert_eq!(resolve_folder_ref(&items, "").unwrap(), "");
        assert_eq!(resolve_folder_ref(&items, "/").unwrap(), "");
        assert_eq!(resolve_folder_ref(&items, "(root)").unwrap(), "");
        assert_eq!(resolve_folder_ref(&items, "Books/Math").unwrap(), "m");
        assert!(matches!(
            resolve_folder_ref(&items, "Books/Physics"),
            Err(Error::NotAFolder(_))
        ));
    }

    #[test]
    fn conflict_detection() {
        let items = sample();
        assert!(ensure_no_conflict(&items, "b", "Chemistry", None).is_ok());
        assert!(matches!(
            ensure_no_conflict(&items, "b", "Physics", None),
            Err(Error::NameConflict { .. })
        ));
        // Renaming an item to its own name is not a conflict.
        assert!(ensure_no_conflict(&items, "b", "Physics", Some("ph")).is_ok());
    }

    #[test]
    fn descendants_and_depth() {
        let items = sample();
        let mut uuids: Vec<&str> = descendants(&items, "b")
            .iter()
            .map(|item| item.uuid.as_str())
            .collect();
        uuids.sort_unstable();
        assert_eq!(uuids, ["la", "m", "ph"]);
        let la = resolve_item_ref(&items, "la").unwrap();
        assert_eq!(depth(&items, la), 2);
        assert!(is_descendant(&items, "la", "b"));
        assert!(is_descendant(&items, "b", "b"));
        assert!(!is_descendant(&items, "n", "b"));
    }

    #[test]
    fn logical_paths() {
        let items = sample();
        let la = resolve_item_ref(&items, "la").unwrap();
        assert_eq!(build_path(&items, la), "/Books/Math/Linear Algebra");
    }

    #[test]
    fn tree_rendering() {
        let items = sample();
        let expected = vec![
            "(root)",
            "├─ Books/",
            "│  ├─ Math/",
            "│  │  └─ Linear Algebra (pdf)",
            "│  └─ Physics (epub)",
            "└─ Notes/",
        ];
        assert_eq!(render_tree(&items, false, false), expected);
    }

    #[test]
    fn tree_orphans() {
        let mut items = sample();
        items.push(doc("tr", "Old Notebook", "trash", "pdf"));
        let lines = render_tree(&items, false, false);
        assert!(lines.contains(&"Orphan items:".to_string()));
        assert!(lines.contains(&"└─ Old Notebook (pdf)".to_string()));
    }

    #[test]
    fn metadata_round_trip() {
        let metadata: Value =
            serde_json::from_str(&document_metadata_json("Doc", "b", 1234)).unwrap();
        let content: Value = serde_json::from_str(&document_content_json("pdf")).unwrap();
        let item = item_from_metadata("u1", &metadata, Some(&content), Some(10)).unwrap();
        assert_eq!(item.visible_name, "Doc");
        assert_eq!(item.parent, "b");
        assert_eq!(item.kind, ItemKind::Document);
        assert_eq!(item.file_type.as_deref(), Some("pdf"));
        assert_eq!(item.created_time, 1234);
        assert_eq!(item.size_bytes, Some(10));

        let folder_md: Value = serde_json::from_str(&folder_metadata_json("Dir", "", 99)).unwrap();
        let folder = item_from_metadata("u2", &folder_md, None, None).unwrap();
        assert_eq!(folder.kind, ItemKind::Folder);
        assert_eq!(folder.parent, "");
    }

    #[test]
    fn metadata_lenient_parsing() {
        // Numeric timestamps and unknown types.
        let metadata = serde_json::json!({
            "type": "DocumentType",
            "visibleName": "X",
            "parent": "trash",
            "createdTime": 42,
            "lastModified": "notanumber",
        });
        let item = item_from_metadata("u", &metadata, None, None).unwrap();
        assert_eq!(item.created_time, 42);
        assert_eq!(item.last_modified, 0);
        assert_eq!(item.parent, "trash");

        let deleted = serde_json::json!({ "type": "TrashType", "visibleName": "X" });
        assert!(item_from_metadata("u", &deleted, None, None).is_none());
    }

    #[test]
    fn notebook_file_type_inference() {
        let metadata = serde_json::json!({ "type": "DocumentType", "visibleName": "N" });

        // Current firmware: explicit "notebook".
        let content = serde_json::json!({ "fileType": "notebook" });
        let item = item_from_metadata("u", &metadata, Some(&content), None).unwrap();
        assert_eq!(item.file_type.as_deref(), Some("notebook"));

        // Older firmware: empty fileType means native notebook.
        let content = serde_json::json!({ "fileType": "" });
        let item = item_from_metadata("u", &metadata, Some(&content), None).unwrap();
        assert_eq!(item.file_type.as_deref(), Some("notebook"));

        // Missing .content entirely: do not guess.
        let item = item_from_metadata("u", &metadata, None, None).unwrap();
        assert_eq!(item.file_type, None);
    }

    // ---- globs -------------------------------------------------------------

    fn glob(pattern: &str, path: &str) -> bool {
        let pattern_segments: Vec<&str> = path_segments(pattern);
        let segments: Vec<&str> = path_segments(path);
        match_segments(&pattern_segments, &segments)
    }

    #[test]
    fn glob_matching_rules() {
        // `*` and `?` within a segment.
        assert!(glob("math-*", "math-books-vol-1"));
        assert!(glob("*.pdf", "a.pdf"));
        assert!(glob("vol-?", "vol-1"));
        assert!(!glob("vol-?", "vol-10"));
        assert!(glob("*", "anything"));

        // `*` never crosses `/`.
        assert!(!glob("*", "Books/Math"));
        assert!(glob("Books/*", "Books/Math"));
        assert!(!glob("Books/*", "Books/Math/Deep"));

        // `**` crosses segment boundaries. Trailing `**` = everything
        // *inside* the folder, not the folder itself (gitignore rule);
        // mid-pattern `**` matches zero or more segments.
        assert!(glob("Books/**", "Books/Math"));
        assert!(glob("Books/**", "Books/Math/Deep"));
        assert!(!glob("Books/**", "Books"));
        assert!(glob("**/Deep", "Books/Math/Deep"));
        assert!(glob("Books/**/Deep", "Books/Deep"));

        // Character classes: sets, ranges, negation, literal `]`.
        assert!(glob("vol-[12]", "vol-1"));
        assert!(!glob("vol-[12]", "vol-3"));
        assert!(glob("vol-[0-9]", "vol-7"));
        assert!(glob("vol-[!0-9]", "vol-x"));
        assert!(!glob("vol-[!0-9]", "vol-7"));
        assert!(glob("a[]]b", "a]b"));

        // Unterminated `[` is a literal (shell rule).
        assert!(glob("a[b", "a[b"));

        // Case-sensitive.
        assert!(!glob("books/*", "Books/Math"));
    }

    #[test]
    fn glob_resolution_scopes_and_errors() {
        let items = sample();
        let names = |matched: Vec<&Item>| -> Vec<String> {
            matched.iter().map(|i| i.visible_name.clone()).collect()
        };

        // Documents and folders both match.
        assert_eq!(
            names(resolve_glob(&items, "Books/*").unwrap()),
            ["Math", "Physics"]
        );
        // `**` recurses.
        assert_eq!(
            names(resolve_glob(&items, "Books/**").unwrap()),
            ["Math", "Linear Algebra", "Physics"]
        );
        // Leading slash is fine.
        assert_eq!(names(resolve_glob(&items, "/N*").unwrap()), ["Notes"]);
        // No matches: error, never an empty no-op.
        assert!(matches!(
            resolve_glob(&items, "Books/z*"),
            Err(Error::PathNotFound(_))
        ));
        // Bare `/` is not a target.
        assert!(matches!(resolve_glob(&items, "/"), Err(Error::RootTarget)));

        // Trash and orphan items never match.
        let mut with_trash = sample();
        with_trash.push(doc("t", "Trashed", "trash", "pdf"));
        assert!(matches!(
            resolve_glob(&with_trash, "Trash*"),
            Err(Error::PathNotFound(_))
        ));

        // Duplicate-named siblings both match a pattern (a pattern
        // means "everything that matches", unlike an exact path).
        let mut dupes = sample();
        dupes.push(doc("ph2", "Physics", "b", "pdf"));
        assert_eq!(resolve_glob(&dupes, "Books/Phys*").unwrap().len(), 2);
    }

    #[test]
    fn expand_refs_prefers_exact_matches() {
        // An item literally named with a metacharacter is addressed
        // exactly, not treated as a pattern.
        let mut items = sample();
        items.push(doc("star", "vol-*", "b", "pdf"));
        items.push(doc("v1", "vol-1", "b", "pdf"));
        assert_eq!(
            expand_refs(&items, &["Books/vol-*"]).unwrap(),
            ["star".to_string()]
        );

        // Without an exact match the pattern expands.
        let items = {
            let mut items = sample();
            items.push(doc("v1", "vol-1", "b", "pdf"));
            items.push(doc("v2", "vol-2", "b", "pdf"));
            items
        };
        assert_eq!(
            expand_refs(&items, &["Books/vol-*"]).unwrap(),
            ["v1".to_string(), "v2".to_string()]
        );

        // Mixed exact and glob refs; non-glob misses stay hard errors.
        assert_eq!(
            expand_refs(&items, &["ph", "Books/vol-*"]).unwrap().len(),
            3
        );
        assert!(matches!(
            expand_refs(&items, &["Books/Nope"]),
            Err(Error::PathNotFound(_))
        ));
    }

    #[test]
    fn plan_moves_validates_everything_up_front() {
        let items = sample();

        // Plain move.
        let plan = plan_moves(&items, &["la".to_string()], "b").unwrap();
        assert_eq!(plan[0].uuid, "la");

        // Already in place: drops out.
        assert!(
            plan_moves(&items, &["la".to_string()], "m")
                .unwrap()
                .is_empty()
        );

        // Folder cycle.
        assert!(matches!(
            plan_moves(&items, &["b".to_string()], "m"),
            Err(Error::MoveIntoDescendant)
        ));

        // Name conflict with an existing destination child.
        let mut items2 = sample();
        items2.push(doc("ph2", "Physics", "n", "pdf"));
        assert!(matches!(
            plan_moves(&items2, &["ph2".to_string()], "b"),
            Err(Error::NameConflict { .. })
        ));

        // Name conflict *within* the moved set.
        let mut items3 = sample();
        items3.push(doc("x1", "Same", "b", "pdf"));
        items3.push(doc("x2", "Same", "m", "pdf"));
        assert!(matches!(
            plan_moves(&items3, &["x1".to_string(), "x2".to_string()], "n"),
            Err(Error::NameConflict { .. })
        ));
    }
}
