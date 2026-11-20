//! Raw deflate data (RFC 1951), the compression inside zlib, gzip, zip and PNG.
//!
//! [`deflate`] finds repeats with hash chains (and lazy matching, apart from
//! [`Compression::Fast`]), then writes each block whichever way is smallest: with Huffman
//! codes built for that block, with the fixed codes, or stored as is. [`inflate`] reads any
//! valid deflate data, up to a size limit the caller sets.

use crate::{Error, Result};

/// How hard [`deflate`] (and the zlib and gzip `compress` functions) look for repeats.
///
/// `Fast`, `Default` and `Best` are the three named presets. `Level` takes zlib's numeric
/// scale, the one tools like git configure: `Level(0)` stores the data uncompressed, and
/// `Level(1)` to `Level(9)` run from quickest to smallest, searching the way zlib's levels
/// of the same number do. Levels above 9 count as 9.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Compression {
	/// Quickest: the first few candidates, no lazy matching.
	Fast,
	#[default]
	Default,
	/// Smallest output: longer searches.
	Best,
	/// zlib's numeric level: 0 (stored, no compression) to 9 (smallest).
	Level(u8),
}

/// How the matcher searches at one level.
#[derive(Clone, Copy)]
struct Params {
	/// Candidates to try per position.
	max_chain: usize,
	/// Lazy matching, tried when the match found is shorter than this; `None` is greedy.
	max_lazy: Option<usize>,
	/// Stop searching once a match is this long.
	nice: usize,
}

impl Compression {
	/// True when the data is to be stored as is, with no search at all.
	fn stored(self) -> bool {
		self == Compression::Level(0)
	}

	fn params(self) -> Params {
		let p = |max_chain, max_lazy, nice| Params { max_chain, max_lazy, nice };
		match self {
			Compression::Fast => p(8, None, 32),
			Compression::Default => p(128, Some(32), 128),
			Compression::Best => p(1024, Some(32), MAX_MATCH),
			// After zlib's configuration table (deflate.c): its chain and nice lengths, and
			// its lazy-match limits for the levels that match lazily. Level 0 never searches.
			Compression::Level(0 | 1) => p(4, None, 8),
			Compression::Level(2) => p(8, None, 16),
			Compression::Level(3) => p(32, None, 32),
			Compression::Level(4) => p(16, Some(4), 16),
			Compression::Level(5) => p(32, Some(16), 32),
			Compression::Level(6) => p(128, Some(16), 128),
			Compression::Level(7) => p(256, Some(32), 128),
			Compression::Level(8) => p(1024, Some(128), MAX_MATCH),
			Compression::Level(_) => p(4096, Some(MAX_MATCH), MAX_MATCH),
		}
	}

	/// The 2-bit level hint in a zlib header (0 quickest to 3 smallest), set as zlib sets it.
	pub(crate) fn zlib_hint(self) -> u8 {
		match self {
			Compression::Fast | Compression::Level(0 | 1) => 0,
			Compression::Level(2..=5) => 1,
			Compression::Default | Compression::Level(6) => 2,
			Compression::Best | Compression::Level(_) => 3,
		}
	}

	/// gzip's XFL byte: 4 for the quickest level, 2 for the smallest, otherwise 0, as gzip
	/// sets it.
	pub(crate) fn gzip_xfl(self) -> u8 {
		match self {
			Compression::Fast | Compression::Level(1) => 4,
			Compression::Best | Compression::Level(9..) => 2,
			_ => 0,
		}
	}
}

// ---------- the format's tables ----------

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] =
	[1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// The order code-length code lengths are sent in.
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

const END_OF_BLOCK: usize = 256;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const WINDOW: usize = 32768;

fn fixed_lit_lengths() -> [u8; 288] {
	let mut l = [8u8; 288];
	l[144..256].fill(9);
	l[256..280].fill(7);
	l
}

const FIXED_DIST_LENGTHS: [u8; 30] = [5; 30];

// ---------- compression ----------

/// One LZ77 symbol: a literal byte (`dist == 0`) or a repeat of `len` bytes from `dist` back.
#[derive(Clone, Copy)]
struct Symbol {
	len: u16,
	dist: u16,
}

/// Symbols per block. Each block gets its own Huffman codes, so smaller blocks adapt to
/// changing data, at the cost of more headers.
const BLOCK_SYMBOLS: usize = 1 << 15;

/// Compress `data` into raw deflate data, with no zlib wrapper.
pub fn deflate(data: &[u8], level: Compression) -> Vec<u8> {
	let mut w = BitWriter::default();
	if level.stored() {
		write_stored(&mut w, data, true);
		return w.finish();
	}
	let mut m = Matcher::new(data, level);
	let mut syms = Vec::with_capacity(BLOCK_SYMBOLS);
	let mut block_start = 0;
	let mut i = 0;
	let max_lazy = level.params().max_lazy;
	while i < data.len() {
		let (len, dist) = m.find(i);
		m.insert(i);
		if len >= MIN_MATCH {
			// Lazy matching: if the next position starts a longer match, take this byte
			// as a literal instead.
			if max_lazy.is_some_and(|max| len < max) && i + 1 < data.len() && m.find(i + 1).0 > len {
				syms.push(Symbol { len: data[i] as u16, dist: 0 });
				i += 1;
			} else {
				syms.push(Symbol { len: len as u16, dist: dist as u16 });
				for k in i + 1..i + len {
					m.insert(k);
				}
				i += len;
			}
		} else {
			syms.push(Symbol { len: data[i] as u16, dist: 0 });
			i += 1;
		}
		if syms.len() >= BLOCK_SYMBOLS {
			write_block(&mut w, &syms, &data[block_start..i], i == data.len());
			syms.clear();
			block_start = i;
		}
	}
	if !syms.is_empty() || data.is_empty() {
		write_block(&mut w, &syms, &data[block_start..], true);
	}
	w.finish()
}

const HASH_BITS: u32 = 15;
const NONE: usize = usize::MAX;

/// Finds earlier occurrences of the bytes at a position, through chains of positions that
/// share a hash of their first three bytes.
struct Matcher<'a> {
	data: &'a [u8],
	head: Vec<usize>,
	prev: Vec<usize>,
	max_chain: usize,
	good_enough: usize,
}

