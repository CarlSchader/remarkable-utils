//! High-level logical operations on reMarkable (xochitl) storage.
//!
//! Mirrors the semantics of xochitl's own storage model: every write
//! operation only touches files under the xochitl data directory, and
//! callers should restart xochitl afterwards ([`Client::restart_xochitl`])
//! so the device UI reflects the changes.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::bundle;
use crate::epub;
use crate::error::{Error, Result};
use crate::progress::{NoProgress, Progress};
use crate::ssh::{SshSession, shell_quote};
use crate::xochitl::{self, Item, ItemKind};

/// High-level client for one device.
pub struct Client {
    session: SshSession,
    dir: String,
    progress: Arc<dyn Progress>,
}

impl Client {
    pub fn new(session: SshSession, xochitl_dir: impl Into<String>) -> Self {
        let dir = xochitl_dir.into().trim_end_matches('/').to_string();
        Self {
            session,
            dir,
            progress: Arc::new(NoProgress),
        }
    }

    /// Attach a progress observer (rendered by the frontend; the
    /// library itself never prints).
    pub fn with_progress(mut self, progress: Arc<dyn Progress>) -> Self {
        self.progress = progress;
        self
    }

    /// Load the logical item list in a single SSH round trip.
    ///
    /// One remote script dumps every `.metadata`/`.content` file plus
    /// payload sizes, delimited by a per-call random marker.
    pub fn list_items(&self) -> Result<Vec<Item>> {
        self.progress.step("Reading document index");
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
        let stdout_bytes = self.session.run_checked_bytes(&script, &*self.progress)?;
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let items = build_items(&parse_listing(&marker, &stdout));
        self.progress.finished();
        Ok(items)
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

        let items = self.list_items()?;
        let parent = xochitl::resolve_folder_ref(&items, parent_ref)?;

        // Fold each path segment into (known items, current parent),
        // reusing existing folders and creating missing ones.
        let (_, _, current) = parts.iter().try_fold(
            (items, parent, None::<Item>),
            |(mut items, parent, _), part| {
                if let Some(existing) = xochitl::find_child(&items, &parent, part, true)?.cloned() {
                    let next_parent = existing.uuid.clone();
                    return Ok((items, next_parent, Some(existing)));
                }
                xochitl::ensure_no_conflict(&items, &parent, part, None)?;
                self.progress.step(&format!("Creating folder '{part}'"));
                let created = self.create_folder_in(part, &parent)?;
                let next_parent = created.uuid.clone();
                items.push(created.clone());
                Ok::<_, Error>((items, next_parent, Some(created)))
            },
        )?;
        self.progress.finished();
        Ok(current.expect("at least one path segment"))
    }

