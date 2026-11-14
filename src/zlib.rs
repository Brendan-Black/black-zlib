//! zlib streams (RFC 1950): deflate data with a two-byte header and an Adler-32 checksum,
//! as PNG stores its pixels.

use crate::checksum::adler32;
use crate::deflate::{Compression, deflate, inflate_counted};
use crate::{Error, Result};

/// Compress `data` into a zlib stream.
pub fn compress(data: &[u8], level: Compression) -> Vec<u8> {
	// CMF 0x78: deflate with a 32 KiB window. FLG holds the level hint in its top two bits,
	// and its low five bits make the pair a multiple of 31.
	const CMF: u8 = 0x78;
	let hint = level.zlib_hint() << 6;
	let flg = hint + (31 - (u16::from(CMF) << 8 | u16::from(hint)) % 31) as u8 % 31;
	let mut out = vec![CMF, flg];
	out.extend(deflate(data, level));
	out.extend(adler32(data).to_be_bytes());
	out
}

/// Decompress a zlib stream, refusing to produce more than `max_len` bytes. Checks the
/// header and the Adler-32 checksum; bytes after the stream are ignored.
pub fn decompress(data: &[u8], max_len: usize) -> Result<Vec<u8>> {
	decompress_counted(data, max_len).map(|(out, _)| out)
}

/// Like [`decompress`], also giving how many input bytes the whole stream took up, header
/// and checksum included, so a caller can find what follows it. Git's packs need this: they
/// record each object's size uncompressed, but not where its zlib stream ends.
pub fn decompress_counted(data: &[u8], max_len: usize) -> Result<(Vec<u8>, usize)> {
	let [cmf, flg, ..] = *data else {
		return Err(Error::corrupt("the zlib stream is cut short"));
	};
	if cmf & 0x0F != 8 || cmf >> 4 > 7 {
		return Err(Error::corrupt("not a zlib stream (unknown compression method or window size)"));
	}
	if (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
		return Err(Error::corrupt("not a zlib stream (bad header check)"));
	}
	if flg & 0x20 != 0 {
		return Err(Error::corrupt("the zlib stream needs a preset dictionary"));
	}
	let (out, used) = inflate_counted(&data[2..], max_len)?;
	let Some(sum) = data[2 + used..].first_chunk::<4>() else {
		return Err(Error::corrupt("the zlib stream is missing its checksum"));
	};
	if u32::from_be_bytes(*sum) != adler32(&out) {
		return Err(Error::corrupt("the zlib checksum doesn't match"));
	}
	Ok((out, 2 + used + 4))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trips() {
		for level in crate::deflate::every_level() {
			for data in [&b""[..], b"x", &b"hello ".repeat(5000)] {
				assert_eq!(decompress(&compress(data, level), data.len()).as_deref(), Ok(data));
			}
		}
	}

	#[test]
	fn reads_what_zlib_wrote() {
		// zlib.compress(b"hello") from zlib itself.
		let z = [0x78, 0x9C, 0xCB, 0x48, 0xCD, 0xC9, 0xC9, 0x07, 0x00, 0x06, 0x2C, 0x02, 0x15];
		assert_eq!(decompress(&z, 100).unwrap(), b"hello");
	}

	#[test]
	fn rejects_bad_streams() {
		let z = compress(b"hello hello hello", Compression::Default);
		let corrupt = |r: Result<Vec<u8>>| matches!(r, Err(Error::Corrupt(_)));
		assert!(corrupt(decompress(&z[..z.len() - 1], 100)), "cut-off checksum");
		assert!(corrupt(decompress(&z[..4], 100)), "cut-off data");
		let mut bad_sum = z.clone();
		*bad_sum.last_mut().unwrap() ^= 1;
		assert!(corrupt(decompress(&bad_sum, 100)));
		assert!(corrupt(decompress(&[0x78, 0x9D, 0x03, 0x00], 100)), "bad header check");
		assert!(corrupt(decompress(&[0x79, 0x9C], 100)), "unknown method");
		assert!(corrupt(decompress(&[0x78, 0xBB, 0, 0, 0, 1], 100)), "preset dictionary");
		assert!(corrupt(decompress(&[], 100)));
	}

	#[test]
	fn stops_at_the_limit() {
		let z = compress(&vec![0; 1 << 20], Compression::Default);
		assert_eq!(decompress(&z, 1000), Err(Error::TooLarge { limit: 1000 }));
		assert_eq!(decompress(&z, 1 << 20).map(|v| v.len()), Ok(1 << 20));
	}

	#[test]
	fn counts_the_whole_stream() {
		// Two streams back to back, as objects sit in a git pack.
		let a = compress(b"first object", Compression::Level(1));
		let b = compress(&b"second ".repeat(100), Compression::Level(9));
		let both = [&a[..], &b[..]].concat();
		let (out, used) = decompress_counted(&both, 100).unwrap();
		assert_eq!((&out[..], used), (&b"first object"[..], a.len()));
		assert_eq!(decompress_counted(&both[used..], 700).unwrap(), (b"second ".repeat(100), b.len()));
	}

	#[test]
	fn headers_carry_zlibs_level_hints() {
		// zlib writes these first two bytes at levels 0 to 9.
		let zlib = [0x01, 0x01, 0x5E, 0x5E, 0x5E, 0x5E, 0x9C, 0xDA, 0xDA, 0xDA];
		for (n, flg) in zlib.into_iter().enumerate() {
			assert_eq!(compress(b"", Compression::Level(n as u8))[..2], [0x78, flg], "level {n}");
		}
		assert_eq!(compress(b"", Compression::Fast)[1], 0x01);
		assert_eq!(compress(b"", Compression::Default)[1], 0x9C);
		assert_eq!(compress(b"", Compression::Best)[1], 0xDA);
	}
}
