//! Transparent compression of file data (format v0.4): zlib (as on btrfs),
//! LZ4 and zstd.
//!
//! A compressed extent holds up to [`ZCHUNK`] file blocks. On disk it is
//! `[stream length u32 | stream]`, padded to whole blocks; it is only kept
//! when that saves at least one block.

use alloc::vec;
use alloc::vec::Vec;

use crate::format::BLOCK_SIZE;
use crate::{Error, Result};

/// File blocks per compressed extent (128 KiB, as on btrfs).
pub const ZCHUNK: u64 = 32;

const BS: usize = BLOCK_SIZE as usize;

/// A compression algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compression {
    Zlib,
    Lz4,
    Zstd,
}

impl Compression {
    pub const ALL: [Compression; 3] = [Compression::Zlib, Compression::Lz4, Compression::Zstd];

    /// On-disk id (zlib and zstd keep their btrfs numbers).
    pub fn to_disk(self) -> u8 {
        match self {
            Compression::Zlib => 1,
            Compression::Zstd => 3,
            Compression::Lz4 => 4,
        }
    }

    pub fn from_disk(v: u8) -> Option<Self> {
        match v {
            1 => Some(Compression::Zlib),
            3 => Some(Compression::Zstd),
            4 => Some(Compression::Lz4),
            _ => None,
        }
    }

    /// Name as in `compress=` and `btrfs property set ... compression`.
    pub fn name(self) -> &'static str {
        match self {
            Compression::Zlib => "zlib",
            Compression::Lz4 => "lz4",
            Compression::Zstd => "zstd",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == s)
    }
}

fn stream(algo: Compression, data: &[u8]) -> Vec<u8> {
    match algo {
        Compression::Zlib => miniz_oxide::deflate::compress_to_vec_zlib(data, 3),
        Compression::Lz4 => lz4_flex::block::compress(data),
        Compression::Zstd => {
            ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
        }
    }
}

/// `data` (whole blocks) as a compressed extent, if that takes fewer blocks.
pub fn pack(algo: Compression, data: &[u8]) -> Option<Vec<u8>> {
    let blocks = data.len() / BS;
    if blocks < 2 {
        return None;
    }
    let s = stream(algo, data);
    let need = (4 + s.len()).div_ceil(BS);
    if need >= blocks {
        return None;
    }
    let mut out = vec![0u8; need * BS];
    out[..4].copy_from_slice(&(s.len() as u32).to_le_bytes());
    out[4..4 + s.len()].copy_from_slice(&s);
    Some(out)
}

const BAD: Error = Error::Corrupt("compressed extent");

/// The `ram` bytes a compressed extent (`disk`, whole blocks) holds.
pub fn unpack(algo: Compression, disk: &[u8], ram: usize) -> Result<Vec<u8>> {
    if disk.len() < 4 {
        return Err(BAD);
    }
    let len = u32::from_le_bytes(disk[..4].try_into().unwrap()) as usize;
    let s = disk.get(4..4 + len).ok_or(BAD)?;
    let out = match algo {
        Compression::Zlib => {
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(s, ram).map_err(|_| BAD)?
        }
        Compression::Lz4 => {
            let mut out = vec![0u8; ram];
            let n = lz4_flex::block::decompress_into(s, &mut out).map_err(|_| BAD)?;
            out.truncate(n);
            out
        }
        Compression::Zstd => {
            let mut out = vec![0u8; ram];
            let n = ruzstd::decoding::FrameDecoder::new()
                .decode_all(s, &mut out)
                .map_err(|_| BAD)?;
            out.truncate(n);
            out
        }
    };
    if out.len() != ram {
        return Err(BAD);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(blocks: usize) -> Vec<u8> {
        let mut v = Vec::new();
        let mut i = 0u32;
        while v.len() < blocks * BS {
            v.extend_from_slice(b"line ");
            v.extend_from_slice(&i.to_le_bytes());
            v.extend_from_slice(b" of some compressible text\n");
            i += 1;
        }
        v.truncate(blocks * BS);
        v
    }

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

    #[test]
    fn every_algorithm_round_trips() {
        for algo in Compression::ALL {
            for blocks in [2, 3, 17, 32] {
                let d = text(blocks);
                let p = pack(algo, &d).unwrap();
                assert!(p.len() < d.len() && p.len().is_multiple_of(BS), "{:?}", algo);
                assert_eq!(unpack(algo, &p, d.len()).unwrap(), d, "{:?}", algo);
            }
            let zeros = vec![0u8; 32 * BS];
            let p = pack(algo, &zeros).unwrap();
            assert_eq!(p.len(), BS, "{:?}", algo);
            assert_eq!(unpack(algo, &p, zeros.len()).unwrap(), zeros);
        }
    }

    #[test]
    fn incompressible_or_single_blocks_are_not_packed() {
        for algo in Compression::ALL {
            assert!(pack(algo, &noise(8 * BS)).is_none(), "{:?}", algo);
            assert!(pack(algo, &vec![0u8; BS]).is_none());
        }
    }

    #[test]
    fn damaged_extents_are_errors_not_panics() {
        for algo in Compression::ALL {
            let d = text(16);
            let p = pack(algo, &d).unwrap();
            assert!(unpack(algo, &p, d.len() + BS).is_err());
            assert!(unpack(algo, &p, d.len() - BS).is_err());
            assert!(unpack(algo, &p[..2], d.len()).is_err());
            let mut long = p.clone();
            long[..4].copy_from_slice(&(u32::MAX).to_le_bytes());
            assert!(unpack(algo, &long, d.len()).is_err());
            // Flipped bits anywhere: an error or other bytes, never a panic.
            for i in (4..p.len().min(600)).step_by(7) {
                let mut bad = p.clone();
                bad[i] ^= 0x5A;
                let _ = unpack(algo, &bad, d.len());
            }
            let _ = unpack(algo, &noise(2 * BS), d.len());
        }
    }

    #[test]
    fn names_and_ids() {
        for algo in Compression::ALL {
            assert_eq!(Compression::from_name(algo.name()), Some(algo));
            assert_eq!(Compression::from_disk(algo.to_disk()), Some(algo));
        }
        assert_eq!(Compression::from_disk(0), None);
        assert_eq!(Compression::from_disk(2), None);
        assert_eq!(Compression::from_name("lzo"), None);
    }
}
