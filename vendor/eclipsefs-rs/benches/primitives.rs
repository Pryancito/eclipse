//! Reference benchmarks for the pieces every EclipseFS I/O is built from:
//! checksums, compression, RAID5/6 parity and encryption, each on its own,
//! with no tree, no allocator and no device.
//!
//! `tests/performance.rs` and `scripts/eclipsefs-linux-bench.sh` say what a
//! workload costs end to end. What they cannot say is how much of a 4 KiB
//! write is the CRC32C of the block, how much the parity of its row and how
//! much the cipher. These rows can: read them as the floor `benches/fs.rs`
//! sits on, and as the first place to look when a row there moves.
//!
//! Every family carries its control row:
//!
//! - `crc32c_*_table` is the portable slicing-by-8 code next to the SSE4.2
//!   instruction the driver uses on x86_64. Their ratio is what the
//!   instruction buys; if it is near 1 the hardware path is not being taken.
//! - `pack_*` and `unpack_*` run each algorithm on the same text, and on
//!   noise (which none of them can shrink): the noise row is what a write
//!   to a compressed file pays when compression gives up.
//! - `raid5_gen_*` (P only) against `raid6_gen_*` (P and Q): the difference
//!   is the cost of the second parity, and the `_4`/`_8` pairs show it grows
//!   with the row width.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the
//! reasons given in `zircon-object/benches/vm.rs`: no benchmarking crate in
//! the kernel's `Cargo.lock`. Rows that move data set `b.bytes`, so the
//! output has a MB/s column.
//!
//! Every row passes its inputs through `test::black_box` as well as its
//! result, so the compiler cannot fold the work away.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p eclipsefs --features std --bench primitives
//! ```

#![feature(test)]

extern crate test;

use eclipsefs::compress::{pack, unpack};
use eclipsefs::crc::{crc32, crc32c, crc32c_table};
use eclipsefs::crypt::FileKeys;
use eclipsefs::raid56::{gen, recover};
use eclipsefs::Compression;
use test::{black_box, Bencher};

const BLOCK: usize = 4096;

/// Deterministic noise (xorshift), so every run measures the same bytes.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Text that compresses about as well as source code does.
fn text(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut i = 0u32;
    while v.len() < len {
        v.extend_from_slice(
            format!("fn item_{}(x: u64) -> u64 {{ x * {} }}\n", i, i % 97).as_bytes(),
        );
        i += 1;
    }
    v.truncate(len);
    v
}

// ---------------------------------------------------------------------------
// Checksums: one per data block written or read, one per tree node.
// ---------------------------------------------------------------------------

#[bench]
fn crc32c_4k(b: &mut Bencher) {
    let d = noise(BLOCK);
    b.bytes = BLOCK as u64;
    b.iter(|| black_box(crc32c(black_box(&d))));
}

#[bench]
fn crc32c_4k_table(b: &mut Bencher) {
    let d = noise(BLOCK);
    b.bytes = BLOCK as u64;
    b.iter(|| black_box(crc32c_table(!0, black_box(&d))));
}

#[bench]
fn crc32c_128k(b: &mut Bencher) {
    let d = noise(32 * BLOCK);
    b.bytes = d.len() as u64;
    b.iter(|| black_box(crc32c(black_box(&d))));
}

#[bench]
fn crc32c_128k_table(b: &mut Bencher) {
    let d = noise(32 * BLOCK);
    b.bytes = d.len() as u64;
    b.iter(|| black_box(crc32c_table(!0, black_box(&d))));
}

/// The v0.3 format's checksum (plain CRC32, superblock and inode table).
#[bench]
fn crc32_4k(b: &mut Bencher) {
    let d = noise(BLOCK);
    b.bytes = BLOCK as u64;
    b.iter(|| black_box(crc32(black_box(&d))));
}

// ---------------------------------------------------------------------------
// Compression: a 128 KiB extent, the largest one a compressed write makes.
// ---------------------------------------------------------------------------

const EXTENT: usize = 32 * BLOCK;

fn bench_pack(b: &mut Bencher, algo: Compression, data: Vec<u8>) {
    b.bytes = data.len() as u64;
    b.iter(|| black_box(pack(black_box(algo), black_box(&data))));
}

fn bench_unpack(b: &mut Bencher, algo: Compression) {
    let d = text(EXTENT);
    let p = pack(algo, &d).expect("text compresses");
    b.bytes = d.len() as u64;
    b.iter(|| black_box(unpack(black_box(algo), black_box(&p), d.len()).unwrap()));
}

#[bench]
fn pack_zlib_text(b: &mut Bencher) {
    bench_pack(b, Compression::Zlib, text(EXTENT));
}

#[bench]
fn pack_lz4_text(b: &mut Bencher) {
    bench_pack(b, Compression::Lz4, text(EXTENT));
}