impl<'a> Matcher<'a> {
	fn new(data: &'a [u8], level: Compression) -> Self {
		let Params { max_chain, nice: good_enough, .. } = level.params();
		Matcher { data, head: vec![NONE; 1 << HASH_BITS], prev: vec![NONE; WINDOW], max_chain, good_enough }
	}

	fn hash(&self, i: usize) -> usize {
		let d = self.data;
		let v = (d[i] as u32) << 16 | (d[i + 1] as u32) << 8 | d[i + 2] as u32;
		(v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
	}

	fn insert(&mut self, i: usize) {
		if i + MIN_MATCH <= self.data.len() {
			let h = self.hash(i);
			self.prev[i % WINDOW] = self.head[h];
			self.head[h] = i;
		}
	}

	/// The longest earlier match for the bytes at `i` as (length, distance), or length 0.
	fn find(&self, i: usize) -> (usize, usize) {
		let d = self.data;
		if i + MIN_MATCH > d.len() {
			return (0, 0);
		}
		let max_len = MAX_MATCH.min(d.len() - i);
		let (mut best_len, mut best_dist) = (0, 0);
		let mut cand = self.head[self.hash(i)];
		let mut tries = self.max_chain;
		while cand != NONE && cand < i && i - cand <= WINDOW && tries > 0 {
			tries -= 1;
			// Only a candidate that beats the best so far is worth comparing in full.
			if d[cand + best_len.min(max_len - 1)] == d[i + best_len.min(max_len - 1)] {
				let len = d[cand..cand + max_len].iter().zip(&d[i..i + max_len]).take_while(|(a, b)| a == b).count();
				if len > best_len {
					(best_len, best_dist) = (len, i - cand);
					if len >= self.good_enough.min(max_len) {
						break;
					}
				}
			}
			let next = self.prev[cand % WINDOW];
			// A slot overwritten by a newer position ends the chain.
			if next == NONE || next >= cand {
				break;
			}
			cand = next;
		}
		if best_len >= MIN_MATCH { (best_len, best_dist) } else { (0, 0) }
	}
}

fn len_code(len: usize) -> usize {
	LEN_BASE.partition_point(|&b| b as usize <= len) - 1
}

fn dist_code(dist: usize) -> usize {
	DIST_BASE.partition_point(|&b| b as usize <= dist) - 1
}

/// Write one block covering `raw` (the input bytes its `syms` encode), whichever way is smallest.
fn write_block(w: &mut BitWriter, syms: &[Symbol], raw: &[u8], last: bool) {
	let mut lit_freq = [0u32; 286];
	let mut dist_freq = [0u32; 30];
	for s in syms {
		if s.dist == 0 {
			lit_freq[s.len as usize] += 1;
		} else {
			lit_freq[257 + len_code(s.len as usize)] += 1;
			dist_freq[dist_code(s.dist as usize)] += 1;
		}
	}
	lit_freq[END_OF_BLOCK] += 1;

	let lit_lens = code_lengths(&lit_freq, 15);
	let dist_lens = code_lengths(&dist_freq, 15);
	let header = DynamicHeader::new(&lit_lens, &dist_lens);
	let dynamic_bits = 3 + header.bits() + data_bits(syms, &lit_lens, &dist_lens);
	let fixed_lit = fixed_lit_lengths();
	let fixed_bits = 3 + data_bits(syms, &fixed_lit, &FIXED_DIST_LENGTHS);
	// Each stored chunk: 3 header bits, up to 7 bits of padding, LEN and NLEN, the bytes.
	let stored_bits = raw.len().div_ceil(65535).max(1) * (3 + 7 + 32) + raw.len() * 8;

	if stored_bits < dynamic_bits.min(fixed_bits) {
		write_stored(w, raw, last);
	} else if fixed_bits <= dynamic_bits {
		w.bits(last as u32, 1);
		w.bits(1, 2);
		write_data(w, syms, &fixed_lit, &FIXED_DIST_LENGTHS);
	} else {
		w.bits(last as u32, 1);
		w.bits(2, 2);
		header.write(w);
		write_data(w, syms, &lit_lens, &dist_lens);
	}
}

/// Write `raw` as stored blocks of up to 65535 bytes each (one empty block if it's empty).
fn write_stored(w: &mut BitWriter, raw: &[u8], last: bool) {
	let chunks: Vec<&[u8]> = if raw.is_empty() { vec![raw] } else { raw.chunks(65535).collect() };
	for (k, chunk) in chunks.iter().enumerate() {
		w.bits((last && k == chunks.len() - 1) as u32, 1);
		w.bits(0, 2);
		w.align();
		w.bits(chunk.len() as u32, 16);
		w.bits(!(chunk.len() as u32) & 0xFFFF, 16);
		w.bytes(chunk);
	}
}

fn data_bits(syms: &[Symbol], lit_lens: &[u8], dist_lens: &[u8]) -> usize {
	let mut bits = lit_lens[END_OF_BLOCK] as usize;
	for s in syms {
		if s.dist == 0 {
			bits += lit_lens[s.len as usize] as usize;
		} else {
			let lc = len_code(s.len as usize);
			let dc = dist_code(s.dist as usize);
			bits += (lit_lens[257 + lc] + LEN_EXTRA[lc] + dist_lens[dc] + DIST_EXTRA[dc]) as usize;
		}
	}
	bits
}

fn write_data(w: &mut BitWriter, syms: &[Symbol], lit_lens: &[u8], dist_lens: &[u8]) {
	let lit_codes = canonical_codes(lit_lens);
	let dist_codes = canonical_codes(dist_lens);
	for s in syms {
		if s.dist == 0 {
			let l = s.len as usize;
			w.bits(lit_codes[l] as u32, lit_lens[l] as u32);
		} else {
			let lc = len_code(s.len as usize);
			w.bits(lit_codes[257 + lc] as u32, lit_lens[257 + lc] as u32);
			w.bits((s.len - LEN_BASE[lc]) as u32, LEN_EXTRA[lc] as u32);
			let dc = dist_code(s.dist as usize);
			w.bits(dist_codes[dc] as u32, dist_lens[dc] as u32);
			w.bits((s.dist - DIST_BASE[dc]) as u32, DIST_EXTRA[dc] as u32);
		}
	}
	w.bits(lit_codes[END_OF_BLOCK] as u32, lit_lens[END_OF_BLOCK] as u32);
}

/// A dynamic block's header: the code lengths of both codes, run-length encoded and then
/// Huffman coded themselves.
struct DynamicHeader {
	hlit: usize,
	hdist: usize,
	/// (code-length symbol 0..=18, its extra bits' value)
	runs: Vec<(u8, u8)>,
	cl_lens: [u8; 19],
	hclen: usize,
}

impl DynamicHeader {
	fn new(lit_lens: &[u8], dist_lens: &[u8]) -> Self {
		let used = |l: &[u8], min: usize| l.iter().rposition(|&x| x != 0).map_or(min, |p| (p + 1).max(min));
		let hlit = used(lit_lens, 257);
		let hdist = used(dist_lens, 1);
		let all: Vec<u8> = lit_lens[..hlit].iter().chain(&dist_lens[..hdist]).copied().collect();
		let runs = run_lengths(&all);
		let mut freq = [0u32; 19];
		for &(sym, _) in &runs {
			freq[sym as usize] += 1;
		}
		let cl_lens = code_lengths(&freq, 7);
		let hclen = CL_ORDER.iter().rposition(|&s| cl_lens[s] != 0).map_or(4, |p| (p + 1).max(4));
		DynamicHeader { hlit, hdist, runs, cl_lens, hclen }
	}

