//! The checksums PNG and zlib use.

const fn crc_table() -> [u32; 256] {
	let mut table = [0u32; 256];
	let mut n = 0;
	while n < 256 {
		let mut c = n as u32;
		let mut k = 0;
		while k < 8 {
			c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
			k += 1;
		}
		table[n] = c;
		n += 1;
	}
	table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (the ISO-HDLC one PNG, gzip and zip use) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
	crc32_update(0, data)
}

/// Continue a CRC-32: `crc32_update(crc32(a), b) == crc32(a ++ b)`.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
	let mut c = !crc;
	for &b in data {
		c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
	}
	!c
}

/// Adler-32 of `data`, as zlib uses.
pub fn adler32(data: &[u8]) -> u32 {
	const MOD: u32 = 65521;
	let (mut a, mut b) = (1u32, 0u32);
	// 5552 is the most bytes that can be summed before `b` could overflow.
	for chunk in data.chunks(5552) {
		for &x in chunk {
			a += x as u32;
			b += a;
		}
		a %= MOD;
		b %= MOD;
	}
	(b << 16) | a
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn known_values() {
		assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
		assert_eq!(crc32(b""), 0);
		assert_eq!(crc32_update(crc32(b"1234"), b"56789"), 0xCBF4_3926);
		assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
		assert_eq!(adler32(b""), 1);
		// Long enough to need the modulo more than once.
		assert_eq!(adler32(&vec![0xFF; 100_000]), adler32_slow(&vec![0xFF; 100_000]));
	}

	fn adler32_slow(data: &[u8]) -> u32 {
		let (mut a, mut b) = (1u64, 0u64);
		for &x in data {
			a = (a + x as u64) % 65521;
			b = (b + a) % 65521;
		}
		((b << 16) | a) as u32
	}
}
