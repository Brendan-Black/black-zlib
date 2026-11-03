//! gzip data (RFC 1952): deflate data with a header and a CRC-32 and length check, as `.gz`
//! files and HTTP's `Content-Encoding: gzip` use.

use crate::checksum::crc32;
use crate::deflate::{Compression, deflate, inflate_counted};
use crate::{Error, Result};

const MAGIC: [u8; 2] = [0x1F, 0x8B];
const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const RESERVED: u8 = 0xE0;

/// Compress `data` into gzip data, with no file name and no time.
pub fn compress(data: &[u8], level: Compression) -> Vec<u8> {
    let xfl = match level {
        Compression::Fast => 4,
        Compression::Default => 0,
        Compression::Best => 2,
    };
    // Magic, deflate, no flags, no time, the level hint, "unknown" operating system.
    let mut out = vec![MAGIC[0], MAGIC[1], 8, 0, 0, 0, 0, 0, xfl, 255];
    out.extend(deflate(data, level));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

/// Decompress gzip data, refusing to produce more than `max_len` bytes in all. Handles
/// several gzip members one after another (as `cat a.gz b.gz` makes) and checks each one's
/// CRC-32 and length. Anything after the last member that isn't another member is ignored.
pub fn decompress(data: &[u8], max_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = data;
    loop {
        let body = skip_header(rest)?;
        let start = out.len();
        // Each member gets what's left of the limit; an error names the caller's whole limit.
        let (member, used) = inflate_counted(body, max_len - start).map_err(|e| match e {
            Error::TooLarge { .. } => Error::TooLarge { limit: max_len },
            e => e,
        })?;
        out.extend(member);
        let Some(trailer) = body[used..].first_chunk::<8>() else {
            return Err(Error::corrupt("the gzip data is missing its checksum"));
        };
        let crc = u32::from_le_bytes(trailer[..4].try_into().unwrap());
        let len = u32::from_le_bytes(trailer[4..].try_into().unwrap());
        if crc != crc32(&out[start..]) {
            return Err(Error::corrupt("the gzip checksum doesn't match"));
        }
        if len != (out.len() - start) as u32 {
            return Err(Error::corrupt("the gzip length doesn't match"));
        }
        rest = &body[used + 8..];
        if !rest.starts_with(&MAGIC) {
            return Ok(out);
        }
    }
}

/// Check a member's header and return what follows it.
fn skip_header(data: &[u8]) -> Result<&[u8]> {
    let cut = || Error::corrupt("the gzip header is cut short");
    let Some(&[m0, m1, cm, flags, ..]) = data.first_chunk::<10>() else {
        return Err(cut());
    };
    if [m0, m1] != MAGIC {
        return Err(Error::corrupt("not gzip data"));
    }
    if cm != 8 {
        return Err(Error::corrupt(format!("unknown gzip compression method {cm}")));
    }
    if flags & RESERVED != 0 {
        return Err(Error::corrupt("reserved gzip header flags are set"));
    }
    let mut pos = 10;
    if flags & FEXTRA != 0 {
        let len = data.get(pos..pos + 2).ok_or_else(cut)?;
        pos += 2 + u16::from_le_bytes([len[0], len[1]]) as usize;
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            let end = data.get(pos..).and_then(|d| d.iter().position(|&b| b == 0)).ok_or_else(cut)?;
            pos += end + 1;
        }
    }
    if flags & FHCRC != 0 {
        let sum = data.get(pos..pos + 2).ok_or_else(cut)?;
        if u16::from_le_bytes([sum[0], sum[1]]) != crc32(&data[..pos]) as u16 {
            return Err(Error::corrupt("the gzip header checksum doesn't match"));
        }
        pos += 2;
    }
    data.get(pos..).ok_or_else(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for level in [Compression::Fast, Compression::Default, Compression::Best] {
            for data in [&b""[..], b"x", &b"hello ".repeat(5000)] {
                assert_eq!(decompress(&compress(data, level), data.len()).as_deref(), Ok(data));
            }
        }
    }

    #[test]
    fn reads_every_header_field() {
        // Extra field, file name, comment and header checksum all present.
        let mut gz = vec![0x1F, 0x8B, 8, FEXTRA | FNAME | FCOMMENT | FHCRC, 1, 2, 3, 4, 0, 3];
        gz.extend([3, 0, b'a', b'b', b'c']);
        gz.extend(b"notes.txt\0");
        gz.extend(b"a comment\0");
        gz.extend((crc32(&gz) as u16).to_le_bytes());
        let body = compress(b"hello", Compression::Default);
        gz.extend(&body[10..]);
        assert_eq!(decompress(&gz, 100).unwrap(), b"hello");
        // A wrong header checksum is caught.
        let at = gz.len() - (body.len() - 10) - 2;
        gz[at] ^= 1;
        assert!(matches!(decompress(&gz, 100), Err(Error::Corrupt(_))));
    }

    #[test]
    fn reads_members_one_after_another() {
        let mut gz = compress(b"hello ", Compression::Fast);
        gz.extend(compress(b"world", Compression::Best));
        assert_eq!(decompress(&gz, 100).unwrap(), b"hello world");
        // The limit counts every member together.
        assert_eq!(decompress(&gz, 8), Err(Error::TooLarge { limit: 8 }));
        // Something that isn't another member is ignored, as zlib ignores what follows.
        gz.extend([0, 0, 0]);
        assert_eq!(decompress(&gz, 100).unwrap(), b"hello world");
    }

    #[test]
    fn rejects_bad_data() {
        let gz = compress(b"hello hello hello", Compression::Default);
        let corrupt = |r: Result<Vec<u8>>| matches!(r, Err(Error::Corrupt(_)));
        assert!(corrupt(decompress(&gz[..gz.len() - 1], 100)), "cut-off length");
        assert!(corrupt(decompress(&gz[..5], 100)), "cut-off header");
        assert!(corrupt(decompress(&[], 100)));
        let flip = |i: usize| {
            let mut g = gz.clone();
            g[i] ^= 0x01;
            decompress(&g, 100)
        };
        assert!(corrupt(flip(0)), "magic");
        assert!(corrupt(flip(2)), "method");
        assert!(corrupt(flip(gz.len() - 8)), "checksum");
        assert!(corrupt(flip(gz.len() - 4)), "length");
        let mut reserved = gz.clone();
        reserved[3] = 0x20;
        assert!(corrupt(decompress(&reserved, 100)));
    }
}