	fn bits(&self) -> usize {
		let runs: usize = self.runs.iter().map(|&(s, _)| self.cl_lens[s as usize] as usize + extra_bits(s) as usize).sum();
		5 + 5 + 4 + 3 * self.hclen + runs
	}

	fn write(&self, w: &mut BitWriter) {
		w.bits((self.hlit - 257) as u32, 5);
		w.bits((self.hdist - 1) as u32, 5);
		w.bits((self.hclen - 4) as u32, 4);
		for &s in &CL_ORDER[..self.hclen] {
			w.bits(self.cl_lens[s] as u32, 3);
		}
		let codes = canonical_codes(&self.cl_lens);
		for &(s, extra) in &self.runs {
			w.bits(codes[s as usize] as u32, self.cl_lens[s as usize] as u32);
			w.bits(extra as u32, extra_bits(s) as u32);
		}
	}
}

fn extra_bits(cl_sym: u8) -> u8 {
	match cl_sym {
		16 => 2,
		17 => 3,
		18 => 7,
		_ => 0,
	}
}

/// Run-length encode code lengths: 16 repeats the previous length 3-6 times, 17 and 18
/// give 3-10 and 11-138 zeros.
fn run_lengths(lens: &[u8]) -> Vec<(u8, u8)> {
	let mut out = Vec::new();
	let mut i = 0;
	while i < lens.len() {
		let v = lens[i];
		let run = lens[i..].iter().take_while(|&&x| x == v).count();
		let mut left = run;
		if v == 0 {
			while left >= 11 {
				let n = left.min(138);
				out.push((18, (n - 11) as u8));
				left -= n;
			}
			if left >= 3 {
				out.push((17, (left - 3) as u8));
				left = 0;
			}
		} else {
			out.push((v, 0));
			left -= 1;
			while left >= 3 {
				let n = left.min(6);
				out.push((16, (n - 3) as u8));
				left -= n;
			}
		}
		out.extend(std::iter::repeat_n((v, 0), left));
		i += run;
	}
	out
}

/// Huffman code lengths for these frequencies, none longer than `limit`. Unused symbols get
/// length 0. At least two symbols always get a code, so every decoder accepts the code.
fn code_lengths<const N: usize>(freq: &[u32; N], limit: u8) -> [u8; N] {
	let mut f = freq.map(u64::from);
	for i in 0..N {
		if f.iter().filter(|&&x| x > 0).count() >= 2 {
			break;
		}
		if f[i] == 0 {
			f[i] = 1;
		}
	}
	loop {
		let lens = huffman_lengths(&f);
		if lens.iter().all(|&l| l <= limit) {
			return lens;
		}
		// Too deep: flatten the frequencies and try again. Halving (rounding up) ends at
		// all ones, a balanced tree, which fits any limit deflate uses.
		for x in f.iter_mut().filter(|x| **x > 0) {
			*x = x.div_ceil(2);
		}
	}
}

/// Optimal (unlimited) Huffman code lengths, using the two-queue method.
fn huffman_lengths<const N: usize>(freq: &[u64; N]) -> [u8; N] {
	let mut leaves: Vec<(u64, usize)> = freq.iter().enumerate().filter(|(_, f)| **f > 0).map(|(i, f)| (*f, i)).collect();
	leaves.sort_unstable();
	let n = leaves.len();
	let mut lens = [0u8; N];
	if n == 0 {
		return lens;
	}
	if n == 1 {
		lens[leaves[0].1] = 1;
		return lens;
	}
	// Nodes 0..n are the leaves, n.. the internal nodes, which are made in order of weight.
	let mut weight: Vec<u64> = leaves.iter().map(|l| l.0).chain(std::iter::repeat_n(0, n - 1)).collect();
	let mut parent = vec![0usize; 2 * n - 1];
	let (mut next_leaf, mut next_inner) = (0, n);
	for k in n..2 * n - 1 {
		let mut take = || {
			if next_leaf < n && (next_inner >= k || weight[next_leaf] <= weight[next_inner]) {
				next_leaf += 1;
				next_leaf - 1
			} else {
				next_inner += 1;
				next_inner - 1
			}
		};
		let (a, b) = (take(), take());
		weight[k] = weight[a] + weight[b];
		parent[a] = k;
		parent[b] = k;
	}
	let mut depth = vec![0u8; 2 * n - 1];
	for k in (0..2 * n - 2).rev() {
		depth[k] = depth[parent[k]] + 1;
	}
	for (x, &(_, sym)) in leaves.iter().enumerate() {
		lens[sym] = depth[x];
	}
	lens
}

/// Canonical Huffman codes for these lengths, bit-reversed, since deflate sends codes
/// starting from their first bit while the writer packs from the lowest bit.
fn canonical_codes(lens: &[u8]) -> Vec<u16> {
	let mut count = [0u16; 16];
	for &l in lens {
		count[l as usize] += 1;
	}
	count[0] = 0;
	let mut next = [0u16; 16];
	let mut code = 0u16;
	for bits in 1..16 {
		code = (code + count[bits - 1]) << 1;
		next[bits] = code;
	}
	lens
		.iter()
		.map(|&l| {
			if l == 0 {
				return 0;
			}
			let c = next[l as usize];
			next[l as usize] += 1;
			c.reverse_bits() >> (16 - l)
		})
		.collect()
}

/// Packs bits from the lowest bit of each byte up, as deflate does.
#[derive(Default)]
struct BitWriter {
	out: Vec<u8>,
	acc: u64,
	n: u32,
}

impl BitWriter {
	fn bits(&mut self, value: u32, count: u32) {
		self.acc |= (value as u64) << self.n;
		self.n += count;
		while self.n >= 8 {
			self.out.push(self.acc as u8);
			self.acc >>= 8;
			self.n -= 8;
		}
	}

