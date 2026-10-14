//! Encryption primitives, as fscrypt uses them:
//!
//! - file contents: AES-256-XTS, one data unit per 4 KiB block, the tweak
//!   being the block's number in the file;
//! - names (and symlink targets in directories): AES-256-CTS-CBC (CS3
//!   ciphertext stealing, IV zero), padded with NULs to the policy's
//!   multiple;
//! - keys: HKDF-SHA512 from the master key, with info `"fscrypt\0"`, a
//!   context byte and (for per-file keys) the inode's 16-byte nonce.
//!
//! AES comes from the vendored `aes` crate, software-only on kernel targets
//! without SSE.

use alloc::vec;
use alloc::vec::Vec;

use aes::cipher::consts::U16;
use aes::cipher::{BlockDecrypt, BlockEncrypt, BlockSizeUser, KeyInit};
use aes::Aes256;
use sha2::{Digest, Sha256, Sha512};

/// HKDF contexts (Linux `fs/crypto/fscrypt_private.h`).
pub const HKDF_CONTEXT_KEY_IDENTIFIER: u8 = 1;
pub const HKDF_CONTEXT_PER_FILE_ENC_KEY: u8 = 2;

/// Master key sizes accepted (fscrypt's limits).
pub const MIN_MASTER_KEY: usize = 16;
pub const MAX_MASTER_KEY: usize = 64;

/// `FSCRYPT_KEY_IDENTIFIER_SIZE`, `FSCRYPT_FILE_NONCE_SIZE`.
pub const KEY_ID_SIZE: usize = 16;
pub const NONCE_SIZE: usize = 16;

/// `HKDF-Expand(HKDF-Extract(salt = none, master), "fscrypt\0" ||
/// context || info)`, `out.len()` bytes.
pub fn derive(master: &[u8], context: u8, info: &[u8], out: &mut [u8]) {
    let mut full = Vec::with_capacity(9 + info.len());
    full.extend_from_slice(b"fscrypt\0");
    full.push(context);
    full.extend_from_slice(info);
    hkdf::Hkdf::<Sha512>::new(None, master)
        .expand(&full, out)
        .expect("HKDF output length");
}

/// The identifier of a master key (what policies name it by).
pub fn key_identifier(master: &[u8]) -> [u8; KEY_ID_SIZE] {
    let mut id = [0u8; KEY_ID_SIZE];
    derive(master, HKDF_CONTEXT_KEY_IDENTIFIER, &[], &mut id);
    id
}

/// SHA-256 (for the names shown when a directory's key is missing).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// The per-file keys of one inode: XTS for its contents, CTS for the
/// names it holds (a directory) or its target (a symlink).
pub struct FileKeys {
    data: Aes256,
    tweak: Aes256,
    names: Aes256,
}

impl FileKeys {
    pub fn new(master: &[u8], nonce: &[u8; NONCE_SIZE]) -> Self {
        // 64 bytes: the two XTS keys; the names key is the first half,
        // as fscrypt derives one 64-byte per-file key and AES-256-CTS
        // uses its first 32 bytes.
        let mut k = [0u8; 64];
        derive(master, HKDF_CONTEXT_PER_FILE_ENC_KEY, nonce, &mut k);
        let keys = Self {
            data: Aes256::new((&k[..32]).into()),
            tweak: Aes256::new((&k[32..]).into()),
            names: Aes256::new((&k[..32]).into()),
        };
        k.fill(0);
        keys
    }

    fn xts(&self, lblk: u64, buf: &mut [u8], encrypt: bool) {
        let mut t = [0u8; 16];
        t[..8].copy_from_slice(&lblk.to_le_bytes());
        self.tweak.encrypt_block((&mut t).into());
        for b in buf.as_chunks_mut::<16>().0 {
            for (x, y) in b.iter_mut().zip(&t) {
                *x ^= y;
            }
            if encrypt {
                self.data.encrypt_block((&mut b[..]).into());
            } else {
                self.data.decrypt_block((&mut b[..]).into());
            }
            for (x, y) in b.iter_mut().zip(&t) {
                *x ^= y;
            }
            gf_double(&mut t);
        }
    }

    /// Encrypt whole blocks in place, the first being file block `lblk`.
    pub fn encrypt_blocks(&self, lblk: u64, buf: &mut [u8], block: usize) {
        for (i, b) in buf.chunks_exact_mut(block).enumerate() {
            self.xts(lblk + i as u64, b, true);
        }
    }

    /// Decrypt whole blocks in place, the first being file block `lblk`.
    pub fn decrypt_blocks(&self, lblk: u64, buf: &mut [u8], block: usize) {
        for (i, b) in buf.chunks_exact_mut(block).enumerate() {
            self.xts(lblk + i as u64, b, false);
        }
    }