    /// Upload a document into a logical folder. Accepted inputs:
    /// - `.pdf` / `.epub`: uploaded as-is.
    /// - `.rmdoc`: bundle restore, re-targeted to a fresh UUID so
    ///   re-importing a download never collides with the original.
    /// - `.md` / `.markdown` / `.txt`: converted to EPUB on the host
    ///   (the device cannot render text files — see
    ///   `docs/text-import.md`).
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
        match extension.as_str() {
            "pdf" | "epub" => self.upload_payload(local, parent_ref, visible_name, &extension),
            "rmdoc" => self.upload_rmdoc(local, parent_ref, visible_name),
            "md" | "markdown" => {
                self.upload_text(local, parent_ref, visible_name, epub::TextKind::Markdown)
            }
            "txt" => self.upload_text(local, parent_ref, visible_name, epub::TextKind::Plain),
            _ => Err(Error::UnsupportedFileType(extension)),
        }
    }

    /// Upload a bare `.pdf`/`.epub` payload, generating fresh metadata.
    fn upload_payload(
        &self,
        local: &Path,
        parent_ref: &str,
        visible_name: Option<&str>,
        file_type: &str,
    ) -> Result<Item> {
        let items = self.list_items()?;
        let parent = xochitl::resolve_folder_ref(&items, parent_ref)?;
        let name = default_name(visible_name, local);
        xochitl::ensure_no_conflict(&items, &parent, &name, None)?;
        self.store_payload(local, &parent, &name, file_type)
    }

    /// Store a `.pdf`/`.epub` payload under `parent_uuid` **without
    /// conflict checks** — callers (e.g. the sync planner) must have
    /// verified the name is free.
    pub fn store_payload(
        &self,
        local: &Path,
        parent_uuid: &str,
        name: &str,
        file_type: &str,
    ) -> Result<Item> {
        let doc_uuid = uuid::Uuid::new_v4().to_string();
        self.progress.step(&format!(
            "Uploading {}",
            local.file_name().unwrap_or_default().to_string_lossy()
        ));
        self.session.upload_local_file(
            local,
            &self.remote_path(&format!("{doc_uuid}.{file_type}")),
            &*self.progress,
        )?;
        let size = std::fs::metadata(local).ok().map(|m| m.len());
        self.register_document(
            doc_uuid,
            name.to_string(),
            parent_uuid.to_string(),
            file_type,
            size,
        )
    }

    /// Convert a `.md`/`.txt` file to EPUB and upload it. The document
    /// lands on the device as a regular EPUB; conversion is one-way.
    fn upload_text(
        &self,
        local: &Path,
        parent_ref: &str,
        visible_name: Option<&str>,
        kind: epub::TextKind,
    ) -> Result<Item> {
        let items = self.list_items()?;
        let parent = xochitl::resolve_folder_ref(&items, parent_ref)?;
        let name = default_name(visible_name, local);
        xochitl::ensure_no_conflict(&items, &parent, &name, None)?;
        self.store_text(local, &parent, &name, kind)
    }

    /// Convert and store a text file as EPUB under `parent_uuid`
    /// **without conflict checks**.
    pub fn store_text(
        &self,
        local: &Path,
        parent_uuid: &str,
        name: &str,
        kind: epub::TextKind,
    ) -> Result<Item> {
        self.progress.step("Converting to EPUB");
        let source = std::fs::read_to_string(local)?;
        let epub_bytes = epub::text_to_epub(name, kind, &source)?;

        let doc_uuid = uuid::Uuid::new_v4().to_string();
        self.progress.step(&format!(
            "Uploading {}",
            local.file_name().unwrap_or_default().to_string_lossy()
        ));
        self.session.run_checked_with_stdin(
            &format!(
                "cat > {}",
                shell_quote(&self.remote_path(&format!("{doc_uuid}.epub")))
            ),
            &epub_bytes,
            &*self.progress,
        )?;
        let size = Some(epub_bytes.len() as u64);
        self.register_document(
            doc_uuid,
            name.to_string(),
            parent_uuid.to_string(),
            "epub",
            size,
        )
    }

    /// Write the metadata/content/pagedata files that make an uploaded
    /// payload a real document, and report the resulting item.
    fn register_document(
        &self,
        doc_uuid: String,
        name: String,
        parent: String,
        file_type: &str,
        size_bytes: Option<u64>,
    ) -> Result<Item> {
        let now = now_ms();
        self.progress.step("Registering document");
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
        self.progress.finished();

        Ok(Item {
            uuid: doc_uuid,
            visible_name: name,
            parent,
            kind: ItemKind::Document,
            file_type: Some(file_type.to_string()),
            created_time: now,
            last_modified: now,
            size_bytes,
        })
    }

    /// Restore an `.rmdoc` bundle: parse locally, rewrite
    /// `parent`/`visibleName`/`lastModified` (preserving all other
    /// metadata fields), re-target every file to a fresh UUID, and
    /// stream one tar into the data dir.
    fn upload_rmdoc(
        &self,
        local: &Path,
        parent_ref: &str,
        visible_name: Option<&str>,
    ) -> Result<Item> {
        let rmdoc = bundle::parse_rmdoc(&std::fs::read(local)?)?;
        let items = self.list_items()?;
        let parent = xochitl::resolve_folder_ref(&items, parent_ref)?;
        let name = visible_name
            .map(str::to_string)
            .or_else(|| rmdoc.visible_name().map(str::to_string))
            .unwrap_or_else(|| default_name(None, local));
        xochitl::ensure_no_conflict(&items, &parent, &name, None)?;
        self.restore_bundle(rmdoc, &parent, &name)
    }

    /// Restore a parsed `.rmdoc` bundle under `parent_uuid` **without
    /// conflict checks**, re-targeted to a fresh UUID.
    pub fn restore_bundle(
        &self,
        mut rmdoc: bundle::Rmdoc,
        parent_uuid: &str,
        name: &str,
    ) -> Result<Item> {
        if rmdoc.metadata.get("type").and_then(Value::as_str) != Some("DocumentType") {
            return Err(Error::Bundle(
                "bundle metadata is not a DocumentType item".to_string(),
            ));
        }
        let new_uuid = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let object = rmdoc
            .metadata
            .as_object_mut()
            .ok_or_else(|| Error::InvalidMetadata(rmdoc.uuid.clone()))?;
        object.insert("parent".to_string(), Value::String(parent_uuid.to_string()));
        object.insert("visibleName".to_string(), Value::String(name.to_string()));
        object.insert("lastModified".to_string(), Value::String(now.to_string()));

        let tar_bytes = bundle::rmdoc_to_tar(&rmdoc, &new_uuid)?;
        self.progress.step(&format!("Restoring bundle '{name}'"));
        self.session.run_checked_with_stdin(
            &format!("cd {} && tar -xf -", shell_quote(&self.dir)),
            &tar_bytes,
            &*self.progress,
        )?;
        self.progress.finished();

        xochitl::item_from_metadata(&new_uuid, &rmdoc.metadata, rmdoc.content.as_ref(), None)
            .ok_or(Error::InvalidMetadata(new_uuid))
    }

    /// Overwrite an existing document's payload file from a local file
    /// and bump `lastModified`. Preserves annotations and tree
    /// location. Returns the new `lastModified`.
    pub fn update_payload_from_file(
        &self,
        uuid: &str,
        file_type: &str,
        local: &Path,
    ) -> Result<i64> {
        self.progress.step(&format!(
            "Updating {}",
            local.file_name().unwrap_or_default().to_string_lossy()
        ));
        self.session.upload_local_file(
            local,
            &self.remote_path(&format!("{uuid}.{file_type}")),
            &*self.progress,
        )?;
        self.touch_last_modified(uuid)
    }

    /// Overwrite an existing document's payload with in-memory bytes
    /// (e.g. a regenerated EPUB) and bump `lastModified`.
    pub fn update_payload_bytes(&self, uuid: &str, file_type: &str, data: &[u8]) -> Result<i64> {
        self.progress.step("Updating document");
        self.session.run_checked_with_stdin(
            &format!(
                "cat > {}",
                shell_quote(&self.remote_path(&format!("{uuid}.{file_type}")))
            ),
            data,
            &*self.progress,
        )?;
        self.touch_last_modified(uuid)
    }

    fn touch_last_modified(&self, uuid: &str) -> Result<i64> {
        let now = now_ms();
        self.update_metadata(uuid, |metadata| {
            metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
        })?;
        self.progress.finished();
        Ok(now)
    }

    /// Download a document payload by UUID (no path resolution).
    pub fn download_payload_to(
        &self,
        uuid: &str,
        file_type: &str,
        dest: &Path,
        size_hint: Option<u64>,
    ) -> Result<()> {
        self.session.download_remote_file(
            &self.remote_path(&format!("{uuid}.{file_type}")),
            dest,
            size_hint,
            &*self.progress,
        )
    }

    /// Download a document as an `.rmdoc` bundle by UUID.
    pub fn download_bundle_to(&self, uuid: &str, dest: &Path) -> Result<()> {
        let tar_bytes = self.fetch_item_tar(uuid)?;
        std::fs::write(dest, bundle::tar_to_rmdoc(&tar_bytes)?)?;
        Ok(())
    }

    /// Download a document. `output` may be a file path or an existing
    /// directory; defaults to `<name>.<ext>` in the current directory.
    ///
    /// Native notebooks have no payload file (their content is per-page
    /// `.rm` data — see `docs/notebook-data.md`), so they always
    /// download as an `.rmdoc` bundle: a zip of the raw xochitl file
    /// set, the same layout the official apps export. Pass `bundle` to
    /// force this for PDFs/EPUBs too, which captures annotations that
    /// the bare payload lacks.
    pub fn download(&self, item_ref: &str, output: Option<&Path>, bundle: bool) -> Result<PathBuf> {
        let items = self.list_items()?;
        let item = xochitl::resolve_item_ref(&items, item_ref)?;
        if !item.is_document() {
            return Err(Error::NotADocument(item_ref.to_string()));
        }

        let is_notebook = matches!(item.file_type.as_deref(), None | Some("notebook"));
        if bundle || is_notebook {
            let destination = resolve_destination(output, &format!("{}.rmdoc", item.visible_name));
            self.progress
                .step(&format!("Downloading '{}'", item.visible_name));
            let tar_bytes = self.fetch_item_tar(&item.uuid)?;
            self.progress.step("Packing .rmdoc");
            std::fs::write(&destination, bundle::tar_to_rmdoc(&tar_bytes)?)?;
            self.progress.finished();
            return Ok(destination);
        }

        let extension = item.file_type.as_deref().expect("payload types are Some");
        let destination =
            resolve_destination(output, &format!("{}.{extension}", item.visible_name));
        self.progress
            .step(&format!("Downloading '{}'", item.visible_name));
        self.session.download_remote_file(
            &self.remote_path(&format!("{}.{extension}", item.uuid)),
            &destination,
            item.size_bytes,
            &*self.progress,
        )?;
        self.progress.finished();
        Ok(destination)
    }

    /// Stream every artifact of an item (`<uuid>`, `<uuid>.*`) from the
    /// device as one tar. The `.*` glob is deliberately outside the
    /// quotes; `[ -e ]` filters unmatched glob literals, and busybox
    /// tar would otherwise fail on missing arguments.
    fn fetch_item_tar(&self, uuid: &str) -> Result<Vec<u8>> {
        let quoted = shell_quote(uuid);
        let script = format!(
            "cd {dir} || exit 9\n\
             set --\n\
             for f in {quoted} {quoted}.*; do [ -e \"$f\" ] && set -- \"$@\" \"$f\"; done\n\
             [ \"$#\" -gt 0 ] || exit 8\n\
             tar -cf - \"$@\"\n",
            dir = shell_quote(&self.dir),
        );
        self.session.run_checked_bytes(&script, &*self.progress)
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

        order.iter().try_for_each(|target| {
            self.progress
                .step(&format!("Deleting '{}'", target.visible_name));
            self.delete_artifacts(&target.uuid)
        })?;
        self.progress.finished();
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
        self.progress.step("Updating metadata");
        self.update_metadata(&item.uuid, |metadata| {
            metadata.insert("parent".to_string(), Value::String(destination.clone()));
            metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
        })?;
        self.progress.finished();
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
        self.progress.step("Updating metadata");
        self.update_metadata(&item.uuid, |metadata| {
            metadata.insert("visibleName".to_string(), Value::String(name.to_string()));
            metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
        })?;
        self.progress.finished();
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
        self.progress.step("Restarting xochitl");
        let _ = self.session.run("systemctl reset-failed xochitl.service");
        let restart = self.session.run("systemctl restart xochitl.service")?;
        if restart.status.success() {
            self.progress.finished();
            return Ok(());
        }
        // Device-side restarts can transiently drop the command channel
        // even though xochitl comes back; verify before failing.
        thread::sleep(Duration::from_secs(1));
        let status = self.session.run("systemctl is-active xochitl.service")?;
        if status.status.success() && String::from_utf8_lossy(&status.stdout).trim() == "active" {
            self.progress.finished();
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

    /// Create a folder under `parent_uuid` **without conflict checks**
    /// — callers must have verified the name is free.
    pub fn create_folder_in(&self, visible_name: &str, parent_uuid: &str) -> Result<Item> {
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

    /// Delete one document's artifacts by UUID, with **no tree
    /// logic** — callers (e.g. the sync executor) must not use this on
    /// folders with children. Use [`Self::delete`] for reference-based
    /// deletion.
    pub fn delete_document(&self, uuid: &str) -> Result<()> {
        self.delete_artifacts(uuid)
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

/// Visible name for an upload: explicit override or the file stem.
fn default_name(visible_name: Option<&str>, local: &Path) -> String {
    visible_name.map(str::to_string).unwrap_or_else(|| {
        local
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_string())
    })
}

/// Default to `filename` in the current directory; an existing local
/// directory means "put it in there".
fn resolve_destination(output: Option<&Path>, filename: &str) -> PathBuf {
    match output {
        None => PathBuf::from(filename),
        Some(path) if path.is_dir() => path.join(filename),
        Some(path) => path.to_path_buf(),
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
    // Group lines into (section name, body) pairs, then fold the
    // sections into the listing maps.
    let sections = output
        .lines()
        .fold(Vec::<(String, String)>::new(), |mut sections, line| {
            match line
                .strip_prefix(marker)
                .and_then(|rest| rest.strip_prefix(' '))
            {
                Some(name) => sections.push((name.trim().to_string(), String::new())),
                None => {
                    if let Some((_, body)) = sections.last_mut() {
                        body.push_str(line);
                        body.push('\n');
                    }
                }
            }
            sections
        });

    sections
        .into_iter()
        .fold(Listing::default(), |mut listing, (name, body)| {
            if name == "__sizes__" {
                listing.sizes.extend(
                    body.lines()
                        .filter_map(|line| line.split_once(' '))
                        .filter_map(|(size, filename)| {
                            size.trim()
                                .parse::<u64>()
                                .ok()
                                .map(|size| (filename.to_string(), size))
                        }),
                );
            } else if let Some(uuid) = name.strip_suffix(".metadata") {
                listing.metadata.insert(uuid.to_string(), body);
            } else if let Some(uuid) = name.strip_suffix(".content") {
                listing.content.insert(uuid.to_string(), body);
            }
            listing
        })
}

fn build_items(listing: &Listing) -> Vec<Item> {
    let mut uuids: Vec<&String> = listing.metadata.keys().collect();
    uuids.sort();

    // Sequential on purpose: parsing 10k metadata docs measures ~7 ms,
    // dwarfed by the SSH round trip that fetched them. Revisit with
    // rayon only if a measured workload says otherwise.
    uuids
        .into_iter()
        .filter_map(|uuid| {
            // Skip unparsable metadata (partial writes, corruption)
            // rather than failing the whole listing.
            let metadata = serde_json::from_str::<Value>(&listing.metadata[uuid]).ok()?;
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
            xochitl::item_from_metadata(uuid, &metadata, content.as_ref(), size)
        })
        .collect()
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