	fn align(&mut self) {
		if self.n > 0 {
			self.out.push(self.acc as u8);
			self.acc = 0;
			self.n = 0;
		}
	}

	/// Whole bytes; the writer must be aligned.
	fn bytes(&mut self, b: &[u8]) {
		debug_assert_eq!(self.n, 0);
		self.out.extend_from_slice(b);
	}

	fn finish(mut self) -> Vec<u8> {
		self.align();
		self.out
	}
}

// ---------- decompression ----------

/// Decompress raw deflate data, refusing to produce more than `max_len` bytes. Bytes after
/// the final block are ignored.
pub fn inflate(data: &[u8], max_len: usize) -> Result<Vec<u8>> {
	inflate_counted(data, max_len).map(|(out, _)| out)
}

/// Like [`inflate`], also giving how many input bytes the deflate data took up, so a caller
/// can find what follows it (as in a git pack, which doesn't record compressed lengths).
pub fn inflate_counted(data: &[u8], max_len: usize) -> Result<(Vec<u8>, usize)> {
	inflate_until(data, max_len, usize::MAX)
}

/// At least the first `want` bytes the deflate data decompresses to (all of them, if there
/// are fewer), decoding no further than that: enough to read a header at the start of a large
/// stream without decompressing the rest. Errors past the point where it stops go unnoticed.
pub fn inflate_prefix(data: &[u8], want: usize, max_len: usize) -> Result<Vec<u8>> {
	inflate_until(data, max_len, want).map(|(out, _)| out)
}

/// How much output to make room for at the start: all of it when the limit is the size the
/// caller expects (as a git pack's entries give it), and no more than this when it's only a
/// bound.
const FIRST_RESERVE: usize = 1 << 16;

/// Decompress until the data ends or at least `stop` bytes are out, giving the bytes and how
/// many input bytes were read.
fn inflate_until(data: &[u8], max_len: usize, stop: usize) -> Result<(Vec<u8>, usize)> {
	let mut r = BitReader { data, pos: 0, buf: 0, n: 0 };
	let mut out = Vec::with_capacity(max_len.min(stop).min(FIRST_RESERVE));
	loop {
		let last = r.bits(1)? == 1;
		match r.bits(2)? {
			0 => {
				r.align();
				let len = r.bytes_u16()?;
				if r.bytes_u16()? != !len {
					return Err(Error::corrupt("a stored block's length check doesn't match"));
				}
				let bytes = r.take(len as usize)?;
				if out.len() + bytes.len() > max_len {
					return Err(Error::TooLarge { limit: max_len });
				}
				out.extend_from_slice(bytes);
			}
			1 => {
				let (lit, dist) = fixed_codes();
				inflate_block(&mut r, &mut out, lit, dist, max_len, stop)?;
			}
			2 => {
				let (lit, dist) = read_dynamic_codes(&mut r)?;
				inflate_block(&mut r, &mut out, &lit, &dist, max_len, stop)?;
			}
			_ => return Err(Error::corrupt("unknown deflate block type 3")),
		}
		if last || out.len() >= stop {
			return Ok((out, r.used()));
		}
	}
}

/// The fixed codes, built once.
fn fixed_codes() -> (&'static Huffman, &'static Huffman) {
	static FIXED: std::sync::OnceLock<(Huffman, Huffman)> = std::sync::OnceLock::new();
	let (lit, dist) = FIXED.get_or_init(|| {
		let lit = Huffman::new(&fixed_lit_lengths()).expect("the fixed literal code is valid").0;
		let dist = Huffman::new(&FIXED_DIST_LENGTHS).expect("the fixed distance code is valid").0;
		(lit, dist)
	});
	(lit, dist)
}

fn read_dynamic_codes(r: &mut BitReader) -> Result<(Huffman, Huffman)> {
	let hlit = r.bits(5)? as usize + 257;
	let hdist = r.bits(5)? as usize + 1;
	let hclen = r.bits(4)? as usize + 4;
	if hlit > 286 || hdist > 30 {
		return Err(Error::corrupt("too many codes in a dynamic block"));
	}
	let mut cl_lens = [0u8; 19];
	for &s in &CL_ORDER[..hclen] {
		cl_lens[s] = r.bits(3)? as u8;
	}
	let (cl, complete) = Huffman::new(&cl_lens)?;
	if !complete {
		return Err(Error::corrupt("incomplete code-length code"));
	}
	let mut lens = [0u8; 286 + 30];
	let lens = &mut lens[..hlit + hdist];
	let mut i = 0;
	while i < lens.len() {
		let sym = cl.decode(r)?;
		let (value, repeat) = match sym {
			0..=15 => (sym as u8, 1),
			16 => {
				let Some(&prev) = i.checked_sub(1).map(|p| &lens[p]) else {
					return Err(Error::corrupt("a repeat with no length before it"));
				};
				(prev, 3 + r.bits(2)? as usize)
			}
			17 => (0, 3 + r.bits(3)? as usize),
			_ => (0, 11 + r.bits(7)? as usize),
		};
		if i + repeat > lens.len() {
			return Err(Error::corrupt("code lengths run past the end"));
		}
		lens[i..i + repeat].fill(value);
		i += repeat;
	}
	if lens[END_OF_BLOCK] == 0 {
		return Err(Error::corrupt("no end-of-block code"));
	}
	let lit = huffman_allowing_single(&lens[..hlit])?;
	let dist = huffman_allowing_single(&lens[hlit..])?;
	Ok((lit, dist))
}

/// An incomplete code is only allowed where the format needs one: no codes at all (a block
/// with no distances) or a single code of length 1.
fn huffman_allowing_single(lens: &[u8]) -> Result<Huffman> {
	let (h, complete) = Huffman::new(lens)?;
	let used: u16 = h.count[1..].iter().sum();
	if !complete && !(used == 0 || used == 1 && h.count[1] == 1) {
		return Err(Error::corrupt("incomplete Huffman code"));
	}
	Ok(h)
}

/// Decode one block into `out`, returning early once it holds at least `stop` bytes.
fn inflate_block(r: &mut BitReader, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, max_len: usize, stop: usize) -> Result<()> {
	loop {
		if out.len() >= stop {
			return Ok(());
		}
		let sym = lit.decode(r)? as usize;
		if sym < 256 {
			if out.len() >= max_len {
				return Err(Error::TooLarge { limit: max_len });
			}
			out.push(sym as u8);
			continue;
		}
		if sym == END_OF_BLOCK {
			return Ok(());
		}
		let lc = sym - 257;
		if lc >= 29 {
			return Err(Error::corrupt("invalid length code"));
		}
		let len = LEN_BASE[lc] as usize + r.bits(LEN_EXTRA[lc] as u32)? as usize;
		let dc = dist.decode(r)? as usize;
		if dc >= 30 {
			return Err(Error::corrupt("invalid distance code"));
		}
		let d = DIST_BASE[dc] as usize + r.bits(DIST_EXTRA[dc] as u32)? as usize;
		if d > out.len() {
			return Err(Error::corrupt("a repeat reaches back before the start"));
		}
		if out.len() + len > max_len {
			return Err(Error::TooLarge { limit: max_len });
		}
		let start = out.len() - d;
		if d >= len {
			out.extend_from_within(start..start + len);
		} else if d == 1 {
			let b = out[start];
			out.resize(out.len() + len, b);
		} else {
			// Overlapping: the repeat is the last `d` bytes over and over.
			let mut left = len;
			while left > 0 {
				let n = left.min(d);
				out.extend_from_within(start..start + n);
				left -= n;
			}
		}
	}
}

/// How many bits of a code [`Huffman::decode`] looks up at once; longer codes are decoded a
/// bit at a time past these.
const TABLE_BITS: u32 = 10;
/// The longest code the format allows.
const MAX_CODE_BITS: u32 = 15;

/// A canonical Huffman code for decoding: how many codes each length has, the symbols in code
/// order, and a table of the codes no longer than [`TABLE_BITS`], as zlib's inflate keeps one.
struct Huffman {
	count: [u16; 16],
	symbols: Vec<u16>,
	/// By the next `table_bits` bits of input: the symbol a code of that many bits or fewer
	/// starts with them (shifted up 4) and the code's length (the low 4 bits), or 0 if no code
	/// that short does.
	table: Vec<u16>,
	/// `TABLE_BITS`, or fewer if no code is that long.
	table_bits: u32,
}

impl Huffman {
	/// The code for these lengths, and whether it's complete (uses every bit pattern).
	fn new(lens: &[u8]) -> Result<(Huffman, bool)> {
		let mut count = [0u16; 16];
		for &l in lens {
			count[l as usize] += 1;
		}
		let mut left: i32 = 1;
		for &c in &count[1..] {
			left = (left << 1) - c as i32;
			if left < 0 {
				return Err(Error::corrupt("over-subscribed Huffman code"));
			}
		}
		let mut offset = [0u16; 16];
		for l in 1..15 {
			offset[l + 1] = offset[l] + count[l];
		}
		let mut symbols = vec![0u16; lens.len()];
		for (sym, &l) in lens.iter().enumerate() {
			if l != 0 {
				symbols[offset[l as usize] as usize] = sym as u16;
				offset[l as usize] += 1;
			}
		}
		// Each short code, its bits reversed (the input is read lowest bit first), fills every
		// entry whose low bits are it. The table is built a length at a time, as libdeflate
		// builds its: doubled, which repeats each shorter code's entries, then each code of
		// the new length put in its one place.
		let longest = (1..=MAX_CODE_BITS).rev().find(|&l| count[l as usize] != 0).unwrap_or(1);
		let table_bits = longest.min(TABLE_BITS);
		let mut table = Vec::with_capacity(1 << table_bits);
		table.extend_from_slice(&[0u16; 2]);
		let mut code = 0u32;
		let mut index = 0usize;
		for len in 1..=table_bits {
			if len > 1 {
				table.extend_from_within(..);
			}
			for _ in 0..count[len as usize] {
				let reversed = code.reverse_bits() >> (32 - len);
				table[reversed as usize] = symbols[index] << 4 | len as u16;
				code += 1;
				index += 1;
			}
			code <<= 1;
		}
		Ok((Huffman { count, symbols, table, table_bits }, left == 0))
	}

