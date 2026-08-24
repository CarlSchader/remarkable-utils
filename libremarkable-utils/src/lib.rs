//! Shared types and logic for reMarkable tablet tooling.
//!
//! Domain-neutral building blocks (device connection info, document
//! metadata, file-format parsing, etc.) live here so binary crates in
//! the workspace can share them. Keep tool-specific CLI concerns out of
//! this crate.

pub mod device;

pub use device::Device;
