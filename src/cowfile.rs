//! The main [`CowFile`] type with OS-level copy-on-write and a pending write log.
//!
//! `CowFile` wraps binary data (from a [`Vec<u8>`] or a copy-on-write memory map)
//! with a pending log that tracks writes. The committed buffer is accessible as
//! `&[u8]` via [`data`](CowFile::data), while [`read`](CowFile::read) and typed
//! accessors composite pending writes over the committed state.
//!
//! # Thread Safety
//!
//! `CowFile` is [`Send`] and [`Sync`]. The committed buffer can be read
//! concurrently via [`data`](CowFile::data) from multiple threads. Writes
//! to the pending log are serialised by an internal [`RwLock`](std::sync::RwLock).

use std::{
    fmt,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        RwLock,
    },
};

use crate::{
    cursor::CowFileCursor,
    error::{Error, Result},
    primitives::Primitive,
    traits::{ReadFrom, WriteTo},
};

/// Threshold above which [`to_file`](CowFile::to_file) uses a writable memory map
/// instead of buffered I/O. Set to 64 MiB.
const MMAP_WRITE_THRESHOLD: usize = 64 * 1024 * 1024;

/// Inner storage for `CowFile`.
enum Inner {
    /// Owned byte vector, directly mutable.
    Vec(Vec<u8>),
    /// Copy-on-write memory map (`MAP_PRIVATE`). Writes are process-private.
    Mmap(memmap2::MmapMut),
}

impl Inner {
    fn as_slice(&self) -> &[u8] {
        match self {
            Inner::Vec(v) => v.as_slice(),
            Inner::Mmap(m) => m,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Inner::Vec(v) => v.as_mut_slice(),
            Inner::Mmap(m) => m.as_mut(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Inner::Vec(v) => v.len(),
            Inner::Mmap(m) => m.len(),
        }
    }
}

/// A single pending write recorded in the log.
struct PendingWrite {
    offset: usize,
    data: Vec<u8>,
}

/// A copy-on-write file abstraction backed by memory or a file.
///
/// Writes accumulate in a pending log and are applied to the committed buffer
/// on [`commit`](CowFile::commit). The committed buffer is accessible as
/// `&[u8]` via [`data`](CowFile::data), while [`read`](CowFile::read) and
/// typed I/O methods composite pending writes over the committed state.
///
/// # Architecture
///
/// ```text
///  Committed Buffer               Pending Log
/// +---------------------+      +-------------------------+
/// | Vec<u8> or MmapMut  | <--- | Vec<PendingWrite>       |
/// | (OS-level CoW)      |      | (applied on commit)     |
/// +---------------------+      +-------------------------+
/// ```
///
/// For memory-mapped files, the buffer is created with
/// [`map_copy`](memmap2::MmapOptions::map_copy), which uses `MAP_PRIVATE` on
/// Unix and `PAGE_WRITECOPY` on Windows. Only pages touched by
/// [`commit`](CowFile::commit) are copied into anonymous memory — the rest
/// of the file remains demand-paged from disk.
///
/// # Examples
///
/// ```
/// use cowfile::CowFile;
///
/// let pf = CowFile::from_vec(vec![0u8; 100]);
///
/// // Writes go to the pending log
/// pf.write(10, &[0xFF, 0xFE]).unwrap();
///
/// // data() returns committed state
/// assert_eq!(pf.data()[10], 0x00);
///
/// // read() composites pending writes
/// assert_eq!(pf.read_byte(10).unwrap(), 0xFF);
///
/// // Commit applies pending to the buffer
/// let mut pf = pf;
/// pf.commit().unwrap();
/// assert_eq!(pf.data()[10], 0xFF);
/// ```
pub struct CowFile {
    /// Committed buffer — only mutated by `commit()`.
    buffer: Inner,
    /// Pending writes, accumulated via interior mutability.
    pending: RwLock<Vec<PendingWrite>>,
    /// Fast check to skip empty pending iteration.
    dirty: AtomicBool,
    /// Original file path (set by `open()`, `None` for vec-backed).
    source_path: Option<PathBuf>,
}

// Static assertion: CowFile must be Send + Sync.
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<CowFile>();
    }
    let _ = check;
};