	#[inline]
	fn decode(&self, r: &mut BitReader) -> Result<u16> {
		r.fill(MAX_CODE_BITS);
		let entry = self.table[(r.buf & ((1 << self.table_bits) - 1)) as usize];
		if entry != 0 {
			r.consume(u32::from(entry & 15))?;
			return Ok(entry >> 4);
		}
		self.decode_long(r)
	}

	/// A code longer than the table holds (or not a code at all), a bit at a time.
	fn decode_long(&self, r: &mut BitReader) -> Result<u16> {
		let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
		for len in 1..=MAX_CODE_BITS {
			code |= ((r.buf >> (len - 1)) & 1) as i32;
			let count = self.count[len as usize] as i32;
			if code - first < count {
				r.consume(len)?;
				return Ok(self.symbols[(index + code - first) as usize]);
			}
			index += count;
			first = (first + count) << 1;
			code <<= 1;
		}
		if r.n < MAX_CODE_BITS {
			return Err(cut_short());
		}
		Err(Error::corrupt("a code that isn't in the Huffman code"))
	}
}

fn cut_short() -> Error {
	Error::corrupt("the deflate data is cut short")
}

/// Reads bits from the lowest bit of each byte up, a word at a time. `buf` holds `n` bits not
/// yet read; any bits above them are the input's next bits too, or zeros past its end.
struct BitReader<'a> {
	data: &'a [u8],
	/// The next byte not yet in `buf`.
	pos: usize,
	buf: u64,
	n: u32,
}

