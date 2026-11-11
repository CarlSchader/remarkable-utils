//! The one chunked copy loop, shared by ssh transfers
//! (`ssh::copy_with_progress`) and git's blob writes
//! (`git::CommitWriter`). Low-level I/O pump: the accepted exception to
//! the functional style.

use std::io::{self, Read};

/// Bytes read per chunk.
const CHUNK: usize = 64 * 1024;

/// Where a [`copy`] failed.
#[derive(Debug)]
pub(crate) enum CopyError<E> {
    /// Reading the source failed (never [`io::ErrorKind::Interrupted`]:
    /// that is retried).
    Read(io::Error),
    /// The sink failed; its own error.
    Write(E),
}

impl CopyError<io::Error> {
    /// The error, whichever side it came from.
    pub(crate) fn into_io(self) -> io::Error {
        match self {
            Self::Read(error) | Self::Write(error) => error,
        }
    }
}

/// Copy `reader` into `sink` in chunks until end of file, retrying a read
/// a signal interrupted (`EINTR`). `sink` gets every chunk read, never an
/// empty one, and does its own progress reporting.
///
/// With a `bound`, at most `bound + 1` bytes are read and at most `bound`
/// handed to `sink`: a count above `bound` means the reader is longer
/// than declared (the rest is never read). Returns the bytes read.
pub(crate) fn copy<E>(
    reader: &mut (impl Read + ?Sized),
    bound: Option<u64>,
    mut sink: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<u64, CopyError<E>> {
    let mut buffer = vec![0u8; CHUNK];
    let mut copied = 0u64;
    loop {
        let want = match bound {
            None => CHUNK,
            Some(bound) if copied > bound => return Ok(copied),
            // One byte past `bound`, so an over-long reader is detected.
            Some(bound) => {
                CHUNK.min(usize::try_from((bound - copied).saturating_add(1)).unwrap_or(usize::MAX))
            }
        };
        let n = match reader.read(&mut buffer[..want]) {
            Ok(0) => return Ok(copied),
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(CopyError::Read(error)),
        };
        let keep = bound.map_or(n, |bound| {
            n.min(usize::try_from(bound.saturating_sub(copied)).unwrap_or(usize::MAX))
        });
        if keep > 0 {
            sink(&buffer[..keep]).map_err(CopyError::Write)?;
        }
        copied += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yields `data` in reads of at most `step` bytes, failing every
    /// other read with `Interrupted` (and once, at `fail_at`, for real).
    struct Flaky<'a> {
        data: &'a [u8],
        step: usize,
        interrupt: bool,
        fail_at: Option<usize>,
        reads: usize,
    }

    impl<'a> Flaky<'a> {
        fn new(data: &'a [u8], step: usize) -> Self {
            Self {
                data,
                step,
                interrupt: true,
                fail_at: None,
                reads: 0,
            }
        }
    }

    impl Read for Flaky<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if !self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.reads += 1;
            if self.fail_at == Some(self.reads) {
                return Err(io::Error::other("injected read failure"));
            }
            let n = buf.len().min(self.step).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn collect(reader: &mut impl Read, bound: Option<u64>) -> (u64, Vec<Vec<u8>>) {
        let mut chunks = Vec::new();
        let copied = copy(reader, bound, |chunk| {
            chunks.push(chunk.to_vec());
            Ok::<_, io::Error>(())
        })
        .unwrap();
        (copied, chunks)
    }

    #[test]
    fn unbounded_copies_everything_and_retries_interrupted_reads() {
        let data: Vec<u8> = (0..150 * 1024).map(|i| (i % 251) as u8).collect();
        let (copied, chunks) = collect(&mut Flaky::new(&data, usize::MAX), None);
        assert_eq!(copied, data.len() as u64);
        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            [CHUNK, CHUNK, 150 * 1024 - 2 * CHUNK]
        );
        assert_eq!(chunks.concat(), data);
        assert_eq!(collect(&mut Flaky::new(b"", 1), None), (0, Vec::new()));
    }

    #[test]
    fn bounded_reads_one_byte_past_the_bound_and_never_hands_it_on() {
        let data = b"0123456789";
        // Exact: the extra read finds the end.
        let (copied, chunks) = collect(&mut Flaky::new(data, 3), Some(10));
        assert_eq!((copied, chunks.concat()), (10, data.to_vec()));
        // Short.
        let (copied, chunks) = collect(&mut Flaky::new(data, 3), Some(12));
        assert_eq!((copied, chunks.concat()), (10, data.to_vec()));
        // Over-long: one byte more is read, only the bound handed on, and
        // the rest is left unread.
        let mut reader = Flaky::new(data, 4);
        let (copied, chunks) = collect(&mut reader, Some(6));
        assert_eq!((copied, chunks.concat()), (7, b"012345".to_vec()));
        assert_eq!(reader.data, b"789");
        // Nothing declared: one byte tells an empty reader from a longer one.
        let mut reader = Flaky::new(data, 4);
        assert_eq!(collect(&mut reader, Some(0)), (1, Vec::new()));
        assert_eq!(reader.data, b"123456789");
    }

    #[test]
    fn read_and_sink_failures_are_told_apart() {
        let mut reader = Flaky::new(b"abcdef", 2);
        reader.fail_at = Some(2);
        let mut seen = Vec::new();
        let failed = copy(&mut reader, None, |chunk| {
            seen.extend_from_slice(chunk);
            Ok::<_, &str>(())
        });
        assert!(
            matches!(failed, Err(CopyError::Read(e)) if e.to_string() == "injected read failure")
        );
        assert_eq!(seen, b"ab");

        let mut calls = 0;
        let failed = copy(&mut Flaky::new(b"abcdef", 2), Some(6), |_| {
            calls += 1;
            Err("sink full")
        });
        assert!(matches!(failed, Err(CopyError::Write("sink full"))));
        assert_eq!(calls, 1, "the copy stops at the sink's first error");
    }
}