impl fmt::Debug for CowFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CowFile")
            .field("len", &self.buffer.len())
            .field(
                "backend",
                &match &self.buffer {
                    Inner::Vec(_) => "Vec",
                    Inner::Mmap(_) => "Mmap",
                },
            )
            .field("dirty", &self.dirty.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl CowFile {
    /// Creates a `CowFile` from an owned byte vector.
    ///
    /// The provided bytes become the committed buffer. No copies are made
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
            buffer: Inner::Vec(data),
            pending: RwLock::new(Vec::new()),
            dirty: AtomicBool::new(false),
            source_path: None,
        }
    }

    /// Creates a `CowFile` by memory-mapping a file from the given path.
    ///
    /// The file is mapped with copy-on-write semantics (`MAP_PRIVATE` on Unix,
    /// `PAGE_WRITECOPY` on Windows). The original file is never modified.
    /// Only pages touched by [`commit`](CowFile::commit) are copied into
    /// anonymous memory — the rest of the file remains demand-paged from disk.
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
    /// let pf = CowFile::open("binary.exe").unwrap();
    /// println!("File size: {} bytes", pf.len());
    /// ```
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let mut cow = Self::from_file(file)?;
        cow.source_path = Some(path.to_path_buf());
        Ok(cow)
    }

    /// Creates a `CowFile` from an already-opened [`std::fs::File`].
    ///
    /// The file is mapped with copy-on-write semantics. The original file is
    /// never modified.
    ///
    /// Empty files (0 bytes) are handled by using a `Vec` backend instead of
    /// mmap, since memory-mapping an empty file is not supported on all
    /// platforms.
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
    /// let pf = CowFile::from_file(file).unwrap();
    /// println!("File size: {} bytes", pf.len());
    /// ```
    pub fn from_file(file: std::fs::File) -> Result<Self> {
        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            return Ok(Self::from_vec(Vec::new()));
        }

        // SAFETY: We use map_copy which creates a private CoW mapping.
        // The file must not be modified externally while the mapping is alive.
        // This is the same contract as any memory-mapped file in Rust.
        let mmap = unsafe { memmap2::MmapOptions::new().map_copy(&file)? };
        Ok(CowFile {
            buffer: Inner::Mmap(mmap),
            pending: RwLock::new(Vec::new()),
            dirty: AtomicBool::new(false),
            source_path: None,
        })
    }

    /// Returns the committed buffer as a byte slice.
    ///
    /// This is a true zero-cost `&[u8]` reference into the committed buffer.
    /// For mmap-backed files, only accessed pages are loaded into physical
    /// memory by the OS.
    ///
    /// Pending writes are **not** visible through this method. Use
    /// [`read`](CowFile::read) or [`read_le`](CowFile::read_le) for a view
    /// that composites pending writes, or call [`commit`](CowFile::commit)
    /// first.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![1, 2, 3]);
    /// let data: &[u8] = pf.data();
    /// assert_eq!(data, &[1, 2, 3]);
    /// ```
    pub fn data(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    /// Returns the total length of the data in bytes.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if the data is empty (zero bytes).
    pub fn is_empty(&self) -> bool {
        self.buffer.len() == 0
    }

    /// Returns `true` if there are uncommitted pending writes.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 10]);
    /// assert!(!pf.has_pending());
    ///
    /// pf.write(0, &[0xFF]).unwrap();
    /// assert!(pf.has_pending());
    /// ```
    pub fn has_pending(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Reads `length` bytes starting at `offset`, compositing pending writes.
    ///
    /// The returned bytes reflect pending writes applied over the committed
    /// buffer. When there are no pending writes, this is equivalent to
    /// slicing [`data`](CowFile::data).
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if the requested range exceeds the data size.
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
    pub fn read(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        self.check_bounds(offset, length)?;

        if length == 0 {
            return Ok(Vec::new());
        }

        let mut buf = self.buffer.as_slice()[offset..offset + length].to_vec();

        if self.dirty.load(Ordering::Relaxed) {
            let pending = self
                .pending
                .read()
                .map_err(|e| Error::LockPoisoned(e.to_string()))?;
            apply_pending(&mut buf, offset, length, &pending);
        }

        Ok(buf)
    }

    /// Reads a single byte at the given offset, compositing pending writes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if the offset is beyond the data size.
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
    pub fn read_byte(&self, offset: usize) -> Result<u8> {
        self.check_bounds(offset, 1)?;

        if self.dirty.load(Ordering::Relaxed) {
            let pending = self
                .pending
                .read()
                .map_err(|e| Error::LockPoisoned(e.to_string()))?;
            // Scan in reverse — last write wins.
            for pw in pending.iter().rev() {
                let pw_end = pw.offset + pw.data.len();
                if offset >= pw.offset && offset < pw_end {
                    return Ok(pw.data[offset - pw.offset]);
                }
            }
        }

        Ok(self.buffer.as_slice()[offset])
    }

    /// Writes `data` at the given `offset` into the pending log.
    ///
    /// The committed buffer is not modified. Pending writes are composited
    /// into reads via [`read`](CowFile::read) and applied to the buffer on
    /// [`commit`](CowFile::commit).
    ///
    /// Empty writes (zero-length data) are silently ignored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if the write extends beyond the data size.
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
    pub fn write(&self, offset: usize, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        self.check_bounds(offset, data.len())?;

        self.pending
            .write()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?
            .push(PendingWrite {
                offset,
                data: data.to_vec(),
            });
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Writes a single byte at the given offset into the pending log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if the offset is beyond the data size.
    pub fn write_byte(&self, offset: usize, byte: u8) -> Result<()> {
        self.write(offset, &[byte])
    }

    /// Applies all pending writes to the committed buffer and clears the log.
    ///
    /// For mmap-backed files, only the OS pages touched by writes are copied
    /// into anonymous memory (`MAP_PRIVATE` CoW). The rest of the file remains
    /// demand-paged from disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if any pending write is out of bounds
    /// (should not happen if writes were bounds-checked).
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let mut pf = CowFile::from_vec(vec![0u8; 10]);
    ///
    /// pf.write(0, &[0xAA]).unwrap();
    /// assert_eq!(pf.data()[0], 0x00); // Not yet committed
    ///
    /// pf.commit().unwrap();
    /// assert_eq!(pf.data()[0], 0xAA); // Now committed
    /// assert!(!pf.has_pending());
    /// ```
    pub fn commit(&mut self) -> Result<()> {
        if !*self.dirty.get_mut() {
            return Ok(());
        }

        let pending = self
            .pending
            .get_mut()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;
        let buf = self.buffer.as_mut_slice();

        for pw in pending.drain(..) {
            buf[pw.offset..pw.offset + pw.data.len()].copy_from_slice(&pw.data);
        }

        *self.dirty.get_mut() = false;
        Ok(())
    }

    /// Discards all pending writes without applying them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the internal lock was poisoned.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let mut pf = CowFile::from_vec(vec![0u8; 10]);
    /// pf.write(0, &[0xFF]).unwrap();
    /// assert!(pf.has_pending());
    ///
    /// pf.discard().unwrap();
    /// assert!(!pf.has_pending());
    /// assert_eq!(pf.data()[0], 0x00);
    /// ```
    pub fn discard(&mut self) -> Result<()> {
        self.pending
            .get_mut()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?
            .clear();
        *self.dirty.get_mut() = false;
        Ok(())
    }

    /// Reads a primitive value in little-endian byte order at the given offset.
    ///
    /// Composites pending writes over the committed state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0xEF, 0xBE, 0xAD, 0xDE, 0, 0, 0, 0]);
    /// assert_eq!(pf.read_le::<u32>(0).unwrap(), 0xDEADBEEF);
    /// ```
    pub fn read_le<T: Primitive>(&self, offset: usize) -> Result<T> {
        let data = self.read(offset, T::SIZE)?;
        Ok(T::from_le_bytes(&data))
    }

    /// Reads a primitive value in big-endian byte order at the given offset.
    ///
    /// Composites pending writes over the committed state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
    ///
    /// # Examples
    ///
    /// ```
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::from_vec(vec![0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0]);
    /// assert_eq!(pf.read_be::<u32>(0).unwrap(), 0xDEADBEEF);
    /// ```
    pub fn read_be<T: Primitive>(&self, offset: usize) -> Result<T> {
        let data = self.read(offset, T::SIZE)?;
        Ok(T::from_be_bytes(&data))
    }

    /// Writes a primitive value in little-endian byte order at the given offset.
    ///
    /// The write goes to the pending log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
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
    pub fn write_le<T: Primitive>(&self, offset: usize, value: T) -> Result<()> {
        let mut buf = vec![0u8; T::SIZE];
        value.write_le_bytes(&mut buf);
        self.write(offset, &buf)
    }

    /// Writes a primitive value in big-endian byte order at the given offset.
    ///
    /// The write goes to the pending log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfBounds`] if there are not enough bytes at `offset`.
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
    pub fn write_be<T: Primitive>(&self, offset: usize, value: T) -> Result<()> {
        let mut buf = vec![0u8; T::SIZE];
        value.write_be_bytes(&mut buf);
        self.write(offset, &buf)
    }

    /// Reads a user-defined type implementing [`ReadFrom`] at the given offset.
    ///
    /// Composites pending writes over the committed state.
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
    ///     fn read_from(pf: &CowFile, offset: usize) -> Result<Self> {
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
    pub fn read_type<T: ReadFrom>(&self, offset: usize) -> Result<T> {
        T::read_from(self, offset)
    }

    /// Writes a user-defined type implementing [`WriteTo`] at the given offset.
    ///
    /// The write goes to the pending log.
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
    ///     fn write_to(&self, pf: &CowFile, offset: usize) -> Result<()> {
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
    pub fn write_type<T: WriteTo>(&self, offset: usize, value: &T) -> Result<()> {
        value.write_to(self, offset)
    }

    /// Creates a cursor over this `CowFile` at position 0.
    ///
    /// The returned [`CowFileCursor`] implements [`std::io::Read`],
    /// [`std::io::Write`], and [`std::io::Seek`], allowing the `CowFile`
    /// to be used with any API that expects standard I/O traits.
    ///
    /// Multiple cursors can exist over the same `CowFile` simultaneously,
    /// each with its own independent position.
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

    /// Returns the original file path for mmap-backed instances opened via [`open`](CowFile::open).
    ///
    /// Returns `None` for vec-backed instances or those created via [`from_file`](CowFile::from_file).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::open("binary.exe").unwrap();
    /// assert!(pf.source_path().is_some());
    ///
    /// let pf = CowFile::from_vec(vec![0u8; 10]);
    /// assert!(pf.source_path().is_none());
    /// ```
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// Creates an independent copy of this `CowFile`.
    ///
    /// For mmap-backed files with a known source path, re-opens the original
    /// file — a new `MAP_PRIVATE` mmap that shares physical read pages with
    /// the parent via OS-level copy-on-write. For vec-backed files or those
    /// without a source path, clones the data.
    ///
    /// Pending writes are **not** carried over — the fork starts clean.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the source file cannot be reopened.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use cowfile::CowFile;
    ///
    /// let pf = CowFile::open("binary.exe").unwrap();
    /// pf.write(0, &[0xFF]).unwrap();
    ///
    /// let forked = pf.fork().unwrap();
    /// // Fork starts clean — no pending writes
    /// assert!(!forked.has_pending());
    /// // But reads the same committed data
    /// assert_eq!(forked.data()[0], pf.data()[0]);
    /// ```
    pub fn fork(&self) -> Result<CowFile> {
        match &self.source_path {
            Some(path) => CowFile::open(path),
            None => Ok(CowFile::from_vec(self.buffer.as_slice().to_vec())),
        }
    }

    /// Produces a `Vec<u8>` with all pending writes composited over the
    /// committed buffer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the internal lock was poisoned.
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
        let mut output = self.buffer.as_slice().to_vec();

        if self.dirty.load(Ordering::Relaxed) {
            let pending = self
                .pending
                .read()
                .map_err(|e| Error::LockPoisoned(e.to_string()))?;
            for pw in pending.iter() {
                output[pw.offset..pw.offset + pw.data.len()].copy_from_slice(&pw.data);
            }
        }

        Ok(output)
    }

    /// Writes the data with all pending writes applied to disk.
    ///
    /// For files smaller than 64 MiB, this uses buffered I/O. For larger
    /// files, this uses a writable memory map for efficient output.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be created or written.
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
        let size = self.buffer.len();

        if size >= MMAP_WRITE_THRESHOLD {
            self.to_file_mmap(path.as_ref())
        } else {
            let output = self.to_vec()?;
            let mut file = std::fs::File::create(path.as_ref())?;
            file.write_all(&output)?;
            file.flush()?;
            Ok(())
        }
    }

    /// Consumes the `CowFile` and returns the data as an owned `Vec<u8>`.
    ///
    /// If there are no pending writes and the backend is a `Vec`, this is a
    /// zero-copy move. Otherwise, the data is materialized with pending writes
    /// applied.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockPoisoned`] if the internal lock was poisoned.
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
        let dirty = self.dirty.load(Ordering::Relaxed);

        if !dirty {
            return Ok(match self.buffer {
                Inner::Vec(v) => v,
                Inner::Mmap(m) => m.as_ref().to_vec(),
            });
        }

        let pending = self
            .pending
            .into_inner()
            .map_err(|e| Error::LockPoisoned(e.to_string()))?;
        let mut output = match self.buffer {
            Inner::Vec(v) => v,
            Inner::Mmap(m) => m.as_ref().to_vec(),
        };

        for pw in pending {
            output[pw.offset..pw.offset + pw.data.len()].copy_from_slice(&pw.data);
        }

        Ok(output)
    }

    /// Validates that `[offset, offset + length)` is within bounds.
    fn check_bounds(&self, offset: usize, length: usize) -> Result<()> {
        let end = offset.checked_add(length).ok_or(Error::OutOfBounds {
            offset,
            length,
            file_size: self.buffer.len(),
        })?;

        if end > self.buffer.len() {
            return Err(Error::OutOfBounds {
                offset,
                length,
                file_size: self.buffer.len(),
            });
        }

        Ok(())
    }

    /// Writes to a file using a writable memory map (for large files).
    fn to_file_mmap(&self, path: &Path) -> Result<()> {
        let base = self.buffer.as_slice();
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

        if self.dirty.load(Ordering::Relaxed) {
            let pending = self
                .pending
                .read()
                .map_err(|e| Error::LockPoisoned(e.to_string()))?;
            for pw in pending.iter() {
                mmap[pw.offset..pw.offset + pw.data.len()].copy_from_slice(&pw.data);
            }
        }

        mmap.flush()?;
        Ok(())
    }
}