impl BitReader<'_> {
	/// Make `buf` hold at least `want` bits (at most 56), if there are that many left.
	#[inline]
	fn fill(&mut self, want: u32) {
		if self.n >= want {
			return;
		}
		if let Some(word) = self.data.get(self.pos..self.pos + 8) {
			let word = u64::from_le_bytes(word.try_into().expect("an 8-byte slice"));
			self.buf |= word << self.n;
			let bytes = (63 - self.n) / 8;
			self.pos += bytes as usize;
			self.n += bytes * 8;
			return;
		}
		while self.n <= 56 {
			let Some(&b) = self.data.get(self.pos) else { break };
			self.buf |= u64::from(b) << self.n;
			self.pos += 1;
			self.n += 8;
		}
	}

	/// Drop `count` bits already looked at.
	#[inline]
	fn consume(&mut self, count: u32) -> Result<()> {
		if count > self.n {
			return Err(cut_short());
		}
		self.buf >>= count;
		self.n -= count;
		Ok(())
	}

	#[inline]
	fn bits(&mut self, count: u32) -> Result<u32> {
		self.fill(count);
		let v = (self.buf & ((1u64 << count) - 1)) as u32;
		self.consume(count)?;
		Ok(v)
	}

	/// Skip to the next byte boundary, and give back the whole bytes read ahead, so the next
	/// unread byte is at `pos`.
	fn align(&mut self) {
		self.pos -= (self.n / 8) as usize;
		self.buf = 0;
		self.n = 0;
	}

	/// How many input bytes have been read, a byte partly read counting as read.
	fn used(&self) -> usize {
		self.pos - (self.n / 8) as usize
	}

	fn take(&mut self, len: usize) -> Result<&[u8]> {
		let bytes = self.data.get(self.pos..self.pos + len).ok_or_else(|| Error::corrupt("a stored block is cut short"))?;
		self.pos += len;
		Ok(bytes)
	}

	fn bytes_u16(&mut self) -> Result<u16> {
		let b = self.take(2)?;
		Ok(u16::from_le_bytes([b[0], b[1]]))
	}
}

