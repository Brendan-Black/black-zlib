# black-zlib

Deflate compression and decompression with **no dependencies**, in each of the wrappers
it's found in. Works on any platform; a sibling of
[black-serde](https://github.com/Brendan-Black/black-serde) and the compression under
black-png.

| Module | What it does |
| --- | --- |
| `deflate` | Raw deflate data (RFC 1951), as zip stores it: `deflate` and `inflate` |
| `zlib` | zlib streams (RFC 1950), as PNG stores its pixels and git its objects: `compress` and `decompress`, checking the Adler-32; `decompress_counted` also says where the stream ended, for formats like git's packs that put streams back to back without recording their lengths |
| `gzip` | gzip data (RFC 1952), as `.gz` files and HTTP `Content-Encoding: gzip` use: `compress` and `decompress`, reading every header field and several members in a row, checking each CRC-32 and length |
| `checksum` | CRC-32 and Adler-32 |

Compression has three named presets, `Fast`, `Default` and `Best`, which come out about the
size of gzip's at the matching level, and zlib's numeric scale as `Level(0)` to `Level(9)`,
the scale git's `core.compression` uses. `Level(0)` stores the data uncompressed. Levels 1 to 9
search the way zlib's levels of the same number do, and come out within a few percent of
zlib's size at each one. Every decompression call takes a size limit, so damaged
or hostile data can't exhaust memory; damaged data is an error, never a panic.

```rust
use black_zlib::{gzip, Compression};

let packed = gzip::compress(b"hello hello hello", Compression::Default);
assert_eq!(gzip::decompress(&packed, 1 << 20)?, b"hello hello hello");
```

## Tests

`cargo test` runs everywhere. `tests/gzip_crosscheck.rs` also checks this crate against the
system's `gzip` both ways, and skips itself where gzip isn't installed (as on Windows).
`tests/git_crosscheck.rs` checks the zlib module against the real zlib, by way of git, which
links it: git's loose objects at every level must decompress here, and git must read and
`fsck` the streams compressed here. It skips itself where git isn't installed. Run it with
`-- --nocapture` to print our sizes against zlib's at each level.
From WSL, `winrun --test` builds the tests and runs them on Windows.