/// Applies pending writes that overlap `[read_offset..read_offset+read_len)` to `buf`.
///
/// Writes are applied in order — later writes overwrite earlier ones.
fn apply_pending(buf: &mut [u8], read_offset: usize, read_len: usize, pending: &[PendingWrite]) {
    let read_end = read_offset + read_len;
    for pw in pending {
        let pw_end = pw.offset + pw.data.len();
        // Check for overlap.
        if pw.offset < read_end && pw_end > read_offset {
            let start = pw.offset.max(read_offset);
            let end = pw_end.min(read_end);
            let buf_start = start - read_offset;
            let pw_start = start - pw.offset;
            buf[buf_start..buf_start + (end - start)]
                .copy_from_slice(&pw.data[pw_start..pw_start + (end - start)]);
        }
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
        assert_eq!(pf.data(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_from_vec_empty() {
        let pf = CowFile::from_vec(vec![]);
        assert_eq!(pf.len(), 0);
        assert!(pf.is_empty());
    }

    #[test]
    fn test_open_basic() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        tmpfile.flush().unwrap();

        let pf = CowFile::open(tmpfile.path()).unwrap();
        assert_eq!(pf.len(), 4);
        assert_eq!(pf.data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_open_nonexistent() {
        let result = CowFile::open("/nonexistent/path.bin");
        assert!(result.is_err());
    }

    #[test]
    fn test_write_and_read() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(2, &[0xFF, 0xFE]).unwrap();

        // data() shows committed state.
        assert_eq!(pf.data()[2], 0x00);

        // read() composites pending.
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
        assert!(!pf.has_pending());
    }

    #[test]
    fn test_commit_and_read() {
        let mut pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xAA]).unwrap();
        assert!(pf.has_pending());

        pf.commit().unwrap();
        assert!(!pf.has_pending());
        assert_eq!(pf.data()[0], 0xAA);
    }

    #[test]
    fn test_multi_commit_cycle() {
        let mut pf = CowFile::from_vec(vec![0u8; 20]);

        // Pass 1.
        pf.write(0, &[0xAA]).unwrap();
        pf.write(10, &[0xBB]).unwrap();
        pf.commit().unwrap();

        // Pass 2.
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
        let result = pf.read(10, 0);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_has_pending() {
        let mut pf = CowFile::from_vec(vec![0u8; 10]);
        assert!(!pf.has_pending());

        pf.write(0, &[0xFF]).unwrap();
        assert!(pf.has_pending());

        pf.commit().unwrap();
        assert!(!pf.has_pending());
    }

    #[test]
    fn test_data_shows_committed_state() {
        let mut pf = CowFile::from_vec(vec![1, 2, 3, 4, 5]);
        pf.write(0, &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap();

        // data() should still show original.
        assert_eq!(pf.data(), &[1, 2, 3, 4, 5]);

        pf.commit().unwrap();

        // After commit, data() shows the changes.
        assert_eq!(pf.data(), &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_data_while_writing() {
        let pf = CowFile::from_vec(vec![0u8; 100]);

        // Hold a reference to data() while writing.
        let view = pf.data();
        pf.write(10, &[0xFF]).unwrap();

        // View still shows committed state.
        assert_eq!(view[10], 0x00);

        // But read_byte composites pending.
        assert_eq!(pf.read_byte(10).unwrap(), 0xFF);
    }

    #[test]
    fn test_discard() {
        let mut pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write(0, &[0xFF]).unwrap();
        assert!(pf.has_pending());

        pf.discard().unwrap();
        assert!(!pf.has_pending());
        assert_eq!(pf.read_byte(0).unwrap(), 0x00);
    }

    #[test]
    fn test_send_static_assertion() {
        fn assert_send<T: Send>() {}
        assert_send::<CowFile>();
    }

    #[test]
    fn test_read_write_le_u16() {
        let pf = CowFile::from_vec(vec![0u8; 16]);
        pf.write_le::<u16>(0, 0xCAFE).unwrap();
        assert_eq!(pf.read_le::<u16>(0).unwrap(), 0xCAFE);
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
            fn read_from(pf: &CowFile, offset: usize) -> crate::Result<Self> {
                Ok(TestStruct {
                    magic: pf.read_le::<u32>(offset)?,
                    version: pf.read_le::<u16>(offset + 4)?,
                    flags: pf.read_le::<u8>(offset + 6)?,
                })
            }
        }

        impl WriteTo for TestStruct {
            fn write_to(&self, pf: &CowFile, offset: usize) -> crate::Result<()> {
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
    fn test_from_file() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        tmpfile.flush().unwrap();

        let std_file = std::fs::File::open(tmpfile.path()).unwrap();
        let pf = CowFile::from_file(std_file).unwrap();
        assert_eq!(pf.len(), 4);
        assert_eq!(pf.data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
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

        let pf = CowFile::open(tmpfile.path()).unwrap();
        let data = pf.into_vec().unwrap();
        assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_into_vec_from_mmap_with_modifications() {
        use std::io::Write;
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        tmpfile.write_all(&[0x00, 0x00, 0x00, 0x00]).unwrap();
        tmpfile.flush().unwrap();

        let pf = CowFile::open(tmpfile.path()).unwrap();
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

    #[test]
    fn test_overlapping_pending_writes() {
        let pf = CowFile::from_vec(vec![0u8; 20]);

        pf.write(0, &[0xAA; 10]).unwrap();
        pf.write(5, &[0xBB; 10]).unwrap();

        let data = pf.read(0, 20).unwrap();
        assert!(data[..5].iter().all(|&b| b == 0xAA));
        assert!(data[5..15].iter().all(|&b| b == 0xBB));
        assert!(data[15..20].iter().all(|&b| b == 0x00));
    }

    #[test]
    fn test_read_byte_pending_last_wins() {
        let pf = CowFile::from_vec(vec![0u8; 10]);
        pf.write_byte(5, 0xAA).unwrap();
        pf.write_byte(5, 0xBB).unwrap();
        assert_eq!(pf.read_byte(5).unwrap(), 0xBB);
    }
}