/// The named presets and every numeric level, for tests to loop over.
#[cfg(test)]
pub(crate) fn every_level() -> Vec<Compression> {
	let mut levels = vec![Compression::Fast, Compression::Default, Compression::Best];
	levels.extend((0..=10).map(Compression::Level));
	levels
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn level_zero_stores() {
		let data = noise(200_000, 5);
		let raw = deflate(&data, Compression::Level(0));
		// Four stored blocks, 5 header bytes each.
		assert_eq!(raw.len(), data.len() + 4 * 5);
		assert_eq!(inflate(&raw, data.len()).unwrap(), data);
		assert_eq!(deflate(b"", Compression::Level(0)), [0x01, 0x00, 0x00, 0xFF, 0xFF]);
		// Even repeats aren't searched for.
		assert_eq!(deflate(&[7; 1000], Compression::Level(0)).len(), 1005);
	}

	#[test]
	fn higher_levels_are_no_bigger() {
		let text: Vec<u8> = (0..3000u32).flat_map(|i| format!("line {} of {}; ", i % 97, i % 13).into_bytes()).collect();
		for data in [screenish(300, 200), text] {
			let sizes: Vec<usize> = (1..=9).map(|n| deflate(&data, Compression::Level(n)).len()).collect();
			// Allow a little noise between neighbouring levels, but the ends must be ordered.
			assert!(sizes[8] <= sizes[0], "level sizes {sizes:?}");
			assert!(sizes[8] < data.len() / 4, "level sizes {sizes:?} of {}", data.len());
		}
		assert_eq!(deflate(b"x", Compression::Level(200)), deflate(b"x", Compression::Level(9)));
	}

	#[test]
	fn counts_what_it_used() {
		let mut d = deflate(b"hello hello hello", Compression::Default);
		let len = d.len();
		d.extend(b"trailing");
		assert_eq!(inflate_counted(&d, 100).unwrap(), (b"hello hello hello".to_vec(), len));
	}

	/// Deterministic noise, which doesn't compress.
	fn noise(len: usize, mut seed: u64) -> Vec<u8> {
		(0..len)
			.map(|_| {
				seed ^= seed << 13;
				seed ^= seed >> 7;
				seed ^= seed << 17;
				seed as u8
			})
			.collect()
	}

	/// Something like a screenshot: flat areas, gradients, repeated rows, a little noise.
	fn screenish(width: usize, height: usize) -> Vec<u8> {
		let speck = noise(width * height, 7);
		let mut out = Vec::with_capacity(width * height * 4);
		for y in 0..height {
			for x in 0..width {
				let px = if y < height / 4 {
					[240, 240, 240, 255]
				} else if x < width / 3 {
					[(x * 255 / width) as u8, (y * 255 / height) as u8, 128, 255]
				} else if speck[y * width + x] < 8 {
					[speck[y * width + x], 0, 0, 255]
				} else {
					[30, 30, 30, 255]
				};
				out.extend(px);
			}
		}
		out
	}

	fn samples() -> Vec<(&'static str, Vec<u8>)> {
		let text = "the quick brown fox jumps over the lazy dog; ".repeat(400).into_bytes();
		vec![
			("empty", vec![]),
			("one byte", vec![42]),
			("two bytes", vec![1, 2]),
			("hello", b"hello".to_vec()),
			("zeros", vec![0; 1 << 20]),
			("text", text),
			("noise", noise(200_000, 1)),
			("short noise", noise(100, 3)),
			("screen", screenish(300, 200)),
			("all bytes", (0..=255u8).cycle().take(70_000).collect()),
		]
	}

	#[test]
	fn round_trips_at_every_level() {
		for level in crate::deflate::every_level() {
			for (name, data) in samples() {
				let raw = deflate(&data, level);
				assert_eq!(inflate(&raw, data.len()).as_deref(), Ok(&data[..]), "{name} at {level:?}");
			}
		}
	}

	#[test]
	fn compresses() {
		assert!(deflate(&vec![0; 1 << 20], Compression::Default).len() < 2_000);
		let text = "the quick brown fox jumps over the lazy dog; ".repeat(400);
		assert!(deflate(text.as_bytes(), Compression::Default).len() < 300);
		let screen = screenish(300, 200);
		let best = deflate(&screen, Compression::Best).len();
		let fast = deflate(&screen, Compression::Fast).len();
		assert!(best <= fast && best < screen.len() / 10, "best {best}, fast {fast}, of {}", screen.len());
		// Noise falls back to stored blocks: barely bigger than the input.
		let n = noise(200_000, 1);
		assert!(deflate(&n, Compression::Default).len() < n.len() + 100);
	}

	#[test]
	fn reads_a_hand_made_stored_block() {
		assert_eq!(inflate(&[0x01, 0x05, 0x00, 0xFA, 0xFF, b'h', b'e', b'l', b'l', b'o'], 100).unwrap(), b"hello");
	}

	#[test]
	fn rejects_bad_data() {
		let corrupt = |r: Result<Vec<u8>>| matches!(r, Err(Error::Corrupt(_)));
		let d = deflate(b"hello hello hello", Compression::Default);
		assert!(corrupt(inflate(&d[..d.len() - 1], 100)), "cut short");
		assert!(corrupt(inflate(&[0x07], 100)), "block type 3");
		assert!(corrupt(inflate(&[0x01, 0x05, 0x00, 0xFA, 0xFE], 100)), "stored length check");
		assert!(corrupt(inflate(&[0x01, 0x05, 0x00, 0xFA, 0xFF, b'h'], 100)), "stored block cut short");
		// Fixed block: a length-3 match (code 257, 7 bits 0000001) at distance 1 with no
		// bytes before it.
		assert!(corrupt(inflate(&[0x03, 0x02, 0x00], 100)), "distance before the start");
		assert!(corrupt(inflate(&[], 100)));
	}

	#[test]
	fn stops_at_the_limit() {
		let z = deflate(&vec![0; 1 << 20], Compression::Default);
		assert_eq!(inflate(&z, 1000), Err(Error::TooLarge { limit: 1000 }));
		assert_eq!(inflate(&z, 1 << 20).map(|v| v.len()), Ok(1 << 20));
		let s = deflate(&noise(5000, 9), Compression::Default);
		assert_eq!(inflate(&s, 4999), Err(Error::TooLarge { limit: 4999 }));
	}

	#[test]
	fn decodes_codes_longer_than_the_table() {
		// Byte k appears about 1.6^k times, so the rarest get codes of 15 bits, past what
		// the decoder's table holds; the noise keeps them literals.
		let mut data = Vec::new();
		let mut weight = 1.0f64;
		for byte in 0..24u8 {
			data.extend(std::iter::repeat_n(byte, weight as usize));
			weight *= 1.6;
		}
		let order = noise(data.len(), 11);
		let mut keyed: Vec<(u8, u8)> = order.into_iter().zip(data).collect();
		keyed.sort_unstable();
		let data: Vec<u8> = keyed.into_iter().map(|(_, b)| b).collect();
		let mut freq = [0u32; 286];
		for &b in &data {
			freq[b as usize] += 1;
		}
		assert!(code_lengths(&freq, 15).iter().any(|&l| u32::from(l) > TABLE_BITS));
		let raw = deflate(&data, Compression::Fast);
		assert_eq!(inflate(&raw, data.len()).as_deref(), Ok(&data[..]));
		// Cut anywhere, it's an error and never a panic.
		for end in (0..raw.len()).step_by(97) {
			assert!(inflate(&raw[..end], data.len()).is_err());
		}
	}

	#[test]
	fn length_limited_codes() {
		// Fibonacci frequencies make the deepest possible Huffman tree.
		let mut fib = [1u32; 30];
		for i in 2..fib.len() {
			fib[i] = fib[i - 1] + fib[i - 2];
		}
		let lens = code_lengths(&fib, 15);
		assert!(lens.iter().all(|&l| (1..=15).contains(&l)));
		// Kraft: the code is complete.
		assert_eq!(lens.iter().map(|&l| 1u64 << (15 - l)).sum::<u64>(), 1 << 15);
		assert_eq!(code_lengths(&[0, 0, 5, 0], 7), [1, 0, 1, 0], "one used symbol gets a partner");
		assert_eq!(code_lengths(&[0, 0, 0], 7), [1, 1, 0]);
	}

	#[test]
	fn code_length_runs() {
		let lens = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 8, 8, 8, 8, 0, 0, 5];
		let runs = run_lengths(&lens);
		assert_eq!(runs, vec![(18, 1), (8, 0), (16, 3), (8, 0), (0, 0), (0, 0), (5, 0)]);
		let mut back = Vec::new();
		for &(s, e) in &runs {
			match s {
				16 => back.extend(std::iter::repeat_n(*back.last().unwrap(), 3 + e as usize)),
				17 => back.extend(std::iter::repeat_n(0, 3 + e as usize)),
				18 => back.extend(std::iter::repeat_n(0, 11 + e as usize)),
				v => back.push(v),
			}
		}
		assert_eq!(back, lens);
	}
}
