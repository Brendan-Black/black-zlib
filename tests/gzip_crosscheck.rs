//! Checks this crate against another implementation: the system's `gzip`. Files compressed
//! here must come out of `gzip -d` unchanged, and files gzip compressed must decompress
//! here. Skipped (passing) where gzip isn't installed, as on Windows.

use std::io::Write;
use std::process::{Command, Stdio};

use black_zlib::{Compression, gzip};

fn gzip_tool(args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
	let mut child = Command::new("gzip").args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
	let mut stdin = child.stdin.take().expect("stdin was piped");
	let input = input.to_vec();
	// Write on another thread so a full output pipe can't deadlock us.
	let writer = std::thread::spawn(move || stdin.write_all(&input));
	let out = child.wait_with_output().expect("waiting for the tool");
	writer.join().expect("the writer thread").expect("writing the tool's input");
	assert!(out.status.success(), "gzip {args:?} failed");
	Some(out.stdout)
}

fn have_gzip() -> bool {
	let found = gzip_tool(&["--version"], b"").is_some();
	if !found {
		eprintln!("gzip isn't installed; skipping");
	}
	found
}

fn samples() -> Vec<Vec<u8>> {
	let mut seed = 0x0123_4567_89AB_CDEF_u64;
	let noise: Vec<u8> = (0..150_000)
		.map(|_| {
			seed ^= seed << 13;
			seed ^= seed >> 7;
			seed ^= seed << 17;
			seed as u8
		})
		.collect();
	let mut mixed = Vec::new();
	for i in 0..60_000u32 {
		mixed.extend(if i % 700 < 500 { [200, 200, 200, 255] } else { [(i % 251) as u8, (i / 300) as u8, 7, 255] });
	}
	vec![
		vec![],
		b"a".to_vec(),
		"hello hello hello, said the echo; ".repeat(2000).into_bytes(),
		vec![0; 3 << 20],
		noise,
		mixed,
		(0..=255u8).cycle().take(300_000).collect(),
	]
}

#[test]
fn gzip_reads_ours() {
	if !have_gzip() {
		return;
	}
	for data in samples() {
		for level in [Compression::Fast, Compression::Default, Compression::Best] {
			let back = gzip_tool(&["-dc"], &gzip::compress(&data, level)).unwrap();
			assert!(back == data, "{} bytes at {level:?}", data.len());
		}
	}
}

#[test]
fn we_read_gzips() {
	if !have_gzip() {
		return;
	}
	for data in samples() {
		for level in ["-1", "-6", "-9"] {
			let gz = gzip_tool(&["-c", level], &data).unwrap();
			assert!(gzip::decompress(&gz, data.len()).unwrap() == data, "{} bytes at gzip {level}", data.len());
		}
	}
	// Two members, as `cat a.gz b.gz` makes.
	let mut two = gzip_tool(&["-c"], b"first, ").unwrap();
	two.extend(gzip_tool(&["-c"], b"second").unwrap());
	assert_eq!(gzip::decompress(&two, 100).unwrap(), b"first, second");
}
