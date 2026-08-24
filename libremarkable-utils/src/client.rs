//! High-level logical operations on reMarkable (xochitl) storage.
//!
//! Mirrors the semantics of xochitl's own storage model: every write
//! operation only touches files under the xochitl data directory, and
//! callers should restart xochitl afterwards ([`Client::restart_xochitl`])
//! so the device UI reflects the changes.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::ssh::{SshSession, shell_quote};
use crate::xochitl::{self, Item, ItemKind};

/// High-level client for one device.
pub struct Client {
    session: SshSession,
    dir: String,
}

impl Client {
    pub fn new(session: SshSession, xochitl_dir: impl Into<String>) -> Self {
        let dir = xochitl_dir.into().trim_end_matches('/').to_string();
        Self { session, dir }
    }

    /// Load the logical item list in a single SSH round trip.
    ///
    /// One remote script dumps every `.metadata`/`.content` file plus
    /// payload sizes, delimited by a per-call random marker.
    pub fn list_items(&self) -> Result<Vec<Item>> {
        let marker = format!("===RMU:{}===", uuid::Uuid::new_v4().simple());
        let script = format!(
            "cd {dir} || exit 9\n\
             for f in *.metadata *.content; do\n\
             [ -f \"$f\" ] || continue\n\
             printf '\\n%s %s\\n' {marker} \"$f\"\n\
             cat \"$f\"\n\
             done\n\
             printf '\\n%s __sizes__\\n' {marker}\n\
             for f in *.pdf *.epub; do\n\
             [ -f \"$f\" ] || continue\n\
             printf '%s %s\\n' \"$(wc -c < \"$f\")\" \"$f\"\n\
             done\n",
            dir = shell_quote(&self.dir),
            marker = shell_quote(&marker),
        );
        let stdout = self.session.run_checked(&script)?;
        Ok(build_items(&parse_listing(&marker, &stdout)))
    }

    /// Create a nested folder path (like `mkdir -p`), reusing existing
    /// folders. Returns the deepest folder.
    pub fn mkdir_path(&self, folder_path: &str, parent_ref: &str) -> Result<Item> {
        let parts: Vec<&str> = folder_path
            .split('/')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(Error::EmptyPath);
        }

        let mut items = self.list_items()?;
        let mut parent = xochitl::resolve_folder_ref(&items, parent_ref)?;
        let mut current: Option<Item> = None;

