//! The main [`CowFile`] type combining an immutable backend with a copy-on-write overlay.
//!
//! `CowFile` is the primary public API of this crate. It wraps immutable binary data
//! (from a [`Vec<u8>`] or a memory-mapped file) with a sparse overlay that tracks
//! byte-level modifications without ever mutating the original data.
//!
//! # Thread Safety
//!
//! `CowFile` is [`Send`] + [`Sync`]. The immutable backend requires no synchronization,
//! and the overlay is protected by an internal [`RwLock`](std::sync::RwLock) that allows
//! concurrent reads with exclusive writes.

use std::{fmt, io::Write, path::Path, sync::RwLock};

use crate::{
    backend::Backend,
    cursor::CowFileCursor,
    error::{Error, Result},
    overlay::Overlay,
    primitives::Primitive,
    traits::{ReadFrom, WriteTo},
};

/// Threshold above which [`to_file`](CowFile::to_file) uses a writable memory map
/// instead of buffered I/O. Set to 64 MiB.
const MMAP_WRITE_THRESHOLD: u64 = 64 * 1024 * 1024;

/// A patchable file providing copy-on-write overlay semantics over immutable binary data.
///
/// `CowFile` allows multiple modification passes over an immutable base layer without
/// ever copying the full binary. Modifications are tracked as sparse overlays, and a
/// final merged output is produced only when explicitly requested via [`to_vec`](CowFile::to_vec)
/// or [`to_file`](CowFile::to_file).
///
/// # Architecture
///
/// ```text
///  Base Layer (immutable)        Overlay (copy-on-write)
/// +---------------------+      +-------------------------+
/// | Vec<u8> or Mmap     |      | committed: BTreeMap     |
/// | (never modified)    | <--- | pending:   BTreeMap     |
/// +---------------------+      +-------------------------+
/// ```
///
/// The overlay has two tiers:
/// - **Pending**: Modifications from the current pass, not yet committed.
/// - **Committed**: Consolidated modifications from previous [`commit`](CowFile::commit) calls.
///
/// When reading, layers are composited: **pending > committed > base**.
///
/// # Thread Safety
///
/// `CowFile` is `Send + Sync`. Multiple threads can read concurrently. Write access
/// to the overlay is synchronized via an internal `RwLock`.
///
/// # Examples
///
/// ```
/// use cowfile::CowFile;
///
/// // Create from owned bytes
/// let pf = CowFile::from_vec(vec![0u8; 100]);
///
/// // Apply modifications without copying the full buffer
/// pf.write(10, &[0xFF, 0xFE]).unwrap();
/// pf.write(20, &[0xAA, 0xBB, 0xCC]).unwrap();
///
/// // Read back modified data
/// let data = pf.read(10, 2).unwrap();
/// assert_eq!(data, vec![0xFF, 0xFE]);
///
/// // Commit consolidates pending changes
/// pf.commit().unwrap();
///
/// // More modifications in a second pass
/// pf.write(30, &[0xDD]).unwrap();
///
/// // Produce final output with all modifications applied
/// let output = pf.to_vec().unwrap();
/// assert_eq!(output[10], 0xFF);
/// assert_eq!(output[20], 0xAA);
/// assert_eq!(output[30], 0xDD);
/// ```
pub struct CowFile {
    /// Immutable base data — never modified after construction.
    backend: Backend,
    /// Copy-on-write overlay behind RwLock for thread-safe access.
    overlay: RwLock<Overlay>,
}

impl fmt::Debug for CowFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CowFile")
            .field("backend", &self.backend)
            .field("len", &self.backend.len())
            .finish_non_exhaustive()
    }
}

// Static assertion: CowFile must be Send + Sync.
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<CowFile>();
    }
    let _ = check;
};

impl CowFile {
    /// Creates a `CowFile` from an owned byte vector.
    ///
    /// The provided bytes become the immutable base layer. No copies are made
    /// during construction — the vector is moved into the `CowFile`.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0x4D, 0x5A, 0x90, 0x00]);
    /// assert_eq!(pf.len(), 4);
    /// ```
    pub fn from_vec(data: Vec<u8>) -> Self {
        CowFile {
            backend: Backend::Vec(data),
            overlay: RwLock::new(Overlay::new()),
        }
    }

