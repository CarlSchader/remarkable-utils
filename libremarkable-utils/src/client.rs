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
use crate::error::{Error, Result};
use crate::progress::{NoProgress, Progress};
use crate::ssh::{SshSession, shell_quote};
use crate::status::{self, SystemStatus};
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

    /// [`Self::store_payload`] from in-memory bytes (for generic fs
    /// endpoints that cannot hand over a local path).
    pub fn store_payload_bytes(
        &self,
        data: &[u8],
        parent_uuid: &str,
        name: &str,
        file_type: &str,
    ) -> Result<Item> {
        let doc_uuid = uuid::Uuid::new_v4().to_string();
        self.progress.step(&format!("Uploading '{name}'"));
        self.session.run_checked_with_stdin(
            &format!(
                "cat > {}",
                shell_quote(&self.remote_path(&format!("{doc_uuid}.{file_type}")))
            ),
            data,
            &*self.progress,
        )?;
        self.register_document(
            doc_uuid,
            name.to_string(),
            parent_uuid.to_string(),
            file_type,
            Some(data.len() as u64),
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
        // .metadata is written LAST: a document only becomes visible to
        // xochitl once it exists, so an interrupted upload leaves
        // invisible orphan files instead of a broken document.
        self.write_text(
            &format!("{doc_uuid}.content"),
            &xochitl::document_content_json(file_type),
        )?;
        self.session.run_checked(&format!(
            "mkdir -p {}",
            shell_quote(&self.remote_path(&doc_uuid))
        ))?;
        self.write_text(&format!("{doc_uuid}.pagedata"), "")?;
        self.write_text(
            &format!("{doc_uuid}.metadata"),
            &xochitl::document_metadata_json(&name, &parent, now),
        )?;
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

    /// Relocate/rename a document by UUID **without tree checks** —
    /// callers (the sync executor) must have validated the destination
    /// is a real folder and the name is free. Metadata-only: zero
    /// bytes transferred, annotations preserved. Returns the new
    /// `lastModified`.
    pub fn move_document(&self, uuid: &str, parent_uuid: &str, name: &str) -> Result<i64> {
        let now = now_ms();
        self.progress
            .step(&format!("Moving '{name}' (metadata only)"));
        self.update_metadata(uuid, |metadata| {
            metadata.insert("parent".to_string(), Value::String(parent_uuid.to_string()));
            metadata.insert("visibleName".to_string(), Value::String(name.to_string()));
            metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
        })?;
        self.progress.finished();
        Ok(now)
    }

    /// Create a new document by copying an existing payload **on the
    /// device** (the bytes are already there; nothing is uploaded).
    /// No conflict checks — callers must have verified the name is
    /// free. The copy happens before registration, so an interruption
    /// leaves an invisible orphan payload, never a broken document.
    pub fn copy_payload_on_device(
        &self,
        from_uuid: &str,
        file_type: &str,
        parent_uuid: &str,
        name: &str,
        size_bytes: Option<u64>,
    ) -> Result<Item> {
        let doc_uuid = uuid::Uuid::new_v4().to_string();
        self.progress
            .step(&format!("Copying payload on device for '{name}'"));
        self.session.run_checked(&format!(
            "cp -f {from} {to}",
            from = shell_quote(&self.remote_path(&format!("{from_uuid}.{file_type}"))),
            to = shell_quote(&self.remote_path(&format!("{doc_uuid}.{file_type}"))),
        ))?;
        self.register_document(
            doc_uuid,
            name.to_string(),
            parent_uuid.to_string(),
            file_type,
            size_bytes,
        )
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

    /// Download a document payload by UUID into memory.
    pub fn download_payload_bytes(
        &self,
        uuid: &str,
        file_type: &str,
        size_hint: Option<u64>,
    ) -> Result<Vec<u8>> {
        self.session.run_checked_bytes_hint(
            &format!(
                "cat {}",
                shell_quote(&self.remote_path(&format!("{uuid}.{file_type}")))
            ),
            size_hint,
            &*self.progress,
        )
    }

    /// Download a document as `.rmdoc` bundle bytes by UUID.
    pub fn download_bundle_bytes(&self, uuid: &str) -> Result<Vec<u8>> {
        let tar_bytes = self.fetch_item_tar(uuid)?;
        bundle::tar_to_rmdoc(&tar_bytes)
    }

    /// Download a document as an `.rmdoc` bundle by UUID.
    pub fn download_bundle_to(&self, uuid: &str, dest: &Path) -> Result<()> {
        std::fs::write(dest, self.download_bundle_bytes(uuid)?)?;
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
        self.download_item(item, output, bundle)
    }

    /// Download one document — or every document matching a glob
    /// pattern. With multiple matches, `output` must be a directory
    /// (or absent = current directory), matched folders are skipped
    /// (`Books/*` downloads Books' documents; `Books/**` recurses),
    /// and colliding output file names are rejected before anything
    /// is transferred.
    pub fn download_matching(
        &self,
        item_ref: &str,
        output: Option<&Path>,
        bundle: bool,
    ) -> Result<Vec<PathBuf>> {
        let items = self.list_items()?;
        let uuids = xochitl::expand_refs(&items, &[item_ref])?;
        let docs: Vec<&Item> = uuids
            .iter()
            .filter_map(|uuid| items.iter().find(|item| &item.uuid == uuid))
            .filter(|item| item.is_document())
            .collect();
        if docs.is_empty() {
            return Err(Error::NotADocument(item_ref.to_string()));
        }
        if docs.len() > 1 {
            if let Some(path) = output
                && !path.is_dir()
            {
                return Err(Error::OutputNotADirectory(path.to_path_buf()));
            }
            // Reject up-front if two matched documents would land on
            // the same local file name.
            let mut names = std::collections::HashSet::new();
            for doc in &docs {
                if !names.insert(download_filename(doc, bundle)) {
                    return Err(Error::NameConflict {
                        name: doc.visible_name.clone(),
                        parent: "the output directory".to_string(),
                    });
                }
            }
        }
        docs.iter()
            .map(|doc| self.download_item(doc, output, bundle))
            .collect()
    }

    fn download_item(&self, item: &Item, output: Option<&Path>, bundle: bool) -> Result<PathBuf> {
        let is_notebook = matches!(item.file_type.as_deref(), None | Some("notebook"));
        if bundle || is_notebook {
            let destination = resolve_destination(output, &download_filename(item, bundle));
            self.progress
                .step(&format!("Downloading '{}'", item.visible_name));
            let tar_bytes = self.fetch_item_tar(&item.uuid)?;
            self.progress.step("Packing .rmdoc");
            std::fs::write(&destination, bundle::tar_to_rmdoc(&tar_bytes)?)?;
            self.progress.finished();
            return Ok(destination);
        }

        let extension = item.file_type.as_deref().expect("payload types are Some");
        let destination = resolve_destination(output, &download_filename(item, bundle));
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

    /// SHA-256 payload hashes for the given `(uuid, extension)`
    /// pairs, in **one** SSH round trip (busybox `sha256sum`).
    /// Missing payload files and hosts without `sha256sum` simply
    /// drop out of the result — callers treat absent hashes as
    /// "unknown", never as an error.
    pub fn payload_hashes(
        &self,
        targets: &[(String, &'static str)],
    ) -> Result<std::collections::HashMap<String, String>> {
        if targets.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.progress.step(&format!(
            "Hashing {} payload(s) on the device",
            targets.len()
        ));
        let files = targets
            .iter()
            .map(|(uuid, ext)| format!("{} \\\n", shell_quote(&format!("{uuid}.{ext}"))))
            .collect::<String>();
        let script = format!(
            "cd {dir} || exit 9\n\
             command -v sha256sum >/dev/null 2>&1 || exit 0\n\
             for f in {files}; do [ -e \"$f\" ] && sha256sum -- \"$f\"; done\n\
             exit 0\n",
            dir = shell_quote(&self.dir),
        );
        let output = self.session.run_checked(&script)?;
        self.progress.finished();
        Ok(parse_payload_hashes(&output))
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
        self.delete_many(&[item_ref], recursive)
    }

    /// Delete several documents/folders in one pass. References may
    /// be UUIDs, logical paths, or glob patterns (`*`, `?`, `[...]`,
    /// `**`). Every reference is resolved against a single listing
    /// **before anything is deleted**, so one bad target aborts the
    /// whole command instead of leaving it half-done. Overlapping
    /// targets (one inside another) are deduplicated; children are
    /// removed before parents.
    pub fn delete_many(&self, item_refs: &[&str], recursive: bool) -> Result<Vec<Item>> {
        let items = self.list_items()?;
        let uuids = xochitl::expand_refs(&items, item_refs)?;
        let refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        let order = deletion_plan(&items, &refs, recursive)?;
        self.execute_deletions(order)
    }

    /// Permanently delete everything in the device's trash (items the
    /// UI "deleted" but keeps restorable). Irreversible.
    pub fn empty_trash(&self) -> Result<Vec<Item>> {
        let items = self.list_items()?;
        let trashed: Vec<&str> = items
            .iter()
            .filter(|item| item.parent == xochitl::TRASH_PARENT)
            .map(|item| item.uuid.as_str())
            .collect();
        // Trashed folders may still contain children; recursive covers
        // them, children-first ordering as usual.
        let order = deletion_plan(&items, &trashed, true)?;
        self.execute_deletions(order)
    }

    fn execute_deletions(&self, order: Vec<Item>) -> Result<Vec<Item>> {
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
        let mut moved = self.move_items(item_ref, destination_ref)?;
        moved
            .pop()
            .ok_or_else(|| Error::PathNotFound(item_ref.to_string()))
    }

    /// Move one item — or everything matching a glob pattern — into
    /// another folder (root allowed). All targets are validated
    /// against a single listing **before anything is written**; items
    /// already in the destination are returned unchanged.
    pub fn move_items(&self, item_ref: &str, destination_ref: &str) -> Result<Vec<Item>> {
        let items = self.list_items()?;
        let destination = xochitl::resolve_folder_ref(&items, destination_ref)?;
        let uuids = xochitl::expand_refs(&items, &[item_ref])?;
        if uuids.contains(&destination) {
            return Err(Error::MoveIntoSelf);
        }
        let plan = xochitl::plan_moves(&items, &uuids, &destination)?;
        if plan.is_empty() {
            // Everything already in place: report the matched items.
            return Ok(uuids
                .iter()
                .filter_map(|uuid| items.iter().find(|item| &item.uuid == uuid))
                .cloned()
                .collect());
        }

        let now = now_ms();
        plan.iter().try_for_each(|item| {
            self.progress
                .step(&format!("Moving '{}'", item.visible_name));
            self.update_metadata(&item.uuid, |metadata| {
                metadata.insert("parent".to_string(), Value::String(destination.clone()));
                metadata.insert("lastModified".to_string(), Value::String(now.to_string()));
            })
        })?;
        self.progress.finished();
        Ok(plan
            .into_iter()
            .map(|item| Item {
                parent: destination.clone(),
                last_modified: now,
                ..item.clone()
            })
            .collect())
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

    /// Gather the device's system state (one SSH round trip for the
    /// system facts, plus the document listing for the counts).
    pub fn system_status(&self) -> Result<SystemStatus> {
        self.progress.step("Reading system state");
        let marker = format!("===RMU:{}===", uuid::Uuid::new_v4().simple());
        let output = self.session.run_checked(&status::status_script(&marker))?;
        let mut system = status::parse_status(&marker, &output);
        system.documents = Some(status::document_counts(&self.list_items()?));
        self.progress.finished();
        Ok(system)
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

/// Parse `sha256sum` output lines (`HASH  uuid.ext`) into
/// `uuid -> hash`.
fn parse_payload_hashes(output: &str) -> std::collections::HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            let uuid = file.trim().trim_start_matches('*').rsplit_once('.')?.0;
            (!hash.is_empty()).then(|| (uuid.to_string(), hash.to_string()))
        })
        .collect()
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

/// Local file name a download of `item` produces (notebooks and
/// forced bundles land as `.rmdoc`, payloads keep their extension).
fn download_filename(item: &Item, bundle: bool) -> String {
    let is_notebook = matches!(item.file_type.as_deref(), None | Some("notebook"));
    if bundle || is_notebook {
        format!("{}.rmdoc", item.visible_name)
    } else {
        format!(
            "{}.{}",
            item.visible_name,
            item.file_type.as_deref().expect("payload types are Some")
        )
    }
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

/// Resolve and order a multi-target deletion: validate every
/// reference and the `recursive` requirement up-front, deduplicate
/// overlapping subtrees, and order children before parents. Pure.
fn deletion_plan(items: &[Item], item_refs: &[&str], recursive: bool) -> Result<Vec<Item>> {
    // Resolve everything first: fail before deleting anything.
    let targets: Vec<&Item> = item_refs
        .iter()
        .map(|item_ref| xochitl::resolve_item_ref(items, item_ref))
        .collect::<Result<_>>()?;

    let mut scheduled = std::collections::HashSet::<&str>::new();
    let mut order: Vec<&Item> = Vec::new();
    targets.iter().try_for_each(|target| -> Result<()> {
        let descendants = xochitl::descendants(items, &target.uuid);
        if target.is_folder() && !descendants.is_empty() && !recursive {
            return Err(Error::FolderNotEmpty(target.visible_name.clone()));
        }
        descendants
            .into_iter()
            .chain(std::iter::once(*target))
            .for_each(|item| {
                if scheduled.insert(item.uuid.as_str()) {
                    order.push(item);
                }
            });
        Ok(())
    })?;

    order.sort_by_key(|item| Reverse(xochitl::depth(items, item)));
    Ok(order.into_iter().cloned().collect())
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

    fn item(uuid: &str, name: &str, parent: &str, kind: ItemKind) -> Item {
        Item {
            uuid: uuid.to_string(),
            visible_name: name.to_string(),
            parent: parent.to_string(),
            kind,
            file_type: None,
            created_time: 0,
            last_modified: 0,
            size_bytes: None,
        }
    }

    fn sample_tree() -> Vec<Item> {
        vec![
            item("b", "Books", "", ItemKind::Folder),
            item("m", "Math", "b", ItemKind::Folder),
            item("la", "Linear Algebra", "m", ItemKind::Document),
            item("ph", "Physics", "b", ItemKind::Document),
            item("n", "Notes", "", ItemKind::Document),
        ]
    }

    #[test]
    fn payload_hash_parsing() {
        let output = "\
abc123  11111111-aaaa-bbbb-cccc-000000000001.pdf
def456  11111111-aaaa-bbbb-cccc-000000000002.epub
garbage line
";
        let hashes = parse_payload_hashes(output);
        assert_eq!(
            hashes.get("11111111-aaaa-bbbb-cccc-000000000001"),
            Some(&"abc123".to_string())
        );
        assert_eq!(
            hashes.get("11111111-aaaa-bbbb-cccc-000000000002"),
            Some(&"def456".to_string())
        );
        assert_eq!(hashes.len(), 2);
    }

    #[test]
    fn deletion_plan_orders_children_first_and_dedupes_overlap() {
        let items = sample_tree();
        // "Books" contains "Books/Math": listing both must not
        // schedule the subtree twice.
        let order = deletion_plan(&items, &["Books", "Books/Math", "Notes"], true).unwrap();
        let uuids: Vec<&str> = order.iter().map(|i| i.uuid.as_str()).collect();
        // Children before parents; every uuid exactly once.
        let position = |u: &str| uuids.iter().position(|x| *x == u).unwrap();
        assert!(position("la") < position("m"));
        assert!(position("m") < position("b"));
        assert!(position("ph") < position("b"));
        assert_eq!(uuids.len(), 5);
    }

    #[test]
    fn deletion_plan_covers_trashed_subtrees() {
        // A trashed folder still parents its children; emptying the
        // trash must delete them too, children first.
        let items = vec![
            item("tf", "Old Folder", xochitl::TRASH_PARENT, ItemKind::Folder),
            item("tc", "Inside", "tf", ItemKind::Document),
            item("td", "Old Doc", xochitl::TRASH_PARENT, ItemKind::Document),
            item("keep", "Keep", "", ItemKind::Document),
        ];
        let trashed: Vec<&str> = items
            .iter()
            .filter(|i| i.parent == xochitl::TRASH_PARENT)
            .map(|i| i.uuid.as_str())
            .collect();
        let order = deletion_plan(&items, &trashed, true).unwrap();
        let uuids: Vec<&str> = order.iter().map(|i| i.uuid.as_str()).collect();
        let position = |u: &str| uuids.iter().position(|x| *x == u).unwrap();
        assert!(position("tc") < position("tf"));
        assert_eq!(uuids.len(), 3);
        assert!(!uuids.contains(&"keep"));
    }

    #[test]
    fn deletion_plan_fails_fast_on_any_bad_target() {
        let items = sample_tree();
        // A typo in one target aborts the whole plan.
        assert!(matches!(
            deletion_plan(&items, &["Notes", "Boks"], true),
            Err(Error::PathNotFound(_))
        ));
    }

    #[test]
    fn deletion_plan_names_the_non_empty_folder() {
        let items = sample_tree();
        match deletion_plan(&items, &["Notes", "Books"], false) {
            Err(Error::FolderNotEmpty(name)) => assert_eq!(name, "Books"),
            other => panic!("expected FolderNotEmpty, got {other:?}"),
        }
    }

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