        for part in parts {
            if let Some(existing) = xochitl::find_child(&items, &parent, part, true)?.cloned() {
                parent = existing.uuid.clone();
                current = Some(existing);
                continue;
            }
            xochitl::ensure_no_conflict(&items, &parent, part, None)?;
            let created = self.create_folder(part, &parent)?;
            parent = created.uuid.clone();
            items.push(created.clone());
            current = Some(created);
        }
        Ok(current.expect("at least one path segment"))
    }

    /// Upload a `.pdf` or `.epub` into a logical folder.
    pub fn upload(
        &self,
        local: &Path,
        parent_ref: &str,
        visible_name: Option<&str>,
    ) -> Result<Item> {
        if !local.is_file() {
            return Err(Error::FileNotFound(local.to_path_buf()));
        }
        let extension = local
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let file_type = match extension.as_str() {
            "pdf" => "pdf",
            "epub" => "epub",
            _ => return Err(Error::UnsupportedFileType(extension)),
        };

        let items = self.list_items()?;
        let parent = xochitl::resolve_folder_ref(&items, parent_ref)?;
        let name = match visible_name {
            Some(name) => name.to_string(),
            None => local
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "untitled".to_string()),
        };
        xochitl::ensure_no_conflict(&items, &parent, &name, None)?;

        let doc_uuid = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        self.session
            .upload_local_file(local, &self.remote_path(&format!("{doc_uuid}.{file_type}")))?;
        self.write_text(
            &format!("{doc_uuid}.metadata"),
            &xochitl::document_metadata_json(&name, &parent, now),
        )?;
        self.write_text(
            &format!("{doc_uuid}.content"),
            &xochitl::document_content_json(file_type),
        )?;
        self.session.run_checked(&format!(
            "mkdir -p {}",
            shell_quote(&self.remote_path(&doc_uuid))
        ))?;
        self.write_text(&format!("{doc_uuid}.pagedata"), "")?;

        Ok(Item {
            uuid: doc_uuid,
            visible_name: name,
            parent,
            kind: ItemKind::Document,
            file_type: Some(file_type.to_string()),
            created_time: now,
            last_modified: now,
            size_bytes: std::fs::metadata(local).ok().map(|m| m.len()),
        })
    }

    /// Download a document. `output` may be a file path or an existing
    /// directory; defaults to `<name>.<ext>` in the current directory.
    pub fn download(&self, item_ref: &str, output: Option<&Path>) -> Result<PathBuf> {
        let items = self.list_items()?;
        let item = xochitl::resolve_item_ref(&items, item_ref)?;
        if !item.is_document() {
            return Err(Error::NotADocument(item_ref.to_string()));
        }
        let extension = item.file_type.as_deref().unwrap_or("bin");
        let filename = format!("{}.{extension}", item.visible_name);
        let destination = match output {
            None => PathBuf::from(&filename),
            Some(path) if path.is_dir() => path.join(&filename),
            Some(path) => path.to_path_buf(),
        };
        self.session.download_remote_file(
            &self.remote_path(&format!("{}.{extension}", item.uuid)),
            &destination,
        )?;
        Ok(destination)
    }

    /// Delete a document or folder. Deleting a non-empty folder
    /// requires `recursive`; children are removed before parents.
    pub fn delete(&self, item_ref: &str, recursive: bool) -> Result<Vec<Item>> {
        let items = self.list_items()?;
        let item = xochitl::resolve_item_ref(&items, item_ref)?.clone();
        let mut order: Vec<Item> = xochitl::descendants(&items, &item.uuid)
            .into_iter()
            .cloned()
            .collect();
        if item.is_folder() && !order.is_empty() && !recursive {
            return Err(Error::FolderNotEmpty);
        }
        order.sort_by_key(|descendant| Reverse(xochitl::depth(&items, descendant)));
        order.push(item);

        for target in &order {
            self.delete_artifacts(&target.uuid)?;
        }
        Ok(order)
    }

    /// Move an item into another folder (root allowed).
    pub fn move_item(&self, item_ref: &str, destination_ref: &str) -> Result<Item> {
        let items = self.list_items()?;
        let item = xochitl::resolve_item_ref(&items, item_ref)?.clone();
        let destination = xochitl::resolve_folder_ref(&items, destination_ref)?;

        if item.uuid == destination {
            return Err(Error::MoveIntoSelf);
        }
        if item.is_folder() && xochitl::is_descendant(&items, &destination, &item.uuid) {
            return Err(Error::MoveIntoDescendant);
        }
        if item.parent == destination {
            return Ok(item);
        }
        xochitl::ensure_no_conflict(&items, &destination, &item.visible_name, Some(&item.uuid))?;

        let now = now_ms();
        self.update_metadata(&item.uuid, |metadata| {
            metadata.insert("parent".to_string(), Value::String(destination.clone()));
            metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
        })?;
        Ok(Item {
            parent: destination,
            last_modified: now,
            ..item
        })
    }

    /// Rename a document or folder.
    pub fn rename(&self, item_ref: &str, new_name: &str) -> Result<Item> {
        let name = new_name.trim();
        if name.is_empty() {
            return Err(Error::InvalidName("name cannot be empty".to_string()));
        }
        if name.contains('/') {
            return Err(Error::InvalidName("name cannot contain '/'".to_string()));
        }

        let items = self.list_items()?;
        let item = xochitl::resolve_item_ref(&items, item_ref)?.clone();
        if item.visible_name == name {
            return Ok(item);
        }
        xochitl::ensure_no_conflict(&items, &item.parent, name, Some(&item.uuid))?;

        let now = now_ms();
        self.update_metadata(&item.uuid, |metadata| {
            metadata.insert("visibleName".to_string(), Value::String(name.to_string()));
            metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
        })?;
        Ok(Item {
            visible_name: name.to_string(),
            last_modified: now,
            ..item
        })
    }

    /// Restart xochitl so the device UI reflects filesystem changes.
    ///
    /// xochitl ships with a strict systemd start limit: after a few
    /// restarts in a short window, plain `restart` can hit
    /// start-limit-hit and trigger the device emergency target, which
    /// reboots the whole tablet. Reset the unit's failure state first
    /// so maintenance restarts do not accumulate against that counter.
    pub fn restart_xochitl(&self) -> Result<()> {
        let _ = self.session.run("systemctl reset-failed xochitl.service");
        let restart = self.session.run("systemctl restart xochitl.service")?;
        if restart.status.success() {
            return Ok(());
        }
        // Device-side restarts can transiently drop the command channel
        // even though xochitl comes back; verify before failing.
        thread::sleep(Duration::from_secs(1));
        let status = self.session.run("systemctl is-active xochitl.service")?;
        if status.status.success() && String::from_utf8_lossy(&status.stdout).trim() == "active" {
            return Ok(());
        }
        Err(Error::XochitlRestart(format!(
            "restart exit={:?} stderr='{}'; is-active exit={:?} stdout='{}'",
            restart.status.code(),
            String::from_utf8_lossy(&restart.stderr).trim(),
            status.status.code(),
            String::from_utf8_lossy(&status.stdout).trim(),
        )))
    }

    fn remote_path(&self, name: &str) -> String {
        format!("{}/{}", self.dir, name)
    }

    fn write_text(&self, name: &str, text: &str) -> Result<()> {
        self.session
            .write_remote_file(&self.remote_path(name), text.as_bytes())
    }

    fn read_json(&self, name: &str) -> Result<Value> {
        let text = self
            .session
            .run_checked(&format!("cat {}", shell_quote(&self.remote_path(name))))?;
        serde_json::from_str(&text).map_err(|source| Error::Json {
            path: name.to_string(),
            source,
        })
    }

    /// Read-modify-write a `.metadata` file, preserving unknown fields.
    fn update_metadata(
        &self,
        uuid: &str,
        mutate: impl FnOnce(&mut serde_json::Map<String, Value>),
    ) -> Result<()> {
        let name = format!("{uuid}.metadata");
        let mut metadata = self.read_json(&name)?;
        let object = metadata
            .as_object_mut()
            .ok_or_else(|| Error::InvalidMetadata(uuid.to_string()))?;
        mutate(object);
        let text = serde_json::to_string_pretty(&metadata).map_err(|source| Error::Json {
            path: name.clone(),
            source,
        })?;
        self.write_text(&name, &text)
    }

    fn create_folder(&self, visible_name: &str, parent_uuid: &str) -> Result<Item> {
        let folder_uuid = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        self.write_text(
            &format!("{folder_uuid}.metadata"),
            &xochitl::folder_metadata_json(visible_name, parent_uuid, now),
        )?;
        self.write_text(&format!("{folder_uuid}.content"), "[]")?;
        self.write_text(&format!("{folder_uuid}.pagedata"), "\n")?;
        Ok(Item {
            uuid: folder_uuid,
            visible_name: visible_name.to_string(),
            parent: parent_uuid.to_string(),
            kind: ItemKind::Folder,
            file_type: None,
            created_time: now,
            last_modified: now,
            size_bytes: None,
        })
    }

    /// Remove `<uuid>`, `<uuid>.*` from the data dir. The `.*` glob is
    /// deliberately outside the quotes; `rm -f` is silent when the
    /// glob matches nothing.
    fn delete_artifacts(&self, uuid: &str) -> Result<()> {
        let quoted = shell_quote(uuid);
        self.session.run_checked(&format!(
            "cd {} && rm -rf -- {quoted} {quoted}.*",
            shell_quote(&self.dir)
        ))?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Sections of the batched listing output, keyed by item UUID
/// (metadata/content) or payload filename (sizes).
#[derive(Debug, Default)]
struct Listing {
    metadata: HashMap<String, String>,
    content: HashMap<String, String>,
    sizes: HashMap<String, u64>,
}

fn parse_listing(marker: &str, output: &str) -> Listing {
    fn flush(listing: &mut Listing, section: Option<&str>, body: &str) {
        let Some(name) = section else { return };
        if name == "__sizes__" {
            for line in body.lines() {
                if let Some((size, filename)) = line.split_once(' ')
                    && let Ok(size) = size.trim().parse::<u64>()
                {
                    listing.sizes.insert(filename.to_string(), size);
                }
            }
        } else if let Some(uuid) = name.strip_suffix(".metadata") {
            listing.metadata.insert(uuid.to_string(), body.to_string());
        } else if let Some(uuid) = name.strip_suffix(".content") {
            listing.content.insert(uuid.to_string(), body.to_string());
        }
    }

    let mut listing = Listing::default();
    let mut section: Option<String> = None;
    let mut body = String::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(marker)
            && let Some(name) = rest.strip_prefix(' ')
        {
            flush(&mut listing, section.as_deref(), &body);
            section = Some(name.trim().to_string());
            body.clear();
            continue;
        }
        if section.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&mut listing, section.as_deref(), &body);
    listing
}

