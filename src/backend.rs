//! Immutable data source backends for [`CowFile`](crate::CowFile).
//!
//! The [`Backend`] enum represents the different ways binary data can be provided.
//! Once constructed, a backend is never modified — it serves as the immutable base
//! layer for the copy-on-write overlay.

use std::{fmt, fs::File, path::Path};

use memmap2::Mmap;

use crate::error::Result;

#[cfg(test)]
use crate::error::Error;

/// The immutable base data source for a [`CowFile`](crate::CowFile).
///
/// Supports two storage modes:
/// - [`Backend::Vec`]: Owned byte vector, suitable when data is already in memory.
/// - [`Backend::Mmap`]: Memory-mapped file, providing zero-copy access to on-disk data.
///
/// Both variants are `Send + Sync`, making the backend safe for concurrent access.
pub(crate) enum Backend {
    /// Owned byte vector.
    Vec(Vec<u8>),
    /// Memory-mapped file for zero-copy access.
    Mmap(Mmap),
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Vec(v) => f
                .debug_tuple("Vec")
                .field(&format_args!("[{} bytes]", v.len()))
                .finish(),
            Backend::Mmap(m) => f
                .debug_tuple("Mmap")
                .field(&format_args!("[{} bytes]", m.len()))
                .finish(),
        }
    }
}

impl Backend {
    /// Creates a new `Backend` by memory-mapping the file at the given path.
    ///
    /// The file is opened read-only and mapped into the process address space.
    /// The operating system handles paging, so only accessed regions are loaded
    /// into physical memory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be opened or memory-mapped.
    pub(crate) fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        // SAFETY: The file is opened read-only and the backend is never modified.
        // The memory mapping is valid for the lifetime of the Mmap value.
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Backend::Mmap(mmap))
    }

    /// Returns the total length of the underlying data in bytes.
    pub(crate) fn len(&self) -> u64 {
        match self {
            Backend::Vec(v) => v.len() as u64,
            Backend::Mmap(m) => m.len() as u64,
        }
    }

    /// Returns the entire underlying data as a byte slice.
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Backend::Vec(v) => v.as_slice(),
            Backend::Mmap(m) => m.as_ref(),
        }
    }

    /// Returns a sub-slice of the underlying data at the given offset and length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if the requested range exceeds the data length.
    #[cfg(test)]
    pub(crate) fn slice(&self, offset: u64, length: u64) -> Result<&[u8]> {
        let data = self.as_slice();
        let start = offset as usize;
        let end = start
            .checked_add(length as usize)
            .ok_or(Error::OutOfBounds {
                offset,
                length,
                file_size: self.len(),
            })?;

        if end > data.len() {
            return Err(Error::OutOfBounds {
                offset,
                length,
                file_size: self.len(),
            });
        }

        Ok(&data[start..end])
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::Backend;

    #[test]
    fn test_backend_vec_basic() {
        let data = vec![1, 2, 3, 4, 5];
        let backend = Backend::Vec(data);
        assert_eq!(backend.len(), 5);
        assert_eq!(backend.as_slice(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_backend_vec_empty() {
        let backend = Backend::Vec(vec![]);
        assert_eq!(backend.len(), 0);
        assert_eq!(backend.as_slice(), &[]);
    }

    #[test]
    fn test_backend_vec_slice_valid() {
        let backend = Backend::Vec(vec![10, 20, 30, 40, 50]);
        let slice = backend.slice(1, 3).unwrap();
        assert_eq!(slice, &[20, 30, 40]);
    }

    #[test]
    fn test_backend_vec_slice_full() {
        let backend = Backend::Vec(vec![10, 20, 30]);
        let slice = backend.slice(0, 3).unwrap();
        assert_eq!(slice, &[10, 20, 30]);
    }

    #[test]
    fn test_backend_vec_slice_out_of_bounds() {
        let backend = Backend::Vec(vec![10, 20, 30]);
        let result = backend.slice(2, 5);
        assert!(result.is_err());
    }

    #[test]
    fn test_backend_vec_slice_zero_length() {
        let backend = Backend::Vec(vec![10, 20, 30]);
        let slice = backend.slice(1, 0).unwrap();
        assert_eq!(slice, &[]);
    }

    #[test]
    fn test_backend_mmap() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        tmpfile.flush().unwrap();

        let backend = Backend::from_path(tmpfile.path()).unwrap();
        assert_eq!(backend.len(), 4);
        assert_eq!(backend.as_slice(), &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_backend_mmap_slice() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0x01, 0x02, 0x03, 0x04, 0x05]).unwrap();
        tmpfile.flush().unwrap();

        let backend = Backend::from_path(tmpfile.path()).unwrap();
        let slice = backend.slice(1, 3).unwrap();
        assert_eq!(slice, &[0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_backend_mmap_nonexistent_path() {
        let result = Backend::from_path("/nonexistent/path/to/file.bin");
        assert!(result.is_err());
    }
}
