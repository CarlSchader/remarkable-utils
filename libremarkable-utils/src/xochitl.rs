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
    /// Payload type for documents (`pdf`/`epub`), if known.
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
        ItemKind::Document => content
            .and_then(|c| c.get("fileType"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
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
    let mut map: HashMap<&str, Vec<&Item>> = HashMap::new();
    for item in items {
        if folders_only && !item.is_folder() {
            continue;
        }
        map.entry(item.parent.as_str()).or_default().push(item);
    }
    for children in map.values_mut() {
        children.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    }
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

    let mut parent = "";
    let mut current: Option<&Item> = None;
    for part in path.split('/').map(str::trim).filter(|p| !p.is_empty()) {
        let matches: Vec<&Item> = items
            .iter()
            .filter(|item| item.parent == parent && item.visible_name == part)
            .collect();
        match matches.as_slice() {
            [] => return Err(Error::PathNotFound(item_ref.to_string())),
            [only] => {
                parent = only.uuid.as_str();
                current = Some(only);
            }
            _ => {
                return Err(Error::AmbiguousPath {
                    segment: part.to_string(),
                    path: item_ref.to_string(),
                });
            }
        }
    }
    current.ok_or_else(|| Error::PathNotFound(item_ref.to_string()))
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

/// Find a uniquely-named child of `parent_uuid`, if any.
pub fn find_child<'a>(
    items: &'a [Item],
    parent_uuid: &str,
    name: &str,
    folders_only: bool,
) -> Result<Option<&'a Item>> {
    let matches: Vec<&Item> = items
        .iter()
        .filter(|item| {
            item.parent == parent_uuid
                && item.visible_name == name
                && (!folders_only || item.is_folder())
        })
        .collect();
    if matches.len() > 1 {
        return Err(Error::AmbiguousPath {
            segment: name.to_string(),
            path: display_parent(parent_uuid).to_string(),
        });
    }
    Ok(matches.first().copied())
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
    let children = children_map(items, false);
    let mut out = Vec::new();
    let mut stack: Vec<&Item> = children.get(parent_uuid).cloned().unwrap_or_default();
    while let Some(item) = stack.pop() {
        out.push(item);
        if item.is_folder()
            && let Some(kids) = children.get(item.uuid.as_str())
        {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// Whether `candidate_uuid` is `ancestor_uuid` or inside it.
pub fn is_descendant(items: &[Item], candidate_uuid: &str, ancestor_uuid: &str) -> bool {
    let parents: HashMap<&str, &str> = items
        .iter()
        .map(|item| (item.uuid.as_str(), item.parent.as_str()))
        .collect();
    let mut current = candidate_uuid;
    // Hop cap guards against parent cycles in corrupt metadata.
    for _ in 0..=items.len() {
        if current.is_empty() {
            return false;
        }
        if current == ancestor_uuid {
            return true;
        }
        current = parents.get(current).copied().unwrap_or("");
    }
    false
}

/// Number of ancestors between `item` and the root.
pub fn depth(items: &[Item], item: &Item) -> usize {
    let parents: HashMap<&str, &str> = items
        .iter()
        .map(|item| (item.uuid.as_str(), item.parent.as_str()))
        .collect();
    let mut depth = 0;
    let mut current = item.parent.as_str();
    while !current.is_empty() && depth <= items.len() {
        depth += 1;
        current = parents.get(current).copied().unwrap_or("");
    }
    depth
}

/// Absolute logical path of an item, e.g. `/Books/Math/Notes`.
pub fn build_path(items: &[Item], item: &Item) -> String {
    let by_uuid: HashMap<&str, &Item> = items
        .iter()
        .map(|item| (item.uuid.as_str(), item))
        .collect();
    let mut parts = vec![item.visible_name.as_str()];
    let mut current = item.parent.as_str();
    for _ in 0..=items.len() {
        if current.is_empty() {
            break;
        }
        match by_uuid.get(current) {
            Some(parent) => {
                parts.push(parent.visible_name.as_str());
                current = parent.parent.as_str();
            }
            None => break,
        }
    }
    parts.reverse();
    format!("/{}", parts.join("/"))
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
        lines: &mut Vec<String>,
    ) {
        let Some(kids) = children.get(parent) else {
            return;
        };
        for (index, child) in kids.iter().enumerate() {
            let last = index == kids.len() - 1;
            let branch = if last { "└─ " } else { "├─ " };
            lines.push(format!("{prefix}{branch}{}", label(child, show_uuid)));
            if child.is_folder() {
                let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
                walk(&child.uuid, &child_prefix, children, show_uuid, lines);
            }
        }
    }

    let children = children_map(items, folders_only);
    let known: HashSet<&str> = items.iter().map(|item| item.uuid.as_str()).collect();
    let mut lines = vec!["(root)".to_string()];
    walk("", "", &children, show_uuid, &mut lines);

    let mut orphan_parents: Vec<&str> = children
        .keys()
        .copied()
        .filter(|parent| !parent.is_empty() && !known.contains(parent))
        .collect();
    orphan_parents.sort_unstable();
    if !orphan_parents.is_empty() {
        lines.push(String::new());
        lines.push("Orphan items:".to_string());
        for parent in orphan_parents {
            walk(parent, "", &children, show_uuid, &mut lines);
        }
    }
    lines
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
}
