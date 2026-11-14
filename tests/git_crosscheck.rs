//! Checks the zlib module against the real zlib, by way of git, which links it and stores each
//! loose object as one zlib stream at the level `core.compression` sets. Streams git wrote at
//! every level must decompress here, git must read the streams written here, and the two
//! must start with the same header. Skipped (passing) where git isn't installed.
//!
//! `cargo test --test git_crosscheck -- --nocapture` also prints how our sizes compare with
//! zlib's at each level.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use black_zlib::{Compression, zlib};

/// Run git in `repo`, feeding it `input`, and return its output.
fn git(repo: &Path, args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    // Write on another thread so a full output pipe can't deadlock us.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    Some(out.stdout)
}

/// A fresh bare repository, removed when dropped.
struct Repo(PathBuf);

impl Repo {
    /// `None` where git isn't installed.
    fn new() -> Option<Repo> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("black-zlib-git-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo(dir);
        if git(&repo.0, &["init", "--quiet", "--bare"], b"").is_none() {
            eprintln!("git isn't installed; skipping");
            return None;
        }
        Some(repo)
    }

    /// Where git keeps the loose object `id` (in hex).
    fn object_path(&self, id: &str) -> PathBuf {
        self.0.join("objects").join(&id[..2]).join(&id[2..])
    }

    /// Store `data` as a blob the way git does at `level` (-1 to 9); return its id.
    fn git_write(&self, data: &[u8], level: i32) -> String {
        let setting = format!("core.compression={level}");
        let out = git(&self.0, &["-c", &setting, "hash-object", "-w", "--stdin"], data).unwrap();
        String::from_utf8(out).unwrap().trim().to_string()
    }

    /// The id git gives `data` as a blob, without storing it.
    fn blob_id(&self, data: &[u8]) -> String {
        let out = git(&self.0, &["hash-object", "--stdin"], data).unwrap();
        String::from_utf8(out).unwrap().trim().to_string()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A loose object's uncompressed bytes: `blob <size>\0` and the data.
fn loose_blob(data: &[u8]) -> Vec<u8> {
    let mut v = format!("blob {}\0", data.len()).into_bytes();
    v.extend_from_slice(data);
    v
}

fn samples() -> Vec<(&'static str, Vec<u8>)> {
    let mut seed = 0x0123_4567_89AB_CDEF_u64;
    let noise: Vec<u8> = (0..150_000)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect();
    // Something shaped like source code: repeated structure with varying names and numbers.
    let mut source = String::new();
    for i in 0..4000u32 {
        source += &format!("    let value_{} = compute(&table[{}], {}) + offset_{};\n", i % 37, i % 211, i * 7 % 1000, i % 5);
        if i % 9 == 0 {
            source += "    // keep going until every entry has been handled\n";
        }
    }
    let mut mixed = Vec::new();
    for i in 0..60_000u32 {
        mixed.extend(if i % 700 < 500 { [200, 200, 200, 255] } else { [(i % 251) as u8, (i / 300) as u8, 7, 255] });
    }
    vec![
        ("empty", vec![]),
        ("one byte", b"a".to_vec()),
        ("echo", "hello hello hello, said the echo; ".repeat(2000).into_bytes()),
        ("zeros", vec![0; 1 << 20]),
        ("noise", noise),
        ("source", source.into_bytes()),
        ("pixels", mixed),
    ]
}

#[test]
fn reads_what_zlib_wrote_at_every_level() {
    let Some(repo) = Repo::new() else { return };
    for (name, data) in samples() {
        let want = loose_blob(&data);
        for level in -1..=9 {
            let id = repo.git_write(&data, level);
            let file = std::fs::read(repo.object_path(&id)).unwrap();
            let (got, used) = zlib::decompress_counted(&file, want.len()).unwrap();
            assert!(got == want, "{name} at level {level}: wrong bytes");
            assert_eq!(used, file.len(), "{name} at level {level}: stream length");
            std::fs::remove_file(repo.object_path(&id)).unwrap();
        }
    }
}

#[test]
fn zlib_reads_what_we_wrote_at_every_level() {
    let Some(repo) = Repo::new() else { return };
    let mut levels = vec![Compression::Fast, Compression::Default, Compression::Best];
    levels.extend((0..=9).map(Compression::Level));
    for (name, data) in samples() {
        let id = repo.blob_id(&data);
        let path = repo.object_path(&id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for level in &levels {
            std::fs::write(&path, zlib::compress(&loose_blob(&data), *level)).unwrap();
            let got = git(&repo.0, &["cat-file", "blob", &id], b"").unwrap();
            assert!(got == data, "{name} at {level:?}: git read back different bytes");
            // fsck inflates the object and checks that it hashes to its name.
            git(&repo.0, &["fsck", "--strict", "--no-dangling"], b"").unwrap();
        }
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn headers_match_zlibs() {
    let Some(repo) = Repo::new() else { return };
    for level in 0..=9 {
        let data = format!("level {level}").into_bytes();
        let id = repo.git_write(&data, level);
        let theirs = std::fs::read(repo.object_path(&id)).unwrap();
        let ours = zlib::compress(&loose_blob(&data), Compression::Level(level as u8));
        assert_eq!(ours[..2], theirs[..2], "level {level}");
    }
}

/// Our size at each level against zlib's, on all the samples together. Run with
/// `--nocapture` to see the table. Fails only if we come out far bigger than zlib.
#[test]
fn sizes_against_zlib() {
    let Some(repo) = Repo::new() else { return };
    let samples = samples();
    eprintln!("level   zlib bytes   ours bytes   ours/zlib");
    for level in 0..=9 {
        let (mut theirs, mut ours) = (0usize, 0usize);
        for (_, data) in &samples {
            let id = repo.git_write(data, level);
            theirs += std::fs::metadata(repo.object_path(&id)).unwrap().len() as usize;
            std::fs::remove_file(repo.object_path(&id)).unwrap();
            ours += zlib::compress(&loose_blob(data), Compression::Level(level as u8)).len();
        }
        let ratio = ours as f64 / theirs as f64;
        eprintln!("{level:>5} {theirs:>12} {ours:>12} {ratio:>11.3}");
        assert!(ratio < 1.10, "level {level}: {ours} bytes against zlib's {theirs}");
    }
}
