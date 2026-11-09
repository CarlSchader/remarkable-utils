//! `rmu hub`: git-backed multi-tablet sync (see `docs/hub-design.md`).
//!
//! One origin git repository holds the library; tablets are working
//! copies that rmu snapshots, merges and applies over SSH. Git is used
//! for storage, history and transport only; every merge is computed
//! by rmu.
//!
//! Submodules:
//! - [`model`] — pure data model: layout mapping, mirror rules,
//!   repository and tablet state formats, normalisation, document
//!   grouping, snapshot validation, carry-forward.
//! - [`merge`] — pure three-way merge, deletion guard, forks.
//! - [`device`] — tablet I/O behind a trait (lock, watchdog, stat
//!   pass, staged apply).
//!
//! Errors: pure functions that fail only on their own terms return
//! their module's error ([`model::ModelError`], [`merge::MergeError`]);
//! functions that call caller-supplied readers return [`crate::Result`],
//! and their own failures are variants of [`crate::Error`]
//! (`HubModel`, `HubMerge`). Hub errors never wrap [`crate::Error`].

pub mod device;
pub mod merge;
pub mod model;
