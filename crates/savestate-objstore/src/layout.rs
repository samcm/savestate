//! A file's data: its allocated extents, in order, as one byte stream cut into
//! chunks. Holes are not part of the data; a file rebuilt from it has the same size
//! and the same holes.

use std::{fs::File, io, os::unix::fs::FileExt as _};

use nix::{errno::Errno, unistd::Whence};

use crate::{CHUNK, Error};

/// Where a file's data lies in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    /// File size, holes included.
    pub size: u64,
    /// Allocated extents `(offset, length)`, ascending, non-empty, not overlapping.
    pub extents: Vec<(u64, u64)>,
    /// Offset of each extent within the data.
    starts: Vec<u64>,
    /// Bytes of data: the extents' lengths summed.
    pub len: u64,
}

impl Layout {
    /// The layout of `file`, from `SEEK_DATA` and `SEEK_HOLE`. Filesystems without
    /// holes report the whole file as data.
    pub fn of(file: &File) -> io::Result<Self> {
        let size = file.metadata()?.len();
        let mut extents = Vec::new();
        let mut pos = 0;
        while pos < size {
            let start = match nix::unistd::lseek(file, offset(pos)?, Whence::SeekData) {
                Ok(start) => unsigned(start)?,
                // Nothing but a hole from `pos` to the end.
                Err(Errno::ENXIO) => break,
                Err(errno) => return Err(errno.into()),
            };
            if start >= size {
                break;
            }
            let end =
                unsigned(nix::unistd::lseek(file, offset(start)?, Whence::SeekHole)?)?.min(size);
            extents.push((start, end - start));
            pos = end;
        }
        Ok(Self::new(size, extents))
    }

    fn new(size: u64, extents: Vec<(u64, u64)>) -> Self {
        let mut starts = Vec::with_capacity(extents.len());
        let mut len = 0;
        for &(_, length) in &extents {
            starts.push(len);
            len += length;
        }
        Self {
            size,
            extents,
            starts,
            len,
        }
    }

    /// A layout read from a store, checked so that writing it stays inside the file.
    pub fn checked(size: u64, extents: Vec<(u64, u64)>) -> Result<Self, Error> {
        let mut end = 0;
        for &(offset, length) in &extents {
            let ok = length > 0
                && offset >= end
                && offset.checked_add(length).is_some_and(|stop| stop <= size);
            if !ok {
                return Err(Error::Corrupt(format!(
                    "extent ({offset}, {length}) is empty, out of order or past the file's {size} bytes"
                )));
            }
            end = offset + length;
        }
        Ok(Self::new(size, extents))
    }

    /// Number of chunks the data is cut into.
    pub fn chunks(&self) -> u64 {
        self.len.div_ceil(CHUNK)
    }

    /// The data range of chunk `k`.
    pub fn chunk(&self, k: u64) -> (u64, u64) {
        let start = k * CHUNK;
        (start, (start + CHUNK).min(self.len))
    }

    /// The pieces of data range `[start, end)`: `(file offset, offset in the range,
    /// length)`.
    fn pieces(&self, start: u64, end: u64) -> Vec<(u64, usize, usize)> {
        let mut pieces = Vec::new();
        let mut i = match self.starts.binary_search(&start) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        while i < self.extents.len() && self.starts[i] < end {
            let (offset, length) = self.extents[i];
            let from = start.max(self.starts[i]);
            let to = end.min(self.starts[i] + length);
            if from < to {
                // Within one chunk, so within usize.
                pieces.push((
                    offset + (from - self.starts[i]),
                    small(from - start),
                    small(to - from),
                ));
            }
            i += 1;
        }
        pieces
    }

    /// Reads data range `[start, end)` from `file`.
    pub fn read(&self, file: &File, start: u64, end: u64) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; small(end - start)];
        for (offset, at, length) in self.pieces(start, end) {
            file.read_exact_at(&mut buf[at..at + length], offset)?;
        }
        Ok(buf)
    }

    /// Writes `data`, data range `[start, start + data.len())`, into `file`.
    pub fn write(&self, file: &File, start: u64, data: &[u8]) -> io::Result<()> {
        let end = start + data.len() as u64;
        for (offset, at, length) in self.pieces(start, end) {
            file.write_all_at(&data[at..at + length], offset)?;
        }
        Ok(())
    }
}

/// A length within one chunk.
fn small(n: u64) -> usize {
    usize::try_from(n.min(CHUNK)).unwrap_or(usize::MAX)
}

fn offset(n: u64) -> io::Result<i64> {
    i64::try_from(n).map_err(|_| io::Error::other("file offset out of range"))
}

fn unsigned(n: i64) -> io::Result<u64> {
    u64::try_from(n).map_err(|_| io::Error::other("negative file offset"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Data ranges that straddle extents and chunk boundaries come back from exactly
    /// the bytes of the extents, so a file rebuilt chunk by chunk equals the original.
    #[test]
    fn chunks_map_onto_extents() {
        let layout =
            Layout::checked(3 * CHUNK, vec![(10, 5), (CHUNK - 3, 6), (2 * CHUNK, CHUNK)]).unwrap();
        assert_eq!(layout.len, 11 + CHUNK);
        assert_eq!(layout.chunks(), 2);
        assert_eq!(
            layout.pieces(0, CHUNK),
            vec![
                (10, 0, 5),
                (CHUNK - 3, 5, 6),
                (2 * CHUNK, 11, small(CHUNK - 11))
            ]
        );
        assert_eq!(layout.chunk(1), (CHUNK, CHUNK + 11));
        assert_eq!(
            layout.pieces(CHUNK, CHUNK + 11),
            vec![(3 * CHUNK - 11, 0, 11)]
        );
        // Mid-extent starts.
        assert_eq!(
            layout.pieces(7, 12),
            vec![(CHUNK - 1, 0, 4), (2 * CHUNK, 4, 1)]
        );
    }
}
