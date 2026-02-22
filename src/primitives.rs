//! Sealed [`Primitive`] trait for typed little-endian and big-endian I/O.
//!
//! This module defines a trait for numeric types that can be read from and
//! written to a [`CowFile`](crate::CowFile) at a given byte offset. The trait
//! is sealed — external crates cannot implement it for their own types. For
//! user-defined structs, use [`ReadFrom`](crate::ReadFrom) and
//! [`WriteTo`](crate::WriteTo) instead.

/// Sealed trait for numeric primitives that support endian-aware byte I/O.
///
/// Implemented for: [`u8`], [`i8`], [`u16`], [`i16`], [`u32`], [`i32`],
/// [`u64`], [`i64`], [`f32`], [`f64`].
///
/// This trait is sealed — it cannot be implemented outside of this crate.
/// For user-defined types, implement [`ReadFrom`](crate::ReadFrom) and
/// [`WriteTo`](crate::WriteTo) instead.
///
/// # Examples
///
/// ```
/// use cowfile::CowFile;
///
/// let pf = CowFile::from_vec(vec![0u8; 16]);
///
/// pf.write_le::<u32>(0, 0xDEADBEEF).unwrap();
/// assert_eq!(pf.read_le::<u32>(0).unwrap(), 0xDEADBEEF);
///
/// pf.write_be::<u16>(8, 0xCAFE).unwrap();
/// assert_eq!(pf.read_be::<u16>(8).unwrap(), 0xCAFE);
/// ```
pub trait Primitive: Sized + Copy + private::Sealed {
    /// The size of this type in bytes.
    const SIZE: usize;

    /// Decodes a value from little-endian bytes.
    ///
    /// # Panics
    ///
    /// Panics if `bytes.len() < Self::SIZE`.
    fn from_le_bytes(bytes: &[u8]) -> Self;

    /// Decodes a value from big-endian bytes.
    ///
    /// # Panics
    ///
    /// Panics if `bytes.len() < Self::SIZE`.
    fn from_be_bytes(bytes: &[u8]) -> Self;

    /// Encodes this value as little-endian bytes into `buf`.
    ///
    /// # Panics
    ///
    /// Panics if `buf.len() < Self::SIZE`.
    fn write_le_bytes(self, buf: &mut [u8]);

    /// Encodes this value as big-endian bytes into `buf`.
    ///
    /// # Panics
    ///
    /// Panics if `buf.len() < Self::SIZE`.
    fn write_be_bytes(self, buf: &mut [u8]);
}

mod private {
    pub trait Sealed {}
}

macro_rules! impl_primitive {
    ($($ty:ty),*) => {
        $(
            impl private::Sealed for $ty {}

            impl Primitive for $ty {
                const SIZE: usize = std::mem::size_of::<$ty>();

                fn from_le_bytes(bytes: &[u8]) -> Self {
                    let arr: [u8; std::mem::size_of::<$ty>()] =
                        bytes[..std::mem::size_of::<$ty>()].try_into().unwrap();
                    <$ty>::from_le_bytes(arr)
                }

                fn from_be_bytes(bytes: &[u8]) -> Self {
                    let arr: [u8; std::mem::size_of::<$ty>()] =
                        bytes[..std::mem::size_of::<$ty>()].try_into().unwrap();
                    <$ty>::from_be_bytes(arr)
                }

                fn write_le_bytes(self, buf: &mut [u8]) {
                    let arr = self.to_le_bytes();
                    buf[..std::mem::size_of::<$ty>()].copy_from_slice(&arr);
                }

                fn write_be_bytes(self, buf: &mut [u8]) {
                    let arr = self.to_be_bytes();
                    buf[..std::mem::size_of::<$ty>()].copy_from_slice(&arr);
                }
            }
        )*
    };
}

impl_primitive!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

#[cfg(test)]
mod tests {
    use crate::Primitive;

    #[test]
    fn test_u8_roundtrip() {
        let mut buf = [0u8; 1];
        42u8.write_le_bytes(&mut buf);
        assert_eq!(<u8 as Primitive>::from_le_bytes(&buf), 42);
        42u8.write_be_bytes(&mut buf);
        assert_eq!(<u8 as Primitive>::from_be_bytes(&buf), 42);
    }

    #[test]
    fn test_i8_roundtrip() {
        let mut buf = [0u8; 1];
        (-7i8).write_le_bytes(&mut buf);
        assert_eq!(<i8 as Primitive>::from_le_bytes(&buf), -7);
    }

    #[test]
    fn test_u16_roundtrip() {
        let mut buf = [0u8; 2];
        0xCAFEu16.write_le_bytes(&mut buf);
        assert_eq!(<u16 as Primitive>::from_le_bytes(&buf), 0xCAFE);
        0xCAFEu16.write_be_bytes(&mut buf);
        assert_eq!(<u16 as Primitive>::from_be_bytes(&buf), 0xCAFE);
    }

    #[test]
    fn test_u32_roundtrip() {
        let mut buf = [0u8; 4];
        0xDEADBEEFu32.write_le_bytes(&mut buf);
        assert_eq!(<u32 as Primitive>::from_le_bytes(&buf), 0xDEADBEEF);
        0xDEADBEEFu32.write_be_bytes(&mut buf);
        assert_eq!(<u32 as Primitive>::from_be_bytes(&buf), 0xDEADBEEF);
    }

    #[test]
    fn test_u64_roundtrip() {
        let mut buf = [0u8; 8];
        0x0123456789ABCDEFu64.write_le_bytes(&mut buf);
        assert_eq!(<u64 as Primitive>::from_le_bytes(&buf), 0x0123456789ABCDEF);
    }

    #[test]
    fn test_i32_roundtrip() {
        let mut buf = [0u8; 4];
        (-123456i32).write_le_bytes(&mut buf);
        assert_eq!(<i32 as Primitive>::from_le_bytes(&buf), -123456);
    }

    #[test]
    fn test_f32_roundtrip() {
        let mut buf = [0u8; 4];
        std::f32::consts::PI.write_le_bytes(&mut buf);
        assert_eq!(
            <f32 as Primitive>::from_le_bytes(&buf),
            std::f32::consts::PI
        );
    }

    #[test]
    fn test_f64_roundtrip() {
        let mut buf = [0u8; 8];
        std::f64::consts::E.write_le_bytes(&mut buf);
        assert_eq!(<f64 as Primitive>::from_le_bytes(&buf), std::f64::consts::E);
    }

    #[test]
    fn test_endianness_u32() {
        let mut le_buf = [0u8; 4];
        let mut be_buf = [0u8; 4];

        0x01020304u32.write_le_bytes(&mut le_buf);
        0x01020304u32.write_be_bytes(&mut be_buf);

        // LE: least significant byte first.
        assert_eq!(le_buf, [0x04, 0x03, 0x02, 0x01]);
        // BE: most significant byte first.
        assert_eq!(be_buf, [0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_endianness_u16() {
        let mut le_buf = [0u8; 2];
        let mut be_buf = [0u8; 2];

        0xAABBu16.write_le_bytes(&mut le_buf);
        0xAABBu16.write_be_bytes(&mut be_buf);

        assert_eq!(le_buf, [0xBB, 0xAA]);
        assert_eq!(be_buf, [0xAA, 0xBB]);
    }
}
