//! Deflate compression and decompression, with no dependencies, in each of the wrappers
//! it's found in:
//!
//! - [`deflate`]: raw deflate data (RFC 1951), as zip stores it.
//! - [`zlib`]: zlib streams (RFC 1950), as PNG stores its pixels.
//! - [`gzip`]: gzip files (RFC 1952), as `.gz` files and HTTP `Content-Encoding: gzip` use.
//! - [`checksum`]: CRC-32 and Adler-32.
//!
//! ```
//! use black_zlib::{gzip, Compression};
//!
//! let packed = gzip::compress(b"hello hello hello", Compression::Default);
//! assert_eq!(gzip::decompress(&packed, 1 << 20)?, b"hello hello hello");
//! # Ok::<(), black_zlib::Error>(())
//! ```

pub mod checksum;
pub mod deflate;
mod error;
pub mod gzip;
pub mod zlib;

pub use deflate::Compression;
pub use error::{Error, Result};
