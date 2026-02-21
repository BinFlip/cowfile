//! # cowfile
//!
//! A copy-on-write overlay layer for immutable binary data.
//!
//! `cowfile` provides [`CowFile`], a type that wraps immutable binary data with a sparse
//! copy-on-write overlay. Modifications are tracked as byte-range patches without ever
//! mutating the original data. A final merged output is produced only when explicitly
//! requested via [`CowFile::to_vec`] or [`CowFile::to_file`].
//!
//! ## Use Case
//!
//! This crate is designed for binary analysis and transformation pipelines where multiple
//! passes modify a binary (e.g., deobfuscation, patching) without needing to copy the
//! entire file between each pass. Only modified byte ranges are stored in the overlay,
//! keeping memory usage proportional to the number of changes rather than the file size.
//!
//! ## Architecture
//!
//! ```text
//!  Base Layer (immutable)        Overlay (copy-on-write)
//! +---------------------+      +-------------------------+
//! | Vec<u8> or Mmap     |      | committed: BTreeMap     |
//! | (never modified)    | <--- | pending:   BTreeMap     |
//! +---------------------+      +-------------------------+
//! ```
//!
//! - **Base layer**: Immutable data from a `Vec<u8>` or a memory-mapped file.
//! - **Pending overlay**: Uncommitted modifications from the current pass.
//! - **Committed overlay**: Consolidated modifications from previous [`commit`](CowFile::commit) calls.
//!
//! When reading, layers are composited with priority: **pending > committed > base**.
//!
//! ## Thread Safety
//!
//! [`CowFile`] is [`Send`] + [`Sync`]. The immutable base layer requires no synchronization.
//! The overlay is protected by an internal [`RwLock`](std::sync::RwLock) that allows
//! concurrent reads with exclusive writes.
//!
//! ## Quick Start
//!
//! ```
//! use cowfile::CowFile;
//!
//! // Create from owned bytes
//! let pf = CowFile::from_vec(vec![0u8; 1024]);
//!
//! // Pass 1: Apply modifications
//! pf.write(0x10, &[0xFF, 0xFE]).unwrap();
//! pf.write(0x20, &[0xAA, 0xBB, 0xCC]).unwrap();
//! pf.commit().unwrap();
//!
//! // Pass 2: More modifications
//! pf.write(0x30, &[0xDD]).unwrap();
//! pf.commit().unwrap();
//!
//! // Produce final output
//! let output = pf.to_vec().unwrap();
//! assert_eq!(output[0x10], 0xFF);
//! assert_eq!(output[0x20], 0xAA);
//! assert_eq!(output[0x30], 0xDD);
//! ```
//!
//! ## Memory-Mapped Files
//!
//! For large binaries, use [`CowFile::from_path`] to memory-map the file. The operating
//! system handles paging, so only accessed regions are loaded into physical memory:
//!
//! ```no_run
//! use cowfile::CowFile;
//!
//! let pf = CowFile::from_path("large_binary.exe").unwrap();
//! pf.write(0, &[0x4D, 0x5A]).unwrap(); // Patch MZ header
//! pf.to_file("patched.exe").unwrap();   // Write output
//! ```

#![deny(missing_docs)]

mod backend;
mod cowfile;
mod cursor;
mod error;
mod overlay;
mod primitives;
mod traits;

pub use crate::{
    cowfile::CowFile,
    cursor::CowFileCursor,
    error::{Error, Result},
    primitives::Primitive,
    traits::{ReadFrom, WriteTo},
};