#[bench]
fn pack_zstd_text(b: &mut Bencher) {
    bench_pack(b, Compression::Zstd, text(EXTENT));
}

#[bench]
fn pack_zlib_noise(b: &mut Bencher) {
    bench_pack(b, Compression::Zlib, noise(EXTENT));
}

#[bench]
fn pack_lz4_noise(b: &mut Bencher) {
    bench_pack(b, Compression::Lz4, noise(EXTENT));
}

#[bench]
fn pack_zstd_noise(b: &mut Bencher) {
    bench_pack(b, Compression::Zstd, noise(EXTENT));
}

#[bench]
fn unpack_zlib(b: &mut Bencher) {
    bench_unpack(b, Compression::Zlib);
}

#[bench]
fn unpack_lz4(b: &mut Bencher) {
    bench_unpack(b, Compression::Lz4);
}

#[bench]
fn unpack_zstd(b: &mut Bencher) {
    bench_unpack(b, Compression::Zstd);
}

// ---------------------------------------------------------------------------
// RAID5/6 parity over one row of 4 KiB blocks.
// ---------------------------------------------------------------------------

fn row(k: usize) -> Vec<Vec<u8>> {
    (0..k)
        .map(|i| {
            let mut v = noise(BLOCK + i);
            v.drain(..i);
            v
        })
        .collect()
}

fn bench_gen(b: &mut Bencher, k: usize, raid6: bool) {
    let data = row(k);
    let refs: Vec<&[u8]> = data.iter().map(|v| &v[..]).collect();
    let (mut p, mut q) = (vec![0u8; BLOCK], vec![0u8; BLOCK]);
    b.bytes = (k * BLOCK) as u64;
    b.iter(|| {
        gen(black_box(&refs), &mut p, raid6.then_some(&mut q[..]));
        black_box((&p, &q));
    });
}

#[bench]
fn raid5_gen_4(b: &mut Bencher) {
    bench_gen(b, 4, false);
}

#[bench]
fn raid5_gen_8(b: &mut Bencher) {
    bench_gen(b, 8, false);
}

#[bench]
fn raid6_gen_4(b: &mut Bencher) {
    bench_gen(b, 4, true);
}

#[bench]
fn raid6_gen_8(b: &mut Bencher) {
    bench_gen(b, 8, true);
}

/// Rebuilding blocks of a degraded row: one lost (P is enough) and two lost
/// data blocks (needs Q, the slowest case `recover` has).
fn bench_recover(b: &mut Bencher, missing: &[usize]) {
    let k = 6;
    let mut good = row(k);
    let (mut p, mut q) = (vec![0u8; BLOCK], vec![0u8; BLOCK]);
    {
        let refs: Vec<&[u8]> = good.iter().map(|v| &v[..]).collect();
        gen(&refs, &mut p, Some(&mut q));
    }
    good.push(p);
    good.push(q);
    let mut r = good.clone();
    b.bytes = (missing.len() * BLOCK) as u64;
    b.iter(|| {
        recover(black_box(&mut r), k, black_box(missing)).unwrap();
        black_box(&r);
    });
    assert_eq!(r, good);
}

#[bench]
fn raid6_recover_one_data(b: &mut Bencher) {
    bench_recover(b, &[2]);
}

#[bench]
fn raid6_recover_two_data(b: &mut Bencher) {
    bench_recover(b, &[1, 4]);
}

// ---------------------------------------------------------------------------
// Encryption: AES-256-XTS per data block, AES-256-CTS per name.
// ---------------------------------------------------------------------------

fn keys() -> FileKeys {
    FileKeys::new(&[0x42; 64], &[7; 16])
}

#[bench]
fn xts_encrypt_4k(b: &mut Bencher) {
    let k = keys();
    let mut d = noise(BLOCK);
    b.bytes = BLOCK as u64;
    b.iter(|| {
        k.encrypt_blocks(black_box(9), &mut d, BLOCK);
        black_box(&d);
    });
}

#[bench]
fn xts_decrypt_4k(b: &mut Bencher) {
    let k = keys();
    let mut d = noise(BLOCK);
    b.bytes = BLOCK as u64;
    b.iter(|| {
        k.decrypt_blocks(black_box(9), &mut d, BLOCK);
        black_box(&d);
    });
}

#[bench]
fn cts_encrypt_name(b: &mut Bencher) {
    let k = keys();
    let name = b"Cargo.toml.orig";
    b.iter(|| black_box(k.encrypt_name(black_box(name), 32, 255)));
}

/// The per-file key derivation (HKDF-SHA512 and three AES key schedules):
/// what the first access to each encrypted inode pays once.
#[bench]
fn derive_file_keys(b: &mut Bencher) {
    let master = [0x42u8; 64];
    let nonce = [7u8; 16];
    b.iter(|| black_box(FileKeys::new(black_box(&master), black_box(&nonce))));
}