    /// Creates a `CowFile` by memory-mapping a file from the given path.
    ///
    /// The file is mapped read-only into the process address space. The operating
    /// system handles paging, so only accessed regions are loaded into physical
    /// memory. This is ideal for large binaries.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be opened or memory-mapped.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_path("binary.exe").unwrap();
    /// println!("File size: {} bytes", pf.len());
    /// ```
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let backend = Backend::from_path(path)?;
        Ok(CowFile {
            backend,
            overlay: RwLock::new(Overlay::new()),
        })
    }

    /// Returns the total length of the base data in bytes.
    ///
    /// This reflects the original, unmodified size. Modifications do not change
    /// the length (writes beyond the end are rejected as out-of-bounds).
    pub fn len(&self) -> u64 {
        self.backend.len()
    }

    /// Returns `true` if the base data is empty (zero bytes).
    pub fn is_empty(&self) -> bool {
        self.backend.len() == 0
    }

    /// Returns the unmodified base data as a byte slice.
    ///
    /// This returns the raw base data without any overlay applied. Useful for
    /// comparing the original against the modified version.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![1, 2, 3]);
    /// pf.write(0, &[0xFF]).unwrap();
    /// assert_eq!(pf.base_data(), &[1, 2, 3]); // Base is unchanged
    /// ```
    pub fn base_data(&self) -> &[u8] {
        self.backend.as_slice()
    }

    /// Returns `true` if there are uncommitted (pending) modifications.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    pub fn has_pending(&self) -> Result<bool> {
        let guard = self
            .overlay
            .read()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;
        Ok(guard.has_pending())
    }

    /// Returns `true` if there are any modifications (pending or committed).
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    pub fn has_modifications(&self) -> Result<bool> {
        let guard = self
            .overlay
            .read()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;
        Ok(guard.has_modifications())
    }

    /// Reads `length` bytes starting at `offset`, applying all modifications.
    ///
    /// The returned bytes reflect the composition of committed modifications,
    /// pending modifications, and unmodified base data. Priority order:
    /// **pending > committed > base** (later layers overwrite earlier ones).
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if the requested range exceeds the base data size.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
    /// pf.write(2, &[0xFF]).unwrap();
    ///
    /// let data = pf.read(1, 3).unwrap();
    /// assert_eq!(data, vec![2, 0xFF, 4]);
    /// ```
    pub fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        self.check_bounds(offset, length)?;

        let guard = self
            .overlay
            .read()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        Ok(guard.read(offset, length, self.backend.as_slice()))
    }

    /// Reads a single byte at the given offset, applying all modifications.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if the offset is beyond the file size.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0xAA, 0xBB, 0xCC]);
    /// assert_eq!(pf.read_byte(1).unwrap(), 0xBB);
    ///
    /// pf.write_byte(1, 0xFF).unwrap();
    /// assert_eq!(pf.read_byte(1).unwrap(), 0xFF);
    /// ```
    pub fn read_byte(&self, offset: u64) -> Result<u8> {
        let data = self.read(offset, 1)?;
        Ok(data[0])
    }

    /// Writes `data` at the given `offset` into the pending overlay.
    ///
    /// The base data is never modified. This records the modification in the
    /// pending layer, which takes priority over committed data and base data
    /// when reading.
    ///
    /// Empty writes (zero-length data) are silently ignored.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if the write extends beyond the base data size.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 100]);
    /// pf.write(50, &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
    ///
    /// let data = pf.read(50, 4).unwrap();
    /// assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    /// ```
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        self.check_bounds(offset, data.len() as u64)?;

        let mut guard = self
            .overlay
            .write()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        guard.write(offset, data);
        Ok(())
    }

    /// Writes a single byte at the given offset into the pending overlay.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if the offset is beyond the file size.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    pub fn write_byte(&self, offset: u64, byte: u8) -> Result<()> {
        self.write(offset, &[byte])
    }

    /// Merges all pending modifications into the committed overlay.
    ///
    /// After this call, the pending layer is empty and the committed layer
    /// contains the consolidated union of all previously committed and pending
    /// modifications. Overlapping regions are resolved with later writes winning.
    ///
    /// Multiple commit cycles are supported:
    /// ```text
    /// modify → commit → modify → commit → ... → to_vec / to_file
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 10]);
    ///
    /// // Pass 1
    /// pf.write(0, &[0xAA]).unwrap();
    /// pf.commit().unwrap();
    /// assert!(!pf.has_pending().unwrap());
    ///
    /// // Pass 2
    /// pf.write(5, &[0xBB]).unwrap();
    /// pf.commit().unwrap();
    ///
    /// let output = pf.to_vec().unwrap();
    /// assert_eq!(output[0], 0xAA);
    /// assert_eq!(output[5], 0xBB);
    /// ```
    pub fn commit(&self) -> Result<()> {
        let mut guard = self
            .overlay
            .write()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        guard.commit();
        Ok(())
    }

    /// Produces a `Vec<u8>` containing the full file with all modifications applied.
    ///
    /// This allocates a new vector of size [`len`](CowFile::len), copies the base data,
    /// then patches in all committed and pending modifications.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
    /// pf.write(0, &[0xFF]).unwrap();
    ///
    /// let output = pf.to_vec().unwrap();
    /// assert_eq!(output, vec![0xFF, 2, 3, 4, 5]);
    /// ```
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        let guard = self
            .overlay
            .read()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        Ok(guard.materialize(self.backend.as_slice()))
    }

    /// Writes the full file with all modifications applied to disk.
    ///
    /// For files smaller than 64 MiB, this uses buffered [`std::fs::File::write_all`].
    /// For larger files, this uses a writable memory map ([`memmap2::MmapMut`]) for
    /// efficient output without requiring the entire file in a contiguous `Vec<u8>`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be created or written.
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 1024]);
    /// pf.write(0, &[0x4D, 0x5A]).unwrap();
    /// pf.to_file("output.bin").unwrap();
    /// ```
    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let guard = self
            .overlay
            .read()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        let base = self.backend.as_slice();
        let size = base.len() as u64;

        if size >= MMAP_WRITE_THRESHOLD {
            self.to_file_mmap(path.as_ref(), base, &guard)
        } else {
            let output = guard.materialize(base);
            let mut file = std::fs::File::create(path.as_ref())?;
            file.write_all(&output)?;
            file.flush()?;
            Ok(())
        }
    }

    /// Reads a primitive value in little-endian byte order at the given offset.
    ///
    /// The number of bytes read equals [`Primitive::SIZE`] for the requested type.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0xEF, 0xBE, 0xAD, 0xDE, 0, 0, 0, 0]);
    /// assert_eq!(pf.read_le::<u32>(0).unwrap(), 0xDEADBEEF);
    /// ```
    pub fn read_le<T: Primitive>(&self, offset: u64) -> Result<T> {
        let data = self.read(offset, T::SIZE as u64)?;
        Ok(T::from_le_bytes(&data))
    }

    /// Reads a primitive value in big-endian byte order at the given offset.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0]);
    /// assert_eq!(pf.read_be::<u32>(0).unwrap(), 0xDEADBEEF);
    /// ```
    pub fn read_be<T: Primitive>(&self, offset: u64) -> Result<T> {
        let data = self.read(offset, T::SIZE as u64)?;
        Ok(T::from_be_bytes(&data))
    }

    /// Writes a primitive value in little-endian byte order at the given offset.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 8]);
    /// pf.write_le::<u32>(0, 0xDEADBEEF).unwrap();
    /// assert_eq!(pf.read(0, 4).unwrap(), vec![0xEF, 0xBE, 0xAD, 0xDE]);
    /// ```
    pub fn write_le<T: Primitive>(&self, offset: u64, value: T) -> Result<()> {
        let mut buf = vec![0u8; T::SIZE];
        value.write_le_bytes(&mut buf);
        self.write(offset, &buf)
    }

    /// Writes a primitive value in big-endian byte order at the given offset.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
    /// - [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 8]);
    /// pf.write_be::<u32>(0, 0xDEADBEEF).unwrap();
    /// assert_eq!(pf.read(0, 4).unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    /// ```
    pub fn write_be<T: Primitive>(&self, offset: u64, value: T) -> Result<()> {
        let mut buf = vec![0u8; T::SIZE];
        value.write_be_bytes(&mut buf);
        self.write(offset, &buf)
    }

    /// Reads a user-defined type implementing [`ReadFrom`] at the given offset.
    ///
    /// This delegates to [`ReadFrom::read_from`], which typically calls
    /// [`read_le`](CowFile::read_le) / [`read_be`](CowFile::read_be) for
    /// individual fields.
    ///
    /// # Errors
    ///
    /// Returns any error produced by the [`ReadFrom`] implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::{CowFile, ReadFrom, Result};
    ///
    /// struct Pair { a: u16, b: u16 }
    ///
    /// impl ReadFrom for Pair {
    ///     fn read_from(pf: &CowFile, offset: u64) -> Result<Self> {
    ///         Ok(Pair {
    ///             a: pf.read_le::<u16>(offset)?,
    ///             b: pf.read_le::<u16>(offset + 2)?,
    ///         })
    ///     }
    /// }
    ///
    /// let pf = CowFile::from_vec(vec![0x01, 0x00, 0x02, 0x00]);
    /// let pair: Pair = pf.read_type(0).unwrap();
    /// assert_eq!(pair.a, 1);
    /// assert_eq!(pair.b, 2);
    /// ```
    pub fn read_type<T: ReadFrom>(&self, offset: u64) -> Result<T> {
        T::read_from(self, offset)
    }

    /// Writes a user-defined type implementing [`WriteTo`] at the given offset.
    ///
    /// This delegates to [`WriteTo::write_to`], which typically calls
    /// [`write_le`](CowFile::write_le) / [`write_be`](CowFile::write_be) for
    /// individual fields.
    ///
    /// # Errors
    ///
    /// Returns any error produced by the [`WriteTo`] implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::{CowFile, WriteTo, Result};
    ///
    /// struct Pair { a: u16, b: u16 }
    ///
    /// impl WriteTo for Pair {
    ///     fn write_to(&self, pf: &CowFile, offset: u64) -> Result<()> {
    ///         pf.write_le::<u16>(offset, self.a)?;
    ///         pf.write_le::<u16>(offset + 2, self.b)?;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 8]);
    /// pf.write_type(0, &Pair { a: 1, b: 2 }).unwrap();
    /// assert_eq!(pf.read(0, 4).unwrap(), vec![0x01, 0x00, 0x02, 0x00]);
    /// ```
    pub fn write_type<T: WriteTo>(&self, offset: u64, value: &T) -> Result<()> {
        value.write_to(self, offset)
    }

    /// Creates a cursor over this `CowFile` at position 0.
    ///
    /// The returned [`CowFileCursor`] implements [`std::io::Read`],
    /// [`std::io::Write`], and [`std::io::Seek`], allowing the `CowFile`
    /// to be used with any API that expects standard I/O traits.
    ///
    /// Multiple cursors can exist over the same `CowFile` simultaneously.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::{Read, Write, Seek, SeekFrom};
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 32]);
    /// let mut cursor = pf.cursor();
    ///
    /// cursor.write_all(&[1, 2, 3, 4]).unwrap();
    /// cursor.seek(SeekFrom::Start(0)).unwrap();
    ///
    /// let mut buf = [0u8; 4];
    /// cursor.read_exact(&mut buf).unwrap();
    /// assert_eq!(buf, [1, 2, 3, 4]);
    /// ```
    pub fn cursor(&self) -> CowFileCursor<'_> {
        CowFileCursor::new(self)
    }

    /// Creates a `CowFile` from an already-opened [`std::fs::File`].
    ///
    /// The file is memory-mapped read-only into the process address space.
    /// This is useful when you already have a file handle from specific
    /// permissions or a special location.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be memory-mapped.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use cowfile::CowFile;
    ///
    /// let file = std::fs::File::open("binary.exe").unwrap();
    /// let pf = CowFile::from_std_file(file).unwrap();
    /// println!("File size: {} bytes", pf.len());
    /// ```
    pub fn from_std_file(file: std::fs::File) -> Result<Self> {
        // SAFETY: The file handle is valid and the mmap is read-only.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(CowFile {
            backend: Backend::Mmap(mmap),
            overlay: RwLock::new(Overlay::new()),
        })
    }

    /// Folds all overlay modifications into the base data **in place**.
    ///
    /// After this call, [`base_data`](CowFile::base_data) returns bytes that
    /// include all previously pending and committed overlay writes. The overlay
    /// is cleared.
    ///
    /// This is useful when a downstream consumer (such as a PE parser) needs
    /// contiguous `&[u8]` that includes modifications.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let mut pf = CowFile::from_vec(vec![0u8; 10]);
    /// pf.write(0, &[0xFF, 0xFE]).unwrap();
    /// pf.consolidate().unwrap();
    ///
    /// // base_data() now includes the overlay writes
    /// assert_eq!(pf.base_data()[0], 0xFF);
    /// assert_eq!(pf.base_data()[1], 0xFE);
    /// assert!(!pf.has_modifications().unwrap());
    /// ```
    pub fn consolidate(&mut self) -> Result<()> {
        let overlay = self
            .overlay
            .get_mut()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        if !overlay.has_modifications() {
            return Ok(());
        }

        let materialized = overlay.materialize(self.backend.as_slice());
        self.backend = Backend::Vec(materialized);
        *overlay = Overlay::new();
        Ok(())
    }

    /// Consumes the `CowFile` and returns the data as an owned `Vec<u8>`.
    ///
    /// If there are no overlay modifications and the backend is a `Vec`,
    /// this is a zero-copy move. Otherwise, the data is materialized.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the overlay lock is poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![1, 2, 3]);
    /// let data = pf.into_vec().unwrap();
    /// assert_eq!(data, vec![1, 2, 3]);
    /// ```
    pub fn into_vec(self) -> Result<Vec<u8>> {
        let overlay = self
            .overlay
            .into_inner()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;

        if !overlay.has_modifications() {
            return match self.backend {
                Backend::Vec(v) => Ok(v),
                Backend::Mmap(m) => Ok(m.as_ref().to_vec()),
            };
        }

        Ok(overlay.materialize(self.backend.as_slice()))
    }

    /// Validates that `[offset, offset + length)` is within the base data bounds.
    fn check_bounds(&self, offset: u64, length: u64) -> Result<()> {
        let end = offset.checked_add(length).ok_or(Error::OutOfBounds {
            offset,
            length,
            file_size: self.backend.len(),
        })?;

        if end > self.backend.len() {
            return Err(Error::OutOfBounds {
                offset,
                length,
                file_size: self.backend.len(),
            });
        }

        Ok(())
    }

    /// Writes to a file using a writable memory map (for large files).
    fn to_file_mmap(&self, path: &Path, base: &[u8], overlay: &Overlay) -> Result<()> {
        let size = base.len() as u64;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(size)?;

        // SAFETY: The file was just created and truncated. We have exclusive write
        // access. The mmap is flushed before being dropped.
        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };
        mmap.copy_from_slice(base);

        // Apply committed layer.
        for (&offset, data) in overlay.committed_entries() {
            let start = offset as usize;
            mmap[start..start + data.len()].copy_from_slice(data);
        }

        // Apply pending layer (overwrites committed where they overlap).
        for (&offset, data) in overlay.pending_entries() {
            let start = offset as usize;
            mmap[start..start + data.len()].copy_from_slice(data);
        }

        mmap.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        traits::{ReadFrom, WriteTo},
        CowFile,
    };

    #[test]
    fn test_from_vec_basic() {
        let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
        assert_eq!(pf.len(), 5);
        assert!(!pf.is_empty());
        assert_eq!(pf.base_data(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_from_vec_empty() {
        let pf = CowFile::from_vec(vec![]);
        assert_eq!(pf.len(), 0);
        assert!(pf.is_empty());
    }

    #[test]
    fn test_from_path_basic() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        tmpfile.flush().unwrap();

        let pf = CowFile::from_path(tmpfile.path()).unwrap();
        assert_eq!(pf.len(), 4);
        assert_eq!(pf.base_data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_from_path_nonexistent() {
        let result = CowFile::from_path("/nonexistent/path.bin");
        assert!(result.is_err());
    }

    #[test]
    fn test_write_and_read() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(2, &[0xFF, 0xFE]).unwrap();

        let data = pf.read(0, 10).unwrap();
        assert_eq!(data[2], 0xFF);
        assert_eq!(data[3], 0xFE);
        assert_eq!(data[0], 0x00);
    }

    #[test]
    fn test_write_byte_and_read_byte() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write_byte(5, 0xAA).unwrap();
        assert_eq!(pf.read_byte(5).unwrap(), 0xAA);
        assert_eq!(pf.read_byte(4).unwrap(), 0x00);
    }

    #[test]
    fn test_write_empty_is_noop() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(5, &[]).unwrap();
        assert!(!pf.has_pending().unwrap());
    }

    #[test]
    fn test_commit_and_read() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xAA]).unwrap();
        assert!(pf.has_pending().unwrap());

        pf.commit().unwrap();
        assert!(!pf.has_pending().unwrap());
        assert!(pf.has_modifications().unwrap());

        assert_eq!(pf.read_byte(0).unwrap(), 0xAA);
    }

    #[test]
    fn test_multi_commit_cycle() {
        let pf = CowFile::from_vec(vec![0u8; 20]);

        // Pass 1
        pf.write(0, &[0xAA]).unwrap();
        pf.write(10, &[0xBB]).unwrap();
        pf.commit().unwrap();

        // Pass 2
        pf.write(5, &[0xCC]).unwrap();
        pf.commit().unwrap();

        let output = pf.to_vec().unwrap();
        assert_eq!(output[0], 0xAA);
        assert_eq!(output[5], 0xCC);
        assert_eq!(output[10], 0xBB);
    }

    #[test]
    fn test_to_vec_no_modifications() {
        let original = vec![1, 2, 3, 4, 5];
        let pf = CowFile::from_vec(original.clone());
        let output = pf.to_vec().unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_to_vec_with_modifications() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xFF]).unwrap();
        pf.write(9, &[0xEE]).unwrap();

        let output = pf.to_vec().unwrap();
        assert_eq!(output[0], 0xFF);
        assert_eq!(output[9], 0xEE);
        assert_eq!(output[5], 0x00);
    }

    #[test]
    fn test_to_file_and_read_back() {
        let pf = CowFile::from_vec(vec![0u8; 100]);
        pf.write(0, &[0x4D, 0x5A]).unwrap();
        pf.write(50, &[0xDE, 0xAD]).unwrap();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        pf.to_file(tmpfile.path()).unwrap();

        let contents = std::fs::read(tmpfile.path()).unwrap();
        assert_eq!(contents.len(), 100);
        assert_eq!(contents[0], 0x4D);
        assert_eq!(contents[1], 0x5A);
        assert_eq!(contents[50], 0xDE);
        assert_eq!(contents[51], 0xAD);
        assert_eq!(contents[10], 0x00);
    }

    #[test]
    fn test_out_of_bounds_read() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        let result = pf.read(8, 5);
        assert!(result.is_err());
    }

    #[test]
    fn test_out_of_bounds_write() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        let result = pf.write(8, &[0xFF; 5]);
        assert!(result.is_err());
    }

    #[test]
    fn test_out_of_bounds_at_exact_end() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        // Reading 0 bytes at offset 10 is within bounds (empty read).
        let result = pf.read(10, 0);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_has_pending_and_modifications() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        assert!(!pf.has_pending().unwrap());
        assert!(!pf.has_modifications().unwrap());

        pf.write(0, &[0xFF]).unwrap();
        assert!(pf.has_pending().unwrap());
        assert!(pf.has_modifications().unwrap());

        pf.commit().unwrap();
        assert!(!pf.has_pending().unwrap());
        assert!(pf.has_modifications().unwrap());
    }

    #[test]
    fn test_base_data_unchanged_after_writes() {
        let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
        pf.write(0, &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        pf.commit().unwrap();

        assert_eq!(pf.base_data(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let pf = Arc::new(CowFile::from_vec(vec![0xAA; 1000]));
        pf.write(500, &[0xFF; 100]).unwrap();

        let mut handles = vec![];
        for _ in 0..8 {
            let pf = Arc::clone(&pf);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let data = pf.read(0, 1000).unwrap();
                    assert_eq!(data[0], 0xAA);
                    assert_eq!(data[500], 0xFF);
                    assert_eq!(data[999], 0xAA);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_write_then_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let pf = Arc::new(CowFile::from_vec(vec![0u8; 100]));

        // Write from the main thread.
        for i in 0..100u8 {
            pf.write_byte(i as u64, i).unwrap();
        }
        pf.commit().unwrap();

        // Concurrent reads should all see the committed data.
        let mut handles = vec![];
        for _ in 0..4 {
            let pf = Arc::clone(&pf);
            handles.push(thread::spawn(move || {
                let data = pf.to_vec().unwrap();
                for (i, &byte) in data.iter().enumerate() {
                    assert_eq!(byte, i as u8);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_writes() {
        use std::sync::Arc;
        use std::thread;

        let pf = Arc::new(CowFile::from_vec(vec![0u8; 1000]));

        let mut handles = vec![];
        for t in 0..4u8 {
            let pf = Arc::clone(&pf);
            handles.push(thread::spawn(move || {
                let base = t as u64 * 250;
                for i in 0..250u64 {
                    pf.write_byte(base + i, t).unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Each quarter should be written by one thread.
        let data = pf.to_vec().unwrap();
        for t in 0..4u8 {
            let base = t as usize * 250;
            for i in 0..250 {
                assert_eq!(data[base + i], t);
            }
        }
    }

    #[test]
    fn test_send_sync_static_assertion() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CowFile>();
    }

    #[test]
    fn test_read_write_le_u16() {
        let pf = CowFile::from_vec(vec![0u8; 16]);
        pf.write_le::<u16>(0, 0xCAFE).unwrap();
        assert_eq!(pf.read_le::<u16>(0).unwrap(), 0xCAFE);
        // Verify byte order.
        assert_eq!(pf.read(0, 2).unwrap(), vec![0xFE, 0xCA]);
    }

    #[test]
    fn test_read_write_le_u32() {
        let pf = CowFile::from_vec(vec![0u8; 16]);
        pf.write_le::<u32>(4, 0xDEADBEEF).unwrap();
        assert_eq!(pf.read_le::<u32>(4).unwrap(), 0xDEADBEEF);
    }

    #[test]
    fn test_read_write_le_u64() {
        let pf = CowFile::from_vec(vec![0u8; 16]);
        pf.write_le::<u64>(0, 0x0123456789ABCDEF).unwrap();
        assert_eq!(pf.read_le::<u64>(0).unwrap(), 0x0123456789ABCDEF);
    }

    #[test]
    fn test_read_write_be_u32() {
        let pf = CowFile::from_vec(vec![0u8; 16]);
        pf.write_be::<u32>(0, 0xDEADBEEF).unwrap();
        assert_eq!(pf.read_be::<u32>(0).unwrap(), 0xDEADBEEF);
        // BE: most significant byte first.
        assert_eq!(pf.read(0, 4).unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_read_le_out_of_bounds() {
        let pf = CowFile::from_vec(vec![0u8; 3]);
        let result = pf.read_le::<u32>(0);
        assert!(result.is_err());
    }

    #[test]
    fn test_write_le_out_of_bounds() {
        let pf = CowFile::from_vec(vec![0u8; 3]);
        let result = pf.write_le::<u32>(0, 42);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_write_type() {
        struct TestStruct {
            magic: u32,
            version: u16,
            flags: u8,
        }

        impl ReadFrom for TestStruct {
            fn read_from(pf: &CowFile, offset: u64) -> crate::Result<Self> {
                Ok(TestStruct {
                    magic: pf.read_le::<u32>(offset)?,
                    version: pf.read_le::<u16>(offset + 4)?,
                    flags: pf.read_le::<u8>(offset + 6)?,
                })
            }
        }

        impl WriteTo for TestStruct {
            fn write_to(&self, pf: &CowFile, offset: u64) -> crate::Result<()> {
                pf.write_le::<u32>(offset, self.magic)?;
                pf.write_le::<u16>(offset + 4, self.version)?;
                pf.write_le::<u8>(offset + 6, self.flags)?;
                Ok(())
            }
        }

        let pf = CowFile::from_vec(vec![0u8; 16]);
        let s = TestStruct {
            magic: 0x4D5A9000,
            version: 3,
            flags: 0xFF,
        };

        pf.write_type(0, &s).unwrap();
        let read_back: TestStruct = pf.read_type(0).unwrap();
        assert_eq!(read_back.magic, 0x4D5A9000);
        assert_eq!(read_back.version, 3);
        assert_eq!(read_back.flags, 0xFF);
    }

    #[test]
    fn test_from_std_file() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        tmpfile.flush().unwrap();

        let std_file = std::fs::File::open(tmpfile.path()).unwrap();
        let pf = CowFile::from_std_file(std_file).unwrap();
        assert_eq!(pf.len(), 4);
        assert_eq!(pf.base_data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_consolidate_no_modifications() {
        let mut pf = CowFile::from_vec(vec![1, 2, 3]);
        pf.consolidate().unwrap();
        assert_eq!(pf.base_data(), &[1, 2, 3]);
        assert!(!pf.has_modifications().unwrap());
    }

    #[test]
    fn test_consolidate_with_pending() {
        let mut pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xFF, 0xFE]).unwrap();
        pf.consolidate().unwrap();

        assert_eq!(pf.base_data()[0], 0xFF);
        assert_eq!(pf.base_data()[1], 0xFE);
        assert_eq!(pf.base_data()[2], 0x00);
        assert!(!pf.has_modifications().unwrap());
    }

    #[test]
    fn test_consolidate_with_committed_and_pending() {
        let mut pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xAA]).unwrap();
        pf.commit().unwrap();
        pf.write(5, &[0xBB]).unwrap();
        pf.consolidate().unwrap();

        assert_eq!(pf.base_data()[0], 0xAA);
        assert_eq!(pf.base_data()[5], 0xBB);
        assert!(!pf.has_modifications().unwrap());
        assert!(!pf.has_pending().unwrap());
    }

    #[test]
    fn test_consolidate_then_write() {
        let mut pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xFF]).unwrap();
        pf.consolidate().unwrap();

        // New writes after consolidation should work
        pf.write(5, &[0xBB]).unwrap();
        assert_eq!(pf.read_byte(0).unwrap(), 0xFF); // from consolidated base
        assert_eq!(pf.read_byte(5).unwrap(), 0xBB); // from new pending
    }

    #[test]
    fn test_into_vec_no_modifications() {
        let pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
        let data = pf.into_vec().unwrap();
        assert_eq!(data, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_into_vec_with_modifications() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xFF]).unwrap();
        pf.write(9, &[0xEE]).unwrap();
        let data = pf.into_vec().unwrap();
        assert_eq!(data[0], 0xFF);
        assert_eq!(data[9], 0xEE);
        assert_eq!(data[5], 0x00);
    }

    #[test]
    fn test_into_vec_from_mmap() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        tmpfile.flush().unwrap();

        let pf = CowFile::from_path(tmpfile.path()).unwrap();
        let data = pf.into_vec().unwrap();
        assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_into_vec_from_mmap_with_modifications() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0x00, 0x00, 0x00, 0x00]).unwrap();
        tmpfile.flush().unwrap();

        let pf = CowFile::from_path(tmpfile.path()).unwrap();
        pf.write(0, &[0xFF]).unwrap();
        let data = pf.into_vec().unwrap();
        assert_eq!(data, vec![0xFF, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_cursor_basic() {
        use std::io::{Read, Seek, SeekFrom, Write};

        let pf = CowFile::from_vec(vec![0u8; 32]);
        let mut cursor = pf.cursor();

        cursor.write_all(&[0xAA, 0xBB, 0xCC]).unwrap();
        cursor.seek(SeekFrom::Start(0)).unwrap();

        let mut buf = [0u8; 3];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0xAA, 0xBB, 0xCC]);
    }
}
