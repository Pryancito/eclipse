//! Parity for RAID5 and RAID6 chunks: P is the XOR of the data blocks of a
//! row, Q their Reed-Solomon syndrome over GF(2^8) with generator 2 and
//! polynomial 0x11d, as Linux md and btrfs compute them:
//!
//! ```text
//! P = D0 ^ D1 ^ ... ^ Dk-1
//! Q = g^0·D0 ^ g^1·D1 ^ ... ^ g^(k-1)·Dk-1
//! ```
//!
//! With P any one lost block of a row comes back; with P and Q any two.
//! Everything is table driven and byte by byte: the kernel target is
//! soft-float, so no SIMD.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Error, Result};

const fn tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    exp[510] = exp[0];
    exp[511] = exp[1];
    (exp, log)
}

const TABLES: ([u8; 512], [u8; 256]) = tables();
const EXP: [u8; 512] = TABLES.0;
const LOG: [u8; 256] = TABLES.1;

/// `a·b` in GF(2^8).
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        EXP[LOG[a as usize] as usize + LOG[b as usize] as usize]
    }
}

/// `1/a` in GF(2^8) (`a` not 0).
pub fn inv(a: u8) -> u8 {
    EXP[255 - LOG[a as usize] as usize]
}

/// `g^n`.
pub fn pow2(n: usize) -> u8 {
    EXP[n % 255]
}

/// `dst ^= c·src`.
fn mul_xor(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 0 {
        return;
    }
    if c == 1 {
        for (d, s) in dst.iter_mut().zip(src) {
            *d ^= s;
        }
        return;
    }
    let lc = LOG[c as usize] as usize;
    for (d, &s) in dst.iter_mut().zip(src) {
        if s != 0 {
            *d ^= EXP[LOG[s as usize] as usize + lc];
        }
    }
}

/// `buf = c·buf`.
fn scale(buf: &mut [u8], c: u8) {
    let lc = LOG[c as usize] as usize;
    for b in buf.iter_mut() {
        if *b != 0 {
            *b = EXP[LOG[*b as usize] as usize + lc];
        }
    }
}

/// P (and Q when `q` is given) of the data blocks.
pub fn gen(data: &[&[u8]], p: &mut [u8], q: Option<&mut [u8]>) {
    p.fill(0);
    for d in data {
        mul_xor(p, d, 1);
    }
    if let Some(q) = q {
        q.fill(0);
        for (i, d) in data.iter().enumerate() {
            mul_xor(q, d, pow2(i));
        }
    }
}