    /// A name, padded with NULs to a multiple of `pad` (at least 16
    /// bytes, at most `max`), then encrypted.
    pub fn encrypt_name(&self, name: &[u8], pad: usize, max: usize) -> Vec<u8> {
        let len = name
            .len()
            .next_multiple_of(pad)
            .clamp(16, max.max(name.len()));
        let mut v = vec![0u8; len];
        v[..name.len()].copy_from_slice(name);
        cts_encrypt(&self.names, &mut v);
        v
    }

    /// The name back, without its padding (`None` if `ct` is too short).
    pub fn decrypt_name(&self, ct: &[u8]) -> Option<Vec<u8>> {
        if ct.len() < 16 {
            return None;
        }
        let mut v = ct.to_vec();
        cts_decrypt(&self.names, &mut v);
        let end = v.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
        v.truncate(end);
        Some(v)
    }
}

/// Multiply the XTS tweak by x in GF(2^128) (little-endian, as IEEE 1619).
fn gf_double(t: &mut [u8; 16]) {
    let mut carry = 0u8;
    for b in t.iter_mut() {
        let next = *b >> 7;
        *b = (*b << 1) | carry;
        carry = next;
    }
    if carry != 0 {
        t[0] ^= 0x87;
    }
}

fn xor16(a: &mut [u8], b: &[u8]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
}

/// CBC with ciphertext stealing, CS3 (Linux `cts(cbc(aes))`), IV zero.
/// `buf` must hold at least one block.
pub fn cts_encrypt<C: BlockEncrypt + BlockSizeUser<BlockSize = U16>>(c: &C, buf: &mut [u8]) {
    let n = buf.len();
    debug_assert!(n >= 16);
    let mut iv = [0u8; 16];
    if n == 16 {
        c.encrypt_block(buf.into());
        return;
    }
    // Blocks before the last two (the last may be partial).
    let tail = (n - 1) % 16 + 1;
    let head = n - 16 - tail;
    for b in buf[..head].as_chunks_mut::<16>().0 {
        xor16(b, &iv);
        c.encrypt_block((&mut b[..]).into());
        iv = *b;
    }
    let mut pen: [u8; 16] = buf[head..head + 16].try_into().unwrap();
    xor16(&mut pen, &iv);
    c.encrypt_block((&mut pen).into());
    let mut last = [0u8; 16];
    last[..tail].copy_from_slice(&buf[head + 16..]);
    xor16(&mut last, &pen);
    c.encrypt_block((&mut last).into());
    buf[head..head + 16].copy_from_slice(&last);
    buf[head + 16..].copy_from_slice(&pen[..tail]);
}

/// The inverse of [`cts_encrypt`].
pub fn cts_decrypt<C: BlockDecrypt + BlockSizeUser<BlockSize = U16>>(c: &C, buf: &mut [u8]) {
    let n = buf.len();
    debug_assert!(n >= 16);
    if n == 16 {
        c.decrypt_block(buf.into());
        return;
    }
    let tail = (n - 1) % 16 + 1;
    let head = n - 16 - tail;
    let mut iv = [0u8; 16];
    for b in buf[..head].as_chunks_mut::<16>().0 {
        let ct = *b;
        c.decrypt_block((&mut b[..]).into());
        xor16(b, &iv);
        iv = ct;
    }
    // The stolen layout: C_last (full) then the head of C_pen.
    let c_last: [u8; 16] = buf[head..head + 16].try_into().unwrap();
    let mut d = c_last;
    c.decrypt_block((&mut d).into());
    // d = P_last||0 ^ C_pen, so the tail of C_pen is the tail of d.
    let mut c_pen = [0u8; 16];
    c_pen[..tail].copy_from_slice(&buf[head + 16..]);
    c_pen[tail..].copy_from_slice(&d[tail..]);
    let mut p_last = d;
    xor16(&mut p_last, &c_pen);
    let mut p_pen = c_pen;
    c.decrypt_block((&mut p_pen).into());
    xor16(&mut p_pen, &iv);
    buf[head..head + 16].copy_from_slice(&p_pen);
    buf[head + 16..].copy_from_slice(&p_last[..tail]);
}

/// URL-safe base64 without padding (names shown without the key).
pub fn base64url(data: &[u8]) -> Vec<u8> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let v = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(A[(v >> (18 - 6 * i) & 63) as usize]);
        }
    }
    out
}

