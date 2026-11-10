//! Shared types and logic for reMarkable tablet tooling.
//!
//! The tablet stores each notebook/document as a flat set of files in
//! the xochitl data directory: `<uuid>.metadata` (name, parent folder,
//! item type), `<uuid>.content` (file type and layout info), and the
//! payload itself (`<uuid>.pdf` / `<uuid>.epub`). This crate rebuilds
//! the logical folder tree from that metadata and performs operations
//! on it over SSH.
//!
//! Module map:
//! - [`bundle`] — `.rmdoc` bundle creation (tar-from-device → zip).
//! - [`ssh`] — subprocess transport around the system `ssh` binary
//!   (multiplexed, optional password auth via a self-askpass helper).
//! - [`git`] — plumbing over the system `git` binary for bare
//!   repositories: remote transport, typed push rejection, tree
//!   listing, batched blob reads, and commits via `fast-import`.
//! - [`xochitl`] — on-device file formats and pure tree logic
//!   (path/UUID resolution, rendering, conflict and cycle checks).
//! - [`client`] — high-level operations: list, mkdir, upload,
//!   download, delete, move, rename, restart xochitl.
//! - [`sync`] — folder sync: endpoint parsing, snapshots, pure
//!   planners, executors (see `docs/sync-design.md`).
//! - [`hub`] — `rmu hub`, git-backed multi-tablet sync (see
//!   `docs/hub-design.md`); [`hub::model`] is its pure data model.
//! - [`pdf`] — a minimal, conservative PDF page counter (pure).
//! - [`status`] — device system state (model, firmware, CPU/RAM/disk,
//!   battery), gathered in one round trip.
//! - [`progress`] — observer trait for progress reporting; the
//!   library itself never prints.
//! - [`error`] — typed errors for all of the above.
//! - `relative_path` (private) — the safe relative-path splitter shared
//!   by `sync` and `hub::model`.
//!
//! Keep tool-specific CLI concerns out of this crate.

pub mod bundle;
pub mod client;
pub mod error;
pub mod git;
pub mod hub;
pub mod pdf;
pub mod progress;
mod relative_path;
pub mod ssh;
pub mod status;
pub mod sync;
pub mod xochitl;

pub use client::Client;
pub use error::{Error, Result};
