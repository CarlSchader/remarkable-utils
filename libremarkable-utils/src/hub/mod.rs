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
//! - [`device`] — tablet I/O behind the [`device::Tablet`] trait (lock,
//!   watchdog, stat pass, streaming download, staged apply), over SSH
//!   or a fault-injecting fake.
//!
//! Errors: pure functions that fail only on their own terms return
//! their module's error ([`model::ModelError`], [`merge::MergeError`],
//! [`device::DeviceError`]). Functions that do I/O or call
//! caller-supplied readers return [`crate::Result`]; their own failures
//! are the variants `HubModel`, `HubMerge` and `HubDevice` of
//! [`crate::Error`]. Hub errors never wrap [`crate::Error`].

pub mod device;
pub mod merge;
pub mod model;