/// The inverse of [`base64url`] (`None` if `s` is not one).
pub fn unbase64url(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        let mut v = 0u32;
        for (i, &ch) in c.iter().enumerate() {
            v |= val(ch)? << (18 - 6 * i);
        }
        for i in 0..c.len() - 1 {
            out.push((v >> (16 - 8 * i)) as u8);
        }
    }
    // Canonical only (no stray low bits in the last character).
    if base64url(&out) != s {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_rejects_others() {
        for n in 0..40 {
            let d: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37) ^ 0xA5).collect();
            let e = base64url(&d);
            assert!(e
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_'));
            assert_eq!(unbase64url(&e).unwrap(), d);
        }
        assert_eq!(base64url(b"foobar"), b"Zm9vYmFy");
        assert_eq!(unbase64url(b"Zm9vYmE").unwrap(), b"fooba");
        assert!(unbase64url(b"Zm9vYmF").is_none());
        assert!(unbase64url(b"Z").is_none());
        assert!(unbase64url(b"Zm9v/mFy").is_none());
        // Non-canonical trailing bits.
        assert!(unbase64url(b"Zh").is_none());
    }

    #[test]
    fn xts_matches_an_independent_implementation() {
        let key: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(29) ^ 0x5C).collect();
        let k = FileKeys {
            data: Aes256::new((&key[..32]).into()),
            tweak: Aes256::new((&key[32..]).into()),
            names: Aes256::new((&key[..32]).into()),
        };
        let plain: Vec<u8> = (0..3 * 4096u32)
            .map(|i| (i * 131 + i / 251) as u8)
            .collect();
        let mut mine = plain.clone();
        k.encrypt_blocks(77, &mut mine, 4096);
        let xts = xts_mode::Xts128::new(
            Aes256::new((&key[..32]).into()),
            Aes256::new((&key[32..]).into()),
        );
        let mut theirs = plain.clone();
        xts.encrypt_area(&mut theirs, 4096, 77, xts_mode::get_tweak_default);
        assert_eq!(mine, theirs);
        k.decrypt_blocks(77, &mut mine, 4096);
        assert_eq!(mine, plain);
    }

    #[test]
    fn cts_matches_an_independent_implementation() {
        use cts::{Decrypt, Encrypt, KeyIvInit};
        let key = [0x42u8; 32];
        let c = Aes256::new(&key.into());
        // From 17 bytes: for exactly one block that crate encrypts twice,
        // where Linux's cts(cbc(aes)) (and this) is plain CBC.
        for n in 17..120 {
            let p: Vec<u8> = (0..n as u8).map(|i| i ^ 0x3C).collect();
            let mut mine = p.clone();
            cts_encrypt(&c, &mut mine);
            let mut theirs = p.clone();
            cts::CbcCs3Enc::<Aes256>::new(&key.into(), &[0u8; 16].into())
                .encrypt(&mut theirs)
                .unwrap();
            assert_eq!(mine, theirs, "len {n}");
            cts::CbcCs3Dec::<Aes256>::new(&key.into(), &[0u8; 16].into())
                .decrypt(&mut theirs)
                .unwrap();
            assert_eq!(theirs, p);
        }
    }

    #[test]
    fn hkdf_matches_rfc_5869_style_derivation() {
        // HKDF-SHA512 by hand: PRK = HMAC(zeros, IKM), T1 = HMAC(PRK, info || 1).
        use hmac::{Hmac, Mac};
        let ikm = [0x0Bu8; 32];
        let mut prk = <Hmac<Sha512> as Mac>::new_from_slice(&[0u8; 64]).unwrap();
        prk.update(&ikm);
        let prk = prk.finalize().into_bytes();
        let mut t1 = <Hmac<Sha512> as Mac>::new_from_slice(&prk).unwrap();
        t1.update(b"fscrypt\0\x01");
        t1.update(&[1]);
        let t1 = t1.finalize().into_bytes();
        assert_eq!(key_identifier(&ikm)[..], t1[..16]);
    }

    #[test]
    fn cts_round_trips_at_every_length() {
        let c = Aes256::new(&[7u8; 32].into());
        for n in 16..100 {
            let p: Vec<u8> = (0..n as u8).collect();
            let mut v = p.clone();
            cts_encrypt(&c, &mut v);
            assert_ne!(v, p);
            cts_decrypt(&c, &mut v);
            assert_eq!(v, p, "len {n}");
        }
    }

    #[test]
    fn names_are_padded_and_round_trip() {
        let k = FileKeys::new(&[3u8; 64], &[9u8; 16]);
        let a = k.encrypt_name(b"a", 32, 255);
        assert_eq!(a.len(), 32);
        assert_eq!(k.decrypt_name(&a).unwrap(), b"a");
        let long = vec![b'x'; 250];
        let e = k.encrypt_name(&long, 32, 255);
        assert_eq!(e.len(), 255);
        assert_eq!(k.decrypt_name(&e).unwrap(), long);
        assert_eq!(k.encrypt_name(b"hello", 4, 255).len(), 16);
        assert_ne!(k.encrypt_name(b"a", 32, 255), k.encrypt_name(b"b", 32, 255));
    }

    #[test]
    fn keys_depend_on_the_master_key_and_the_nonce() {
        let id1 = key_identifier(&[1u8; 64]);
        assert_ne!(id1, key_identifier(&[2u8; 64]));
        assert_eq!(id1, key_identifier(&[1u8; 64]));
        let mut b1 = vec![0u8; 4096];
        let mut b2 = b1.clone();
        FileKeys::new(&[1u8; 64], &[0u8; 16]).encrypt_blocks(0, &mut b1, 4096);
        FileKeys::new(&[1u8; 64], &[1u8; 16]).encrypt_blocks(0, &mut b2, 4096);
        assert_ne!(b1, b2);
    }
}
