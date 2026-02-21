//! Extensible traits for reading and writing user-defined types.
//!
//! These traits allow external crates to define how their structs are
//! serialized to and deserialized from a [`CowFile`](crate::CowFile).
//! For primitive numeric types, use the built-in
//! [`read_le`](crate::CowFile::read_le) / [`write_le`](crate::CowFile::write_le) methods instead.
//!
//! # Examples
//!
//! ```
//! use cowfile::{CowFile, ReadFrom, WriteTo, Result};
//!
//! struct Header {
//!     magic: u32,
//!     version: u16,
//! }
//!
//! impl ReadFrom for Header {
//!     fn read_from(pf: &CowFile, offset: u64) -> Result<Self> {
//!         Ok(Header {
//!             magic: pf.read_le::<u32>(offset)?,
//!             version: pf.read_le::<u16>(offset + 4)?,
//!         })
//!     }
//! }
//!
//! impl WriteTo for Header {
//!     fn write_to(&self, pf: &CowFile, offset: u64) -> Result<()> {
//!         pf.write_le::<u32>(offset, self.magic)?;
//!         pf.write_le::<u16>(offset + 4, self.version)?;
//!         Ok(())
//!     }
//! }
//!
//! let pf = CowFile::from_vec(vec![0u8; 64]);
//! let header = Header { magic: 0x4D5A9000, version: 2 };
//! pf.write_type(0, &header).unwrap();
//!
//! let read_back: Header = pf.read_type(0).unwrap();
//! assert_eq!(read_back.magic, 0x4D5A9000);
//! assert_eq!(read_back.version, 2);
//! ```

use crate::{cowfile::CowFile, error::Result};

/// Trait for types that can be deserialized from a [`CowFile`] at a given offset.
///
/// Implement this for user-defined structs to enable
/// [`CowFile::read_type`](crate::CowFile::read_type).
///
/// # Examples
///
/// ```
/// use cowfile::{CowFile, ReadFrom, Result};
///
/// struct Pair {
///     a: u32,
///     b: u32,
/// }
///
/// impl ReadFrom for Pair {
///     fn read_from(pf: &CowFile, offset: u64) -> Result<Self> {
///         Ok(Pair {
///             a: pf.read_le::<u32>(offset)?,
///             b: pf.read_le::<u32>(offset + 4)?,
///         })
///     }
/// }
/// ```
pub trait ReadFrom: Sized {
    /// Reads and deserializes a value from the given `cowfile` at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying reads fail (e.g., out of bounds).
    fn read_from(cowfile: &CowFile, offset: u64) -> Result<Self>;
}

/// Trait for types that can be serialized into a [`CowFile`] at a given offset.
///
/// Implement this for user-defined structs to enable
/// [`CowFile::write_type`](crate::CowFile::write_type).
///
/// # Examples
///
/// ```
/// use cowfile::{CowFile, WriteTo, Result};
///
/// struct Pair {
///     a: u32,
///     b: u32,
/// }
///
/// impl WriteTo for Pair {
///     fn write_to(&self, pf: &CowFile, offset: u64) -> Result<()> {
///         pf.write_le::<u32>(offset, self.a)?;
///         pf.write_le::<u32>(offset + 4, self.b)?;
///         Ok(())
///     }
/// }
/// ```
pub trait WriteTo {
    /// Serializes this value into the given `cowfile` at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying writes fail (e.g., out of bounds).
    fn write_to(&self, cowfile: &CowFile, offset: u64) -> Result<()>;
}