fn build_items(listing: &Listing) -> Vec<Item> {
    let mut uuids: Vec<&String> = listing.metadata.keys().collect();
    uuids.sort();

    let mut items = Vec::new();
    for uuid in uuids {
        // Skip unparsable metadata (partial writes, corruption) rather
        // than failing the whole listing.
        let Ok(metadata) = serde_json::from_str::<Value>(&listing.metadata[uuid]) else {
            continue;
        };
        let content = listing
            .content
            .get(uuid)
            .and_then(|body| serde_json::from_str::<Value>(body).ok());
        let size = content
            .as_ref()
            .and_then(|c| c.get("fileType"))
            .and_then(Value::as_str)
            .filter(|file_type| !file_type.is_empty())
            .and_then(|file_type| listing.sizes.get(&format!("{uuid}.{file_type}")))
            .copied();
        if let Some(item) = xochitl::item_from_metadata(uuid, &metadata, content.as_ref(), size) {
            items.push(item);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_parse_and_build() {
        let marker = "===RMU:test===";
        let output = "\n\
===RMU:test=== aaa.metadata\n\
{\"type\": \"DocumentType\", \"visibleName\": \"Doc\", \"parent\": \"\", \"lastModified\": \"5\"}\n\
\n\
===RMU:test=== aaa.content\n\
{\"fileType\": \"pdf\"}\n\
\n\
===RMU:test=== bbb.metadata\n\
{\"type\": \"CollectionType\", \"visibleName\": \"Dir\", \"parent\": \"\"}\n\
\n\
===RMU:test=== ccc.metadata\n\
not json at all\n\
\n\
===RMU:test=== __sizes__\n\
2048 aaa.pdf\n";

        let listing = parse_listing(marker, output);
        assert_eq!(listing.metadata.len(), 3);
        assert_eq!(listing.content.len(), 1);
        assert_eq!(listing.sizes.get("aaa.pdf"), Some(&2048));

        let items = build_items(&listing);
        assert_eq!(items.len(), 2);
        let doc = items.iter().find(|item| item.uuid == "aaa").unwrap();
        assert_eq!(doc.visible_name, "Doc");
        assert_eq!(doc.file_type.as_deref(), Some("pdf"));
        assert_eq!(doc.size_bytes, Some(2048));
        assert_eq!(doc.last_modified, 5);
        let dir = items.iter().find(|item| item.uuid == "bbb").unwrap();
        assert!(dir.is_folder());
    }

    #[test]
    fn listing_ignores_noise_outside_sections() {
        let marker = "===M===";
        let output =
            "motd noise\n===M=== x.metadata\n{\"type\":\"CollectionType\",\"visibleName\":\"A\"}\n";
        let listing = parse_listing(marker, output);
        assert_eq!(listing.metadata.len(), 1);
        assert!(listing.metadata["x"].contains("CollectionType"));
    }
}
