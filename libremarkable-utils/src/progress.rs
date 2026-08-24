//! Progress reporting hooks.
//!
//! The library never prints. Long-running operations report through
//! this trait so frontends can render progress UIs (on stderr) while
//! stdout stays machine-usable.

/// Observer for operation progress.
///
/// Events arrive in phases: `step` announces what is happening,
/// `bytes` streams transfer progress within the current step, and
/// `finished` marks the end of a whole operation (always called,
/// possibly more than once — implementations must tolerate that).
pub trait Progress {
    /// A new phase began (e.g. "Uploading sample.pdf").
    fn step(&self, message: &str);

    /// Byte-transfer progress within the current step. `total` is
    /// `None` for streams of unknown length (e.g. tar from the device).
    fn bytes(&self, transferred: u64, total: Option<u64>);

    /// The current byte transfer completed.
    fn bytes_done(&self);

    /// The whole operation completed (successfully or not); tear down
    /// any UI so regular output can be printed cleanly.
    fn finished(&self);
}

/// Ignores all progress events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn step(&self, _message: &str) {}
    fn bytes(&self, _transferred: u64, _total: Option<u64>) {}
    fn bytes_done(&self) {}
    fn finished(&self) {}
}
