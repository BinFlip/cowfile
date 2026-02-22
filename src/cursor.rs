//! Cursor-based [`std::io`] compatibility for [`CowFile`](crate::CowFile).
//!
//! [`CowFileCursor`] wraps a reference to a `CowFile` and maintains an internal
//! byte position, implementing [`Read`](std::io::Read), [`Write`](std::io::Write),
//! and [`Seek`](std::io::Seek). This allows a `CowFile` to be used with any API
//! that expects standard I/O traits.
//!
//! # Examples
//!
//! ```
//! use std::io::{Read, Write, Seek, SeekFrom};
//! use cowfile::CowFile;
//!
//! let pf = CowFile::from_vec(vec![0u8; 64]);
//! let mut cursor = pf.cursor();
//!
//! // Write via std::io::Write
//! cursor.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
//!
//! // Seek back and read via std::io::Read
//! cursor.seek(SeekFrom::Start(0)).unwrap();
//! let mut buf = [0u8; 4];
//! cursor.read_exact(&mut buf).unwrap();
//! assert_eq!(buf, [0xDE, 0xAD, 0xBE, 0xEF]);
//! ```

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::cowfile::CowFile;

/// A cursor over a [`CowFile`] implementing [`Read`], [`Write`], and [`Seek`].
///
/// The cursor maintains an internal byte position that advances on each read
/// or write. Multiple cursors can exist over the same `CowFile` simultaneously,
/// each with its own independent position.
///
/// Created via [`CowFile::cursor`].
///
/// # Examples
///
/// ```
/// use std::io::{Read, Seek, SeekFrom};
/// use cowfile::CowFile;
///
/// let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
/// let mut cursor = pf.cursor();
///
/// cursor.seek(SeekFrom::Start(2)).unwrap();
/// let mut buf = [0u8; 2];
/// cursor.read_exact(&mut buf).unwrap();
/// assert_eq!(buf, [3, 4]);
/// ```
pub struct CowFileCursor<'a> {
    cowfile: &'a CowFile,
    position: usize,
}

impl<'a> CowFileCursor<'a> {
    /// Creates a new cursor at position 0.
    pub(crate) fn new(cowfile: &'a CowFile) -> Self {
        CowFileCursor {
            cowfile,
            position: 0,
        }
    }

    /// Returns the current byte position of the cursor.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Sets the cursor position directly.
    pub fn set_position(&mut self, pos: usize) {
        self.position = pos;
    }
}

impl Read for CowFileCursor<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.cowfile.len().saturating_sub(self.position);
        let to_read = buf.len().min(remaining);
        if to_read == 0 {
            return Ok(0);
        }

        let data = self
            .cowfile
            .read(self.position, to_read)
            .map_err(io::Error::other)?;
        buf[..to_read].copy_from_slice(&data);
        self.position += to_read;
        Ok(to_read)
    }
}

impl Write for CowFileCursor<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        self.cowfile
            .write(self.position, buf)
            .map_err(io::Error::other)?;
        self.position += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for CowFileCursor<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.cowfile.len() as i64 + n,
            SeekFrom::Current(n) => self.position as i64 + n,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative position",
            ));
        }

        self.position = new_pos as usize;
        Ok(self.position as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use crate::CowFile;

    #[test]
    fn test_cursor_read_sequential() {
        let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let mut cursor = pf.cursor();

        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [5, 6, 7, 8]);
    }

    #[test]
    fn test_cursor_write_sequential() {
        let pf = CowFile::from_vec(vec![0u8; 8]);
        let mut cursor = pf.cursor();

        cursor.write_all(&[0xAA, 0xBB]).unwrap();
        cursor.write_all(&[0xCC, 0xDD]).unwrap();
        assert_eq!(cursor.position(), 4);

        let data = pf.read(0, 4).unwrap();
        assert_eq!(data, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn test_cursor_seek_start() {
        let pf = CowFile::from_vec(vec![10, 20, 30, 40, 50]);
        let mut cursor = pf.cursor();

        cursor.seek(SeekFrom::Start(3)).unwrap();
        assert_eq!(cursor.position(), 3);

        let mut buf = [0u8; 1];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 40);
    }

    #[test]
    fn test_cursor_seek_end() {
        let pf = CowFile::from_vec(vec![10, 20, 30, 40, 50]);
        let mut cursor = pf.cursor();

        cursor.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(cursor.position(), 3);

        let mut buf = [0u8; 2];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [40, 50]);
    }

    #[test]
    fn test_cursor_seek_current() {
        let pf = CowFile::from_vec(vec![10, 20, 30, 40, 50]);
        let mut cursor = pf.cursor();

        cursor.seek(SeekFrom::Start(2)).unwrap();
        cursor.seek(SeekFrom::Current(1)).unwrap();
        assert_eq!(cursor.position(), 3);

        cursor.seek(SeekFrom::Current(-2)).unwrap();
        assert_eq!(cursor.position(), 1);
    }

    #[test]
    fn test_cursor_seek_before_start() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        let mut cursor = pf.cursor();

        let result = cursor.seek(SeekFrom::Current(-1));
        assert!(result.is_err());
    }

    #[test]
    fn test_cursor_read_at_eof() {
        let pf = CowFile::from_vec(vec![1, 2, 3]);
        let mut cursor = pf.cursor();

        cursor.seek(SeekFrom::End(0)).unwrap();
        let mut buf = [0u8; 4];
        let n = cursor.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_cursor_read_partial_at_eof() {
        let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
        let mut cursor = pf.cursor();

        cursor.seek(SeekFrom::Start(3)).unwrap();
        let mut buf = [0u8; 4];
        let n = cursor.read(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[4, 5]);
    }

    #[test]
    fn test_cursor_read_write_interleaved() {
        let pf = CowFile::from_vec(vec![0u8; 16]);
        let mut cursor = pf.cursor();

        cursor.write_all(&[0xAA, 0xBB]).unwrap();
        cursor.write_all(&[0xCC, 0xDD]).unwrap();
        assert_eq!(cursor.position(), 4);

        cursor.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn test_cursor_with_std_io_copy() {
        let src = CowFile::from_vec(vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let dst = CowFile::from_vec(vec![0u8; 8]);

        let mut src_cursor = src.cursor();
        let mut dst_cursor = dst.cursor();

        let copied = std::io::copy(&mut src_cursor, &mut dst_cursor).unwrap();
        assert_eq!(copied, 8);

        let output = dst.to_vec().unwrap();
        assert_eq!(output, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_cursor_position_and_set_position() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        let mut cursor = pf.cursor();

        assert_eq!(cursor.position(), 0);
        cursor.set_position(5);
        assert_eq!(cursor.position(), 5);
    }

    #[test]
    fn test_cursor_seek_beyond_end() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        let mut cursor = pf.cursor();

        // Seeking beyond the end is allowed (like std::io::Cursor).
        let pos = cursor.seek(SeekFrom::Start(100)).unwrap();
        assert_eq!(pos, 100);

        // Reads at that position return 0 bytes.
        let mut buf = [0u8; 4];
        let n = cursor.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_cursor_flush_is_noop() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        let mut cursor = pf.cursor();
        cursor.flush().unwrap();
    }
}