/// Bring back the `missing` blocks of a row: `row` holds the `k` data
/// blocks, then P, then (RAID6) Q; `missing` names at most as many of them
/// as there are parity blocks. The others must be intact.
pub fn recover(row: &mut [Vec<u8>], k: usize, missing: &[usize]) -> Result<()> {
    let parity = row.len() - k;
    let mut miss: Vec<usize> = missing.to_vec();
    miss.sort_unstable();
    miss.dedup();
    if miss.len() > parity || miss.iter().any(|&m| m >= row.len()) {
        return Err(Error::Io);
    }
    let len = row.first().map_or(0, |b| b.len());
    let lost_data: Vec<usize> = miss.iter().copied().filter(|&m| m < k).collect();
    let p_lost = miss.contains(&k);
    let q_lost = parity == 2 && miss.contains(&(k + 1));
    match lost_data.len() {
        0 => {}
        1 if !p_lost => {
            // From P: everything else XORed.
            let x = lost_data[0];
            let mut out = row[k].clone();
            for (i, d) in row[..k].iter().enumerate() {
                if i != x {
                    mul_xor(&mut out, d, 1);
                }
            }
            row[x] = out;
        }
        1 => {
            // P lost too: from Q. g^x·Dx = Q ^ sum of the others.
            if parity < 2 || q_lost {
                return Err(Error::Io);
            }
            let x = lost_data[0];
            let mut out = row[k + 1].clone();
            for (i, d) in row[..k].iter().enumerate() {
                if i != x {
                    mul_xor(&mut out, d, pow2(i));
                }
            }
            scale(&mut out, inv(pow2(x)));
            row[x] = out;
        }
        2 => {
            // Both from P and Q (the classic RAID6 two-data recovery).
            let (x, y) = (lost_data[0], lost_data[1]);
            let mut pxy = row[k].clone();
            let mut qxy = row[k + 1].clone();
            for (i, d) in row[..k].iter().enumerate() {
                if i != x && i != y {
                    mul_xor(&mut pxy, d, 1);
                    mul_xor(&mut qxy, d, pow2(i));
                }
            }
            // Dx·(g^x ^ g^y) = Qxy ^ g^y·Pxy
            let denom = inv(pow2(x) ^ pow2(y));
            let mut dx = qxy;
            mul_xor(&mut dx, &pxy, pow2(y));
            scale(&mut dx, denom);
            let mut dy = pxy;
            mul_xor(&mut dy, &dx, 1);
            row[x] = dx;
            row[y] = dy;
        }
        _ => return Err(Error::Io),
    }
    if p_lost || q_lost {
        let mut p = vec![0u8; len];
        let mut q = vec![0u8; len];
        {
            let data: Vec<&[u8]> = row[..k].iter().map(|b| &b[..]).collect();
            gen(&data, &mut p, (parity == 2).then_some(&mut q[..]));
        }
        if p_lost {
            row[k] = p;
        }
        if q_lost {
            row[k + 1] = q;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rand_row(k: usize, parity: usize, seed: u32) -> Vec<Vec<u8>> {
        let mut s = seed.wrapping_mul(2654435761).max(1);
        let mut row: Vec<Vec<u8>> = (0..k)
            .map(|_| {
                (0..64)
                    .map(|_| {
                        s ^= s << 13;
                        s ^= s >> 17;
                        s ^= s << 5;
                        s as u8
                    })
                    .collect()
            })
            .collect();
        let mut p = vec![0u8; 64];
        let mut q = vec![0u8; 64];
        {
            let d: Vec<&[u8]> = row.iter().map(|b| &b[..]).collect();
            gen(&d, &mut p, (parity == 2).then_some(&mut q[..]));
        }
        row.push(p);
        if parity == 2 {
            row.push(q);
        }
        row
    }

    #[test]
    fn field_arithmetic() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1);
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(a, 0), 0);
        }
        // 0x11d: g^8 = 0x1d.
        assert_eq!(pow2(8), 0x1d);
        assert_eq!(mul(0x80, 2), 0x1d);
        // Distributive.
        for (a, b, c) in [(3u8, 7u8, 200u8), (0x53, 0xca, 0x11), (255, 254, 253)] {
            assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
        }
    }

    #[test]
    fn p_and_q_match_linux_raid6() {
        // Two data bytes 1 and 1: P = 0, Q = g^0 ^ g^1 = 3.
        let (a, b) = ([1u8], [1u8]);
        let (mut p, mut q) = ([0u8], [0u8]);
        gen(&[&a, &b], &mut p, Some(&mut q));
        assert_eq!((p[0], q[0]), (0, 3));
    }

    #[test]
    fn raid5_recovers_any_one() {
        for k in 1..8 {
            let good = rand_row(k, 1, k as u32);
            for m in 0..=k {
                let mut r = good.clone();
                r[m] = vec![0xee; 64];
                recover(&mut r, k, &[m]).unwrap();
                assert_eq!(r, good, "k={} lost {}", k, m);
            }
            let mut r = good.clone();
            assert!(recover(&mut r, k, &[0, k]).is_err());
        }
    }

    #[test]
    fn raid6_recovers_any_two() {
        for k in 1..10 {
            let good = rand_row(k, 2, 7 + k as u32);
            let n = k + 2;
            for a in 0..n {
                for b in a..n {
                    let mut r = good.clone();
                    r[a] = vec![0x55; 64];
                    r[b] = vec![0xaa; 64];
                    recover(&mut r, k, &[a, b]).unwrap();
                    assert_eq!(r, good, "k={} lost {} and {}", k, a, b);
                }
            }
            let mut r = good.clone();
            assert!(recover(&mut r, k, &[0, 1, 2]).is_err());
        }
    }
}
