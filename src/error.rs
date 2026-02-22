//! Error types for the cowfile crate.
//!
//! All fallible operations in this crate return [`Result<T>`], which is an alias
//! for `std::result::Result<T, Error>`. The [`Error`] enum covers I/O failures
//! and out-of-bounds access.

use thiserror::Error;

/// Errors that can occur during cowfile operations.
#[derive(Error, Debug)]
pub enum Error {
    /// An I/O error occurred during file operations.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// An access attempted to read or write beyond the file boundaries.
    #[error("offset {offset} with length {length} exceeds file size {file_size}")]
    OutOfBounds {
        /// The starting offset of the access.
        offset: usize,
        /// The length of the access.
        length: usize,
        /// The total size of the file.
        file_size: usize,
    },
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(e) => e,
            other => std::io::Error::other(other),
        }
    }
}

/// Convenience type alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;
