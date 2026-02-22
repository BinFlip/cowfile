//! # cowfile
//!
//! A copy-on-write abstraction for working with binary data from files or memory.
//!
//! `cowfile` provides [`CowFile`], a type that wraps binary data with a pending
//! write log. The committed buffer is backed by either a `Vec<u8>` or an OS-level
//! copy-on-write memory map ([`MmapMut`](memmap2::MmapMut) with `MAP_PRIVATE`).
//! Modifications accumulate in a pending log and are applied to the buffer on
//! [`commit`](CowFile::commit).
//!
//! ## Use Case
//!
//! This crate is designed for binary analysis and transformation pipelines where
//! multiple passes modify a binary (e.g., deobfuscation, patching). A parser can
//! hold a `&[u8]` reference to the committed state via [`data`](CowFile::data)
//! while writes accumulate in the pending log, and [`commit`](CowFile::commit)
//! applies them cheaply (only OS pages touched by patches are copied).
//!
//! ## Architecture
//!
//! ```text
//!  Committed Buffer               Pending Log
//! +---------------------+      +-------------------------+
//! | Vec<u8> or MmapMut  | <--- | Vec<PendingWrite>       |
//! | (OS-level CoW)      |      | (applied on commit)     |
//! +---------------------+      +-------------------------+
//! ```
//!
//! - **Committed buffer**: `Vec<u8>` (owned) or `MmapMut` created with
//!   [`map_copy`](memmap2::MmapOptions::map_copy) (`MAP_PRIVATE` on Unix,
//!   `PAGE_WRITECOPY` on Windows). Only pages touched by commits use extra RAM.
//! - **Pending log**: Accumulated writes not yet applied to the buffer.
//!
//! [`data`](CowFile::data) returns `&[u8]` of the committed buffer.
//! [`read`](CowFile::read), [`read_le`](CowFile::read_le), etc. composite
//! pending writes over the committed state.
//!
//! ## Thread Safety
//!
//! [`CowFile`] is [`Send`] and [`Sync`]. The committed buffer can be read
//! concurrently via [`data`](CowFile::data) from multiple threads. Writes
//! to the pending log are serialised by an internal [`RwLock`](std::sync::RwLock).
//!
//! ## Quick Start
//!
//! ```
//! use cowfile::CowFile;
//!
//! let pf = CowFile::from_vec(vec![0u8; 1024]);
//!
//! // Writes go to the pending log
//! pf.write(0x10, &[0xFF, 0xFE]).unwrap();
//! pf.write(0x20, &[0xAA, 0xBB, 0xCC]).unwrap();
//!
//! // data() shows committed state (before writes)
//! assert_eq!(pf.data()[0x10], 0x00);
//!
//! // read() composites pending writes
//! assert_eq!(pf.read_byte(0x10).unwrap(), 0xFF);
//!
//! // Commit applies pending to the buffer
//! let mut pf = pf;
//! pf.commit().unwrap();
//!
//! // Now data() shows the committed writes
//! assert_eq!(pf.data()[0x10], 0xFF);
//! ```
//!
//! ## Memory-Mapped Files
//!
//! For large binaries, use [`CowFile::open`] to create a copy-on-write memory
//! map. The OS handles paging — only accessed regions are loaded into physical
//! memory, and only pages modified by [`commit`](CowFile::commit) are copied:
//!
//! ```no_run
//! use cowfile::CowFile;
//!
//! let pf = CowFile::open("large_binary.exe").unwrap();
//! pf.write(0, &[0x4D, 0x5A]).unwrap(); // Pending
//!
//! let mut pf = pf;
//! pf.commit().unwrap();                 // Only the first page is CoW'd
//! pf.to_file("patched.exe").unwrap();   // Write output
//! ```

#![deny(missing_docs)]

mod cowfile;
mod cursor;
mod error;
mod primitives;
mod traits;

pub use crate::{
    cowfile::CowFile,
    cursor::CowFileCursor,
    error::{Error, Result},
    primitives::Primitive,
    traits::{ReadFrom, WriteTo},
};
