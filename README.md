# cowfile

A copy-on-write overlay layer for immutable binary data.

[![CI](https://github.com/BinFlip/cowfile/actions/workflows/ci.yml/badge.svg)](https://github.com/BinFlip/cowfile/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/cowfile.svg)](https://crates.io/crates/cowfile)
[![Documentation](https://docs.rs/cowfile/badge.svg)](https://docs.rs/cowfile)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

## Overview

`cowfile` provides `CowFile`, a type that wraps immutable binary data with a sparse copy-on-write
overlay. Modifications are tracked as byte-range patches without ever mutating the original data.
A final merged output is produced only when explicitly requested.

This is useful for binary analysis and transformation pipelines where multiple passes modify
a binary without needing to copy the entire file between each pass.

## Features

- **Zero-copy base layer**: Memory-mapped files or owned byte vectors as the immutable base
- **Sparse overlay**: Only modified byte ranges are stored, not the entire file
- **Two-tier commit model**: Pending modifications can be committed (consolidated) independently
- **Thread-safe**: `Send + Sync` with internal `RwLock` synchronization
- **Dual output**: Produce final output as `Vec<u8>` or write directly to a file

## Quick Start

```rust
use cowfile::CowFile;

// Create from owned bytes
let pf = CowFile::from_vec(vec![0u8; 1024]);

// Apply modifications (multiple passes)
pf.write(0x10, &[0xFF, 0xFE]).unwrap();
pf.write(0x20, &[0xAA, 0xBB, 0xCC]).unwrap();

// Commit consolidates pending changes
pf.commit().unwrap();

// More modifications in a second pass
pf.write(0x30, &[0xDD]).unwrap();

// Produce final output with all modifications applied
let output = pf.to_vec().unwrap();
assert_eq!(output[0x10], 0xFF);
assert_eq!(output[0x20], 0xAA);
assert_eq!(output[0x30], 0xDD);
```

### From a file (memory-mapped)

```rust
use cowfile::CowFile;

let pf = CowFile::from_path("input.bin").unwrap();
pf.write(0, &[0x4D, 0x5A]).unwrap(); // Patch MZ header
pf.to_file("output.bin").unwrap();
```

## Architecture

```
 Base Layer (immutable)        Overlay (copy-on-write)
+---------------------+      +-------------------------+
| Vec<u8> or Mmap     |      | committed: BTreeMap     |
| (never modified)    | <--- | pending:   BTreeMap     |
+---------------------+      +-------------------------+
                                        |
                              read: base + committed + pending
                              commit: pending -> committed
                              to_vec/to_file: materialize all
```

## License

Licensed under the Apache License, Version 2.0. See [LICENSE-APACHE](LICENSE-APACHE) for details.
