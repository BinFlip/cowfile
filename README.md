# cowfile

A copy-on-write abstraction for binary data backed by memory or files.

[![CI](https://github.com/BinFlip/cowfile/actions/workflows/ci.yml/badge.svg)](https://github.com/BinFlip/cowfile/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/cowfile.svg)](https://crates.io/crates/cowfile)
[![Documentation](https://docs.rs/cowfile/badge.svg)](https://docs.rs/cowfile)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

## Overview

`cowfile` provides `CowFile`, a type that wraps binary data with a pending write log backed by
either a `Vec<u8>` or an OS-level copy-on-write memory map (`MAP_PRIVATE` on Unix, `PAGE_WRITECOPY`
on Windows). Modifications accumulate in a pending log and are applied to the committed buffer on
`commit()`. A final merged output can be produced at any time without committing.

This is designed for binary analysis and transformation pipelines where multiple passes modify
a binary (e.g., deobfuscation, patching) without needing to copy the entire file between each pass.

## Features

- **Zero-copy base layer**: Memory-mapped files or owned byte vectors as the committed buffer
- **Pending write log**: Only modified byte ranges are stored, not the entire file
- **Two-tier commit model**: `data()` returns committed state, `read()` composites pending writes
- **Typed I/O**: Read/write primitives (`u8`..`u64`, `i8`..`i64`, `f32`, `f64`) in little-endian or big-endian
- **User-defined types**: `ReadFrom` and `WriteTo` traits for custom struct serialization
- **Cursor support**: `CowFileCursor` implements `std::io::Read`, `Write`, and `Seek`
- **Thread-safe**: `Send + Sync` with internal `RwLock` synchronization
- **Fork support**: Create independent copies that share read pages via OS-level CoW
- **Dual output**: Produce final output as `Vec<u8>` or write directly to a file

## Quick Start

```rust
use cowfile::CowFile;

// Create from owned bytes
let pf = CowFile::from_vec(vec![0u8; 1024]);

// Writes go to the pending log (uses &self — interior mutability)
pf.write(0x10, &[0xFF, 0xFE]).unwrap();
pf.write(0x20, &[0xAA, 0xBB, 0xCC]).unwrap();

// data() shows committed state (unchanged)
assert_eq!(pf.data()[0x10], 0x00);

// read() composites pending writes over committed state
assert_eq!(pf.read_byte(0x10).unwrap(), 0xFF);

// Commit applies pending writes to the buffer (requires &mut self)
let mut pf = pf;
pf.commit().unwrap();
assert_eq!(pf.data()[0x10], 0xFF);

// More modifications in a second pass
pf.write(0x30, &[0xDD]).unwrap();

// Produce final output with all modifications applied
let output = pf.to_vec().unwrap();
assert_eq!(output[0x10], 0xFF);
assert_eq!(output[0x20], 0xAA);
assert_eq!(output[0x30], 0xDD);
```

### From a file (memory-mapped)

```rust,no_run
use cowfile::CowFile;

let pf = CowFile::open("input.bin").unwrap();
pf.write(0, &[0x4D, 0x5A]).unwrap(); // Pending write

let mut pf = pf;
pf.commit().unwrap();                 // Only the first page is CoW'd
pf.to_file("output.bin").unwrap();    // Write output to disk
```

### Typed primitive I/O

```rust
use cowfile::CowFile;

let pf = CowFile::from_vec(vec![0u8; 16]);

// Write and read little-endian u32
pf.write_le::<u32>(0, 0xDEADBEEF).unwrap();
assert_eq!(pf.read_le::<u32>(0).unwrap(), 0xDEADBEEF);

// Write and read big-endian u16
pf.write_be::<u16>(8, 0xCAFE).unwrap();
assert_eq!(pf.read_be::<u16>(8).unwrap(), 0xCAFE);
```

### User-defined types

```rust
use cowfile::{CowFile, ReadFrom, WriteTo, Result};

struct Header {
    magic: u32,
    version: u16,
}

impl ReadFrom for Header {
    fn read_from(pf: &CowFile, offset: usize) -> Result<Self> {
        Ok(Header {
            magic: pf.read_le::<u32>(offset)?,
            version: pf.read_le::<u16>(offset + 4)?,
        })
    }
}

impl WriteTo for Header {
    fn write_to(&self, pf: &CowFile, offset: usize) -> Result<()> {
        pf.write_le::<u32>(offset, self.magic)?;
        pf.write_le::<u16>(offset + 4, self.version)?;
        Ok(())
    }
}

let pf = CowFile::from_vec(vec![0u8; 64]);
pf.write_type(0, &Header { magic: 0x4D5A9000, version: 2 }).unwrap();

let header: Header = pf.read_type(0).unwrap();
assert_eq!(header.magic, 0x4D5A9000);
assert_eq!(header.version, 2);
```

### Cursor (std::io compatibility)

```rust
use std::io::{Read, Write, Seek, SeekFrom};
use cowfile::CowFile;

let pf = CowFile::from_vec(vec![0u8; 64]);
let mut cursor = pf.cursor();

// Use standard I/O traits
cursor.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
cursor.seek(SeekFrom::Start(0)).unwrap();

let mut buf = [0u8; 4];
cursor.read_exact(&mut buf).unwrap();
assert_eq!(buf, [0xDE, 0xAD, 0xBE, 0xEF]);
```

### Forking

```rust,no_run
use cowfile::CowFile;

let pf = CowFile::open("binary.exe").unwrap();
pf.write(0, &[0xFF]).unwrap();

// Fork re-opens the file — shares read pages via OS-level CoW
let forked = pf.fork().unwrap();
assert!(!forked.has_pending()); // Fork starts clean
```

## Architecture

```
 Committed Buffer (immutable)    Pending Log (copy-on-write)
+---------------------+        +-------------------------+
| Vec<u8> or MmapMut  |  <---  | Vec<PendingWrite>       |
| (OS-level CoW)      |        | (applied on commit)     |
+---------------------+        +-------------------------+

  data()  -> &[u8] of committed buffer (zero-cost)
  read()  -> composites pending writes over committed state
  commit() -> applies pending to buffer, clears log
  discard() -> clears pending log without applying
  to_vec() / to_file() -> materializes with pending applied
```

## API Summary

### Constructors

| Method | Description |
|--------|-------------|
| `CowFile::from_vec(data)` | Create from an owned `Vec<u8>` (zero-copy move) |
| `CowFile::open(path)` | Memory-map a file with copy-on-write semantics |
| `CowFile::from_file(file)` | Memory-map from an open `std::fs::File` |

### Data Access

| Method | Description |
|--------|-------------|
| `data()` | Returns `&[u8]` of committed buffer (pending not visible) |
| `read(offset, len)` | Read bytes with pending writes composited |
| `read_byte(offset)` | Read a single byte with pending composited |
| `read_le::<T>(offset)` | Read a primitive in little-endian order |
| `read_be::<T>(offset)` | Read a primitive in big-endian order |
| `read_type::<T>(offset)` | Read a user-defined `ReadFrom` type |

### Writing

| Method | Description |
|--------|-------------|
| `write(offset, data)` | Write bytes to the pending log (`&self`) |
| `write_byte(offset, byte)` | Write a single byte to the pending log |
| `write_le::<T>(offset, val)` | Write a primitive in little-endian order |
| `write_be::<T>(offset, val)` | Write a primitive in big-endian order |
| `write_type(offset, val)` | Write a user-defined `WriteTo` type |

### Lifecycle

| Method | Description |
|--------|-------------|
| `commit()` | Apply pending writes to the committed buffer (`&mut self`) |
| `discard()` | Clear pending writes without applying (`&mut self`) |
| `has_pending()` | Check if there are uncommitted writes |
| `fork()` | Create an independent copy (re-maps file if mmap-backed) |

### Output

| Method | Description |
|--------|-------------|
| `to_vec()` | Produce `Vec<u8>` with pending applied |
| `to_file(path)` | Write to disk with pending applied |
| `into_vec()` | Consume and return data (zero-copy if no pending and Vec-backed) |
| `cursor()` | Create a `CowFileCursor` implementing `Read`/`Write`/`Seek` |

### Metadata

| Method | Description |
|--------|-------------|
| `len()` | Total data length in bytes |
| `is_empty()` | Whether the data is empty |
| `source_path()` | Original file path (for `open()`-created instances) |

## Thread Safety

`CowFile` is `Send + Sync`. The committed buffer can be read concurrently via `data()` from
multiple threads. Writes to the pending log are serialised by an internal `RwLock`. The `dirty`
flag uses an `AtomicBool` for a lock-free fast path when there are no pending writes.

Note that `commit()` and `discard()` require `&mut self`, so they need exclusive access.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE-APACHE](LICENSE-APACHE) for details.
