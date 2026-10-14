//! Volumes (format v0.4 with `FEAT_VOLUME`): one logical block space over
//! one or more devices, as btrfs chunks.
//!
//! The logical space is cut into the same 128 MiB groups the allocator
//! uses. Each group in use is a *chunk*: what it holds (data, metadata or
//! both), its profile and where its copies live:
//!
//! | profile  | copies | stripes                 | devices |
//! |----------|--------|-------------------------|---------|
//! | SINGLE   | 1      | 1                       | 1       |
//! | DUP      | 2      | 2 on one device         | 1       |
//! | RAID0    | 1      | every device, 64 KiB    | 2       |
//! | RAID1    | 2      | 2 devices               | 2       |
//! | RAID10   | 2      | pairs, striped by 64 KiB| 4       |
//! | RAID1C3  | 3      | 3 devices               | 3       |
//! | RAID1C4  | 4      | 4 devices               | 4       |
//!
//! Block 0 of every device holds a copy of the superblock (with that
//! device's id); the superblock also maps group 0, which holds the journal
//! and the chunk map, so a mount finds everything from any device.
//!
//! The filesystem above sees logical blocks only; [`Volume`] turns each
//! read into a read of the first copy that answers and each write into a
//! write of every copy. Checksums live above, so the filesystem reads the
//! other copies itself ([`Volume::read_copy`]) when one fails to verify,
//! and writes the good one back over the bad.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::crc::crc32c;
use crate::device::BlockDevice;
use crate::format::{le32, le64, put32, put64, BLOCK_SIZE, GROUP_BLOCKS};
use crate::journal::Spin;
use crate::{Error, Result};

const BS: u64 = BLOCK_SIZE;

/// Blocks of a RAID0/RAID10 stripe unit (64 KiB).
pub const STRIPE_UNIT: u64 = 16;
/// Blocks at the start of every device kept for its superblock.
pub const DEV_RESERVED: u64 = 16;
/// Smallest chunk worth making (1 MiB).
pub const MIN_CHUNK: u64 = 256;
/// Stripes a superblock can describe for group 0.
pub const SYS_STRIPES: usize = 4;

const MAP_MAGIC: &[u8; 8] = b"EFSCHMAP";

/// How a chunk keeps its blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Profile {
    Single,
    Dup,
    Raid0,
    Raid1,
    Raid10,
    Raid1c3,
    Raid1c4,
}

impl Profile {
    pub const ALL: [Profile; 7] = [
        Profile::Single,
        Profile::Dup,
        Profile::Raid0,
        Profile::Raid1,
        Profile::Raid10,
        Profile::Raid1c3,
        Profile::Raid1c4,
    ];

    pub fn id(self) -> u8 {
        match self {
            Profile::Single => 0,
            Profile::Dup => 1,
            Profile::Raid0 => 2,
            Profile::Raid1 => 3,
            Profile::Raid10 => 4,
            Profile::Raid1c3 => 5,
            Profile::Raid1c4 => 6,
        }
    }

    pub fn from_id(id: u8) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.id() == id)
            .ok_or(Error::Corrupt("chunk profile"))
    }

    /// Copies of every block.
    pub fn copies(self) -> usize {
        match self {
            Profile::Single | Profile::Raid0 => 1,
            Profile::Dup | Profile::Raid1 | Profile::Raid10 => 2,
            Profile::Raid1c3 => 3,
            Profile::Raid1c4 => 4,
        }
    }

    /// Fewest devices it can be laid out on.
    pub fn min_devices(self) -> usize {
        match self {
            Profile::Single | Profile::Dup => 1,
            Profile::Raid0 | Profile::Raid1 => 2,
            Profile::Raid1c3 => 3,
            Profile::Raid10 | Profile::Raid1c4 => 4,
        }
    }

    /// Copies that may be lost without losing data.
    pub fn tolerates(self) -> usize {
        match self {
            Profile::Single | Profile::Raid0 => 0,
            p => p.copies() - 1,
        }
    }

    pub fn striped(self) -> bool {
        matches!(self, Profile::Raid0 | Profile::Raid10)
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Single => "single",
            Profile::Dup => "dup",
            Profile::Raid0 => "raid0",
            Profile::Raid1 => "raid1",
            Profile::Raid10 => "raid10",
            Profile::Raid1c3 => "raid1c3",
            Profile::Raid1c4 => "raid1c4",
        }
    }
}

/// What a chunk's blocks may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChunkKind {
    /// Anything (groups of a filesystem made on one device before it
    /// became a volume).
    Mixed,
    Data,
    Metadata,
}

impl ChunkKind {
    pub fn id(self) -> u8 {
        match self {
            ChunkKind::Mixed => 0,
            ChunkKind::Data => 1,
            ChunkKind::Metadata => 2,
        }
    }

    pub fn from_id(id: u8) -> Result<Self> {
        match id {
            0 => Ok(ChunkKind::Mixed),
            1 => Ok(ChunkKind::Data),
            2 => Ok(ChunkKind::Metadata),
            _ => Err(Error::Corrupt("chunk kind")),
        }
    }

    /// Whether blocks wanted for `want` may come from a chunk of this kind.
    pub fn serves(self, want: ChunkKind) -> bool {
        self == ChunkKind::Mixed || self == want
    }
}

/// Where one stripe of a chunk starts: device and physical block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stripe {
    pub devid: u32,
    pub phys: u64,
}

/// One group of the logical space and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub kind: ChunkKind,
    pub profile: Profile,
    /// Logical blocks (at most a group).
    pub blocks: u64,
    pub stripes: Vec<Stripe>,
}

/// Physical blocks each of `n` stripes takes for `blocks` logical blocks.
pub fn stripe_len(profile: Profile, blocks: u64, n: usize) -> u64 {
    let units = blocks.div_ceil(STRIPE_UNIT);
    match profile {
        Profile::Raid0 => units.div_ceil(n.max(1) as u64) * STRIPE_UNIT,
        Profile::Raid10 => units.div_ceil((n / 2).max(1) as u64) * STRIPE_UNIT,
        _ => blocks,
    }
}

impl Chunk {
    pub fn stripe_len(&self) -> u64 {
        stripe_len(self.profile, self.blocks, self.stripes.len())
    }

    /// Every copy of block `off` of the chunk, and how many blocks from
    /// `off` on stay contiguous in all of them.
    pub fn locate(&self, off: u64) -> (Vec<Stripe>, u64) {
        let s = &self.stripes;
        let at = |st: Stripe, p: u64| Stripe {
            devid: st.devid,
            phys: st.phys + p,
        };
        let unit = off / STRIPE_UNIT;
        let within = off % STRIPE_UNIT;
        let rest = self.blocks - off;
        match self.profile {
            Profile::Raid0 => {
                let k = s.len() as u64;
                let i = (unit % k) as usize;
                let p = unit / k * STRIPE_UNIT + within;
                (vec![at(s[i], p)], (STRIPE_UNIT - within).min(rest))
            }
            Profile::Raid10 => {
                let w = (s.len() / 2) as u64;
                let i = (unit % w) as usize;
                let p = unit / w * STRIPE_UNIT + within;
                (
                    vec![at(s[2 * i], p), at(s[2 * i + 1], p)],
                    (STRIPE_UNIT - within).min(rest),
                )
            }
            _ => (s.iter().map(|&st| at(st, off)).collect(), rest),
        }
    }

    /// Whether the stripes fit the profile.
    fn valid(&self) -> bool {
        let n = self.stripes.len();
        let devs: Vec<u32> = {
            let mut d: Vec<u32> = self.stripes.iter().map(|s| s.devid).collect();
            d.sort_unstable();
            d.dedup();
            d
        };
        let distinct = devs.len() == n;
        self.blocks > 0
            && self.blocks <= GROUP_BLOCKS
            && match self.profile {
                Profile::Single => n == 1,
                Profile::Dup => n == 2 && devs.len() == 1,
                Profile::Raid1 => n == 2 && distinct,
                Profile::Raid1c3 => n == 3 && distinct,
                Profile::Raid1c4 => n == 4 && distinct,
                Profile::Raid0 => n >= 2 && distinct,
                Profile::Raid10 => n >= 4 && n.is_multiple_of(2) && distinct,
            }
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.kind.id());
        out.push(self.profile.id());
        out.extend_from_slice(&(self.stripes.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.blocks.to_le_bytes());
        for s in &self.stripes {
            out.extend_from_slice(&s.devid.to_le_bytes());
            out.extend_from_slice(&s.phys.to_le_bytes());
        }
    }

    fn decode_from(b: &[u8], at: &mut usize) -> Result<Self> {
        let need = |n: usize, at: usize| -> Result<()> {
            if at + n > b.len() {
                Err(Error::Corrupt("chunk map length"))
            } else {
                Ok(())
            }
        };
        need(16, *at)?;
        let kind = ChunkKind::from_id(b[*at])?;
        let profile = Profile::from_id(b[*at + 1])?;
        let n = u16::from_le_bytes([b[*at + 2], b[*at + 3]]) as usize;
        let blocks = le64(b, *at + 8);
        *at += 16;
        need(n * 12, *at)?;
        let mut stripes = Vec::with_capacity(n);
        for _ in 0..n {
            stripes.push(Stripe {
                devid: le32(b, *at),
                phys: le64(b, *at + 4),
            });
            *at += 12;
        }
        let c = Chunk {
            kind,
            profile,
            blocks,
            stripes,
        };
        if !c.valid() {
            return Err(Error::Corrupt("chunk stripes"));
        }
        Ok(c)
    }

    /// Group 0's chunk as the superblock keeps it (`SYS_STRIPES` at most).
    pub fn encode_sys(&self, b: &mut [u8]) {
        b[0] = self.kind.id();
        b[1] = self.profile.id();
        b[2] = self.stripes.len() as u8;
        put64(b, 8, self.blocks);
        for (i, s) in self.stripes.iter().enumerate() {
            put32(b, 16 + i * 12, s.devid);
            put64(b, 20 + i * 12, s.phys);
        }
    }

    pub fn decode_sys(b: &[u8]) -> Result<Self> {
        let n = b[2] as usize;
        if n > SYS_STRIPES {
            return Err(Error::Corrupt("system chunk"));
        }
        let c = Chunk {
            kind: ChunkKind::from_id(b[0])?,
            profile: Profile::from_id(b[1])?,
            blocks: le64(b, 8),
            stripes: (0..n)
                .map(|i| Stripe {
                    devid: le32(b, 16 + i * 12),
                    phys: le64(b, 20 + i * 12),
                })
                .collect(),
        };
        if !c.valid() || c.profile.striped() {
            return Err(Error::Corrupt("system chunk"));
        }
        Ok(c)
    }
}

/// Every device and chunk of a volume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkMap {
    /// Device id -> size in blocks.
    pub devices: BTreeMap<u32, u64>,
    /// Group -> chunk.
    pub chunks: BTreeMap<u64, Chunk>,
}

/// A physical range: device, first block, blocks.
pub type PhysRange = (u32, u64, u64);

/// A device's free blocks, id and free extents.
type DevRoom = (u64, u32, Vec<(u64, u64)>);

impl ChunkMap {
    /// `EFSCHMAP`, version, devices, chunks, CRC32C of everything before it.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAP_MAGIC);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(self.devices.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        for (&id, &size) in &self.devices {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
        }
        for (&g, c) in &self.chunks {
            out.extend_from_slice(&g.to_le_bytes());
            c.encode_into(&mut out);
        }
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < 28 || &b[0..8] != MAP_MAGIC || le32(b, 8) != 1 {
            return Err(Error::Corrupt("chunk map header"));
        }
        let ndev = le32(b, 12) as usize;
        let nchunk = le32(b, 16) as usize;
        let mut at = 24;
        let mut map = ChunkMap::default();
        for _ in 0..ndev {
            if at + 12 > b.len() {
                return Err(Error::Corrupt("chunk map length"));
            }
            map.devices.insert(le32(b, at), le64(b, at + 4));
            at += 12;
        }
        for _ in 0..nchunk {
            if at + 8 > b.len() {
                return Err(Error::Corrupt("chunk map length"));
            }
            let g = le64(b, at);
            at += 8;
            let c = Chunk::decode_from(b, &mut at)?;
            map.chunks.insert(g, c);
        }
        if at + 4 > b.len() || crc32c(&b[..at]) != le32(b, at) {
            return Err(Error::Corrupt("chunk map checksum"));
        }
        map.check()?;
        Ok(map)
    }

    /// Stripes inside their devices, never overlapping, devices known.
    pub fn check(&self) -> Result<()> {
        let mut per_dev: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
        for c in self.chunks.values() {
            if !c.valid() {
                return Err(Error::Corrupt("chunk stripes"));
            }
            let len = c.stripe_len();
            for s in &c.stripes {
                let size = *self
                    .devices
                    .get(&s.devid)
                    .ok_or(Error::Corrupt("chunk on an unknown device"))?;
                if s.phys.checked_add(len).is_none_or(|e| e > size) {
                    return Err(Error::Corrupt("chunk past the end of its device"));
                }
                per_dev.entry(s.devid).or_default().push((s.phys, len));
            }
        }
        for v in per_dev.values_mut() {
            v.sort_unstable();
            if v.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
                return Err(Error::Corrupt("overlapping chunks"));
            }
        }
        Ok(())
    }

    /// The end of the logical space (in blocks).
    pub fn end(&self) -> u64 {
        self.chunks
            .iter()
            .next_back()
            .map(|(g, c)| g * GROUP_BLOCKS + c.blocks)
            .unwrap_or(0)
    }

    /// The first group with no chunk.
    pub fn free_group(&self) -> u64 {
        (0..)
            .find(|g| !self.chunks.contains_key(g))
            .unwrap_or(self.chunks.len() as u64)
    }

    /// Free physical ranges of `devid` (`held` counts as used).
    pub fn free_extents(&self, devid: u32, held: &[PhysRange]) -> Vec<(u64, u64)> {
        let Some(&size) = self.devices.get(&devid) else {
            return Vec::new();
        };
        let mut used: Vec<(u64, u64)> = vec![(0, DEV_RESERVED)];
        for c in self.chunks.values() {
            let len = c.stripe_len();
            for s in c.stripes.iter().filter(|s| s.devid == devid) {
                used.push((s.phys, len));
            }
        }
        used.extend(
            held.iter()
                .filter(|h| h.0 == devid)
                .map(|&(_, s, l)| (s, l)),
        );
        used.sort_unstable();
        let mut out = Vec::new();
        let mut at = 0;
        for (s, l) in used {
            if s > at {
                out.push((at, s - at));
            }
            at = at.max(s + l);
        }
        if size > at {
            out.push((at, size - at));
        }
        // A device made smaller may still have chunks past its new end.
        out.retain_mut(|e| {
            e.1 = e.1.min(size.saturating_sub(e.0));
            e.1 > 0
        });
        out
    }

    /// Physical blocks not in any chunk, per device.
    pub fn unallocated(&self, held: &[PhysRange]) -> BTreeMap<u32, u64> {
        self.devices
            .keys()
            .map(|&d| (d, self.free_extents(d, held).iter().map(|e| e.1).sum()))
            .collect()
    }

    /// Stripes for a new chunk of `profile` holding up to `blocks`
    /// (smaller if that is what fits, down to [`MIN_CHUNK`]), on devices
    /// outside `avoid`. Devices with more room left come first.
    pub fn place(
        &self,
        profile: Profile,
        blocks: u64,
        avoid: &[u32],
        held: &[PhysRange],
    ) -> Option<(u64, Vec<Stripe>)> {
        let mut n = blocks;
        loop {
            if let Some(s) = self.place_exact(profile, n, avoid, held) {
                return Some((n, s));
            }
            if n <= MIN_CHUNK.min(blocks) {
                return None;
            }
            n = (n / 2).max(MIN_CHUNK.min(blocks));
        }
    }

    /// Stripes to move chunk `old` to `profile`, keeping those of its
    /// stripes that can stay where they are (not on an `avoid` device, and
    /// neither layout striped), so only the copies that change place need
    /// writing. The kept stripes come first, in their old order.
    pub fn place_moving(
        &self,
        old: &Chunk,
        profile: Profile,
        avoid: &[u32],
        held: &[PhysRange],
    ) -> Option<Vec<Stripe>> {
        let blocks = old.blocks;
        if old.profile.striped() || profile.striped() {
            return self.place_exact(profile, blocks, avoid, held);
        }
        let want = profile.copies();
        let mut keep: Vec<Stripe> = Vec::new();
        for s in &old.stripes {
            if avoid.contains(&s.devid) || keep.len() == want {
                continue;
            }
            let ok = match profile {
                Profile::Dup => keep.iter().all(|k| k.devid == s.devid),
                Profile::Single => true,
                _ => keep.iter().all(|k| k.devid != s.devid),
            };
            if ok {
                keep.push(*s);
            }
        }
        // The rest: whole new stripes, on the right devices.
        let mut out = keep.clone();
        let mut extra: Vec<PhysRange> = held.to_vec();
        while out.len() < want {
            let devs: Vec<u32> = match profile {
                Profile::Dup if !out.is_empty() => vec![out[0].devid],
                Profile::Dup | Profile::Single => self.devices.keys().copied().collect(),
                _ => self
                    .devices
                    .keys()
                    .copied()
                    .filter(|d| out.iter().all(|s| s.devid != *d))
                    .collect(),
            };
            let mut best: Option<(u64, Stripe)> = None;
            for d in devs.into_iter().filter(|d| !avoid.contains(d)) {
                let ext = self.free_extents(d, &extra);
                let room: u64 = ext.iter().map(|e| e.1).sum();
                if let Some(e) = ext.iter().find(|e| e.1 >= blocks) {
                    if best.is_none_or(|b| room > b.0) {
                        best = Some((
                            room,
                            Stripe {
                                devid: d,
                                phys: e.0,
                            },
                        ));
                    }
                }
            }
            let (_, s) = best?;
            extra.push((s.devid, s.phys, blocks));
            out.push(s);
        }
        Some(out)
    }

    /// Stripes for a chunk of exactly `blocks` (see [`place`](Self::place)).
    pub fn place_exact(
        &self,
        profile: Profile,
        blocks: u64,
        avoid: &[u32],
        held: &[PhysRange],
    ) -> Option<Vec<Stripe>> {
        // (room left, devid, free extents)
        let mut devs: Vec<DevRoom> = self
            .devices
            .keys()
            .filter(|d| !avoid.contains(d))
            .map(|&d| {
                let ext = self.free_extents(d, held);
                (ext.iter().map(|e| e.1).sum(), d, ext)
            })
            .collect();
        devs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        // First fit of `len` blocks on a device.
        let fit = |ext: &[(u64, u64)], len: u64| ext.iter().find(|e| e.1 >= len).map(|e| e.0);
        match profile {
            Profile::Single => devs.iter().find_map(|(_, d, ext)| {
                fit(ext, blocks).map(|p| vec![Stripe { devid: *d, phys: p }])
            }),
            Profile::Dup => devs.iter().find_map(|(_, d, ext)| {
                let mut slots = Vec::new();
                for &(s, l) in ext {
                    for i in 0..l / blocks {
                        slots.push(s + i * blocks);
                    }
                }
                (slots.len() >= 2).then(|| {
                    slots[..2]
                        .iter()
                        .map(|&p| Stripe { devid: *d, phys: p })
                        .collect()
                })
            }),
            Profile::Raid1 | Profile::Raid1c3 | Profile::Raid1c4 => {
                let want = profile.copies();
                let got: Vec<Stripe> = devs
                    .iter()
                    .filter_map(|(_, d, ext)| {
                        fit(ext, blocks).map(|p| Stripe { devid: *d, phys: p })
                    })
                    .take(want)
                    .collect();
                (got.len() == want).then_some(got)
            }
            Profile::Raid0 | Profile::Raid10 => {
                let min = profile.min_devices();
                let mut k = devs.len();
                if profile == Profile::Raid10 {
                    k &= !1;
                }
                while k >= min {
                    let len = stripe_len(profile, blocks, k);
                    let got: Vec<Stripe> = devs
                        .iter()
                        .filter_map(|(_, d, ext)| {
                            fit(ext, len).map(|p| Stripe { devid: *d, phys: p })
                        })
                        .take(k)
                        .collect();
                    if got.len() == k {
                        return Some(got);
                    }
                    k -= if profile == Profile::Raid10 { 2 } else { 1 };
                }
                None
            }
        }
    }
}

/// One piece of an I/O: where in the caller's buffer, and every copy.
struct Piece {
    at: usize,
    len: usize,
    copies: Vec<(u32, u64)>,
    /// Logical block 0: the superblock, kept at block 0 of every device.
    superblock: bool,
}

/// The logical device of a volume.
pub struct Volume {
    devs: Spin<BTreeMap<u32, Arc<dyn BlockDevice>>>,
    map: Spin<ChunkMap>,
    /// Physical ranges the open transaction stopped using: the committed
    /// state still lives there, so nothing new goes there until it commits.
    held: Spin<Vec<PhysRange>>,
    /// Devices that missed commits (they came back after a degraded
    /// mount): read only when no other copy answers, until resynced.
    stale: Spin<Vec<u32>>,
    /// Writes the superblock copy of one device: patches the device id in.
    sb_for: fn(&[u8], u32) -> Vec<u8>,
}

impl Volume {
    pub fn new(
        devs: BTreeMap<u32, Arc<dyn BlockDevice>>,
        map: ChunkMap,
        sb_for: fn(&[u8], u32) -> Vec<u8>,
    ) -> Self {
        Self {
            devs: Spin::new(devs),
            map: Spin::new(map),
            held: Spin::new(Vec::new()),
            stale: Spin::new(Vec::new()),
            sb_for,
        }
    }

    pub fn set_stale(&self, devs: Vec<u32>) {
        self.stale.with(|s| *s = devs);
    }

    pub fn stale(&self) -> Vec<u32> {
        self.stale.with(|s| s.clone())
    }

    /// Copies in the order to read them: up-to-date devices first.
    fn read_order(&self, copies: &[(u32, u64)]) -> Vec<(u32, u64)> {
        let stale = self.stale();
        let mut v: Vec<(u32, u64)> = copies.to_vec();
        v.sort_by_key(|c| stale.contains(&c.0));
        v
    }

    pub fn map(&self) -> ChunkMap {
        self.map.with(|m| m.clone())
    }

    pub fn with_map<R>(&self, f: impl FnOnce(&mut ChunkMap) -> R) -> R {
        self.map.with(f)
    }

    pub fn held(&self) -> Vec<PhysRange> {
        self.held.with(|h| h.clone())
    }

    /// Keep `r` out of new chunks until [`release`](Self::release).
    pub fn hold(&self, r: PhysRange) {
        self.held.with(|h| h.push(r));
    }

    /// The transaction committed: held ranges are free.
    pub fn release(&self) {
        self.held.with(|h| h.clear());
    }

    pub fn device(&self, devid: u32) -> Option<Arc<dyn BlockDevice>> {
        self.devs.with(|d| d.get(&devid).cloned())
    }

    pub fn present(&self) -> Vec<u32> {
        self.devs.with(|d| d.keys().copied().collect())
    }

    pub fn attach(&self, devid: u32, dev: Arc<dyn BlockDevice>) {
        self.devs.with(|d| d.insert(devid, dev));
    }

    pub fn detach(&self, devid: u32) -> Option<Arc<dyn BlockDevice>> {
        self.devs.with(|d| d.remove(&devid))
    }

    /// Copies of logical block `b` (0 if it is in no chunk).
    pub fn copies(&self, b: u64) -> usize {
        self.map.with(|m| {
            m.chunks
                .get(&(b / GROUP_BLOCKS))
                .filter(|c| b % GROUP_BLOCKS < c.blocks)
                .map(|c| c.profile.copies())
                .unwrap_or(0)
        })
    }

    fn pieces(&self, offset: u64, len: usize) -> Result<Vec<Piece>> {
        let mut out = Vec::new();
        let mut at = 0usize;
        self.map.with(|m| {
            while at < len {
                let pos = offset + at as u64;
                let b = pos / BS;
                let inner = pos % BS;
                if b == 0 {
                    let n = ((BS - inner) as usize).min(len - at);
                    out.push(Piece {
                        at,
                        len: n,
                        copies: Vec::new(),
                        superblock: true,
                    });
                    at += n;
                    continue;
                }
                let c = m
                    .chunks
                    .get(&(b / GROUP_BLOCKS))
                    .filter(|c| b % GROUP_BLOCKS < c.blocks)
                    .ok_or(Error::Corrupt("block in no chunk"))?;
                let (copies, run) = c.locate(b % GROUP_BLOCKS);
                let n = ((run * BS - inner) as usize).min(len - at);
                out.push(Piece {
                    at,
                    len: n,
                    copies: copies
                        .into_iter()
                        .map(|s| (s.devid, s.phys * BS + inner))
                        .collect(),
                    superblock: false,
                });
                at += n;
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn read_sb(&self, inner: u64, buf: &mut [u8]) -> Result<()> {
        let stale = self.stale();
        let mut devs: Vec<(u32, Arc<dyn BlockDevice>)> = self
            .devs
            .with(|d| d.iter().map(|(&i, d)| (i, d.clone())).collect());
        devs.sort_by_key(|d| stale.contains(&d.0));
        for (_, d) in devs {
            if d.read_at(inner, buf).is_ok() {
                return Ok(());
            }
        }
        Err(Error::Io)
    }

    /// Read `buf` at `offset` from copy `k` of every block (no fallback).
    pub fn read_copy(&self, offset: u64, buf: &mut [u8], k: usize) -> Result<()> {
        for p in self.pieces(offset, buf.len())? {
            let dst = &mut buf[p.at..p.at + p.len];
            if p.superblock {
                self.read_sb(offset + p.at as u64, dst)?;
                continue;
            }
            let (devid, phys) = *p.copies.get(k).ok_or(Error::Invalid)?;
            self.device(devid).ok_or(Error::Io)?.read_at(phys, dst)?;
        }
        Ok(())
    }

    /// Write `buf` at `offset` to copy `k` only (repairs).
    pub fn write_copy(&self, offset: u64, buf: &[u8], k: usize) -> Result<()> {
        for p in self.pieces(offset, buf.len())? {
            if p.superblock {
                continue;
            }
            let (devid, phys) = *p.copies.get(k).ok_or(Error::Invalid)?;
            self.device(devid)
                .ok_or(Error::Io)?
                .write_at(phys, &buf[p.at..p.at + p.len])?;
        }
        Ok(())
    }

    /// Copy blocks `start..start+n` of a group from its layout `from` to
    /// its layout `to`, as they read from the first copy that answers.
    pub fn copy_between(&self, from: &Chunk, to: &Chunk, start: u64, n: u64) -> Result<()> {
        let mut off = start;
        let mut buf = Vec::new();
        while off < start + n {
            let (src, a) = from.locate(off);
            let (dst, b) = to.locate(off);
            let run = a.min(b).min(start + n - off).min(256);
            buf.resize((run * BS) as usize, 0);
            let mut done = false;
            let stale = self.stale();
            let mut src = src;
            src.sort_by_key(|s| stale.contains(&s.devid));
            for s in &src {
                if let Some(d) = self.device(s.devid) {
                    if d.read_at(s.phys * BS, &mut buf).is_ok() {
                        done = true;
                        break;
                    }
                }
            }
            if !done {
                return Err(Error::Io);
            }
            for (i, s) in dst.iter().enumerate() {
                // A stripe both layouts share already holds the blocks.
                if !from.profile.striped()
                    && !to.profile.striped()
                    && from.stripes.contains(&to.stripes[i])
                {
                    continue;
                }
                self.device(s.devid)
                    .ok_or(Error::Io)?
                    .write_at(s.phys * BS, &buf)?;
            }
            off += run;
        }
        Ok(())
    }
}

impl Volume {
    /// Bring the copies of `c` on stale devices up to date for blocks
    /// `start..start+n`, from a copy on a device that is not stale.
    /// Copies with no up-to-date twin are left as they are.
    pub fn resync(&self, c: &Chunk, start: u64, n: u64) -> Result<()> {
        let stale = self.stale();
        let mut off = start;
        let mut buf = Vec::new();
        while off < start + n {
            let (copies, run) = c.locate(off);
            let run = run.min(start + n - off).min(256);
            let (old, fresh): (Vec<Stripe>, Vec<Stripe>) =
                copies.into_iter().partition(|s| stale.contains(&s.devid));
            if !old.is_empty() {
                buf.resize((run * BS) as usize, 0);
                let mut got = false;
                for s in &fresh {
                    if let Some(d) = self.device(s.devid) {
                        if d.read_at(s.phys * BS, &mut buf).is_ok() {
                            got = true;
                            break;
                        }
                    }
                }
                if got {
                    for s in &old {
                        if let Some(d) = self.device(s.devid) {
                            d.write_at(s.phys * BS, &buf)?;
                        }
                    }
                }
            }
            off += run;
        }
        Ok(())
    }
}

impl BlockDevice for Volume {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        for p in self.pieces(offset, buf.len())? {
            let dst = &mut buf[p.at..p.at + p.len];
            if p.superblock {
                self.read_sb(offset + p.at as u64, dst)?;
                continue;
            }
            let mut ok = false;
            for (devid, phys) in self.read_order(&p.copies) {
                if let Some(d) = self.device(devid) {
                    if d.read_at(phys, dst).is_ok() {
                        ok = true;
                        break;
                    }
                }
            }
            if !ok {
                return Err(Error::Io);
            }
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        for p in self.pieces(offset, buf.len())? {
            let src = &buf[p.at..p.at + p.len];
            if p.superblock {
                let devs: Vec<(u32, Arc<dyn BlockDevice>)> = self
                    .devs
                    .with(|d| d.iter().map(|(&i, d)| (i, d.clone())).collect());
                let inner = offset + p.at as u64;
                for (id, d) in devs {
                    if inner == 0 && src.len() == BS as usize {
                        d.write_at(0, &(self.sb_for)(src, id))?;
                    } else {
                        d.write_at(inner, src)?;
                    }
                }
                continue;
            }
            let mut wrote = false;
            for &(devid, phys) in &p.copies {
                if let Some(d) = self.device(devid) {
                    d.write_at(phys, src)?;
                    wrote = true;
                }
            }
            if !wrote {
                return Err(Error::Io);
            }
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let devs: Vec<Arc<dyn BlockDevice>> = self.devs.with(|d| d.values().cloned().collect());
        for d in devs {
            d.sync()?;
        }
        Ok(())
    }

    fn size(&self) -> u64 {
        self.map.with(|m| m.end()) * BS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(devs: &[(u32, u64)]) -> ChunkMap {
        ChunkMap {
            devices: devs.iter().copied().collect(),
            chunks: BTreeMap::new(),
        }
    }

    fn add(m: &mut ChunkMap, g: u64, p: Profile, blocks: u64) -> Chunk {
        let (n, stripes) = m.place(p, blocks, &[], &[]).unwrap();
        let c = Chunk {
            kind: ChunkKind::Data,
            profile: p,
            blocks: n,
            stripes,
        };
        m.chunks.insert(g, c.clone());
        m.check().unwrap();
        c
    }

    #[test]
    fn profiles_place_their_stripes() {
        let mut m = map(&[(1, 10_000), (2, 10_000), (3, 10_000), (4, 10_000)]);
        let s = add(&mut m, 0, Profile::Single, 1000);
        assert_eq!(s.stripes.len(), 1);
        let d = add(&mut m, 1, Profile::Dup, 1000);
        assert_eq!(d.stripes[0].devid, d.stripes[1].devid);
        assert_ne!(d.stripes[0].phys, d.stripes[1].phys);
        let r = add(&mut m, 2, Profile::Raid1, 1000);
        assert_ne!(r.stripes[0].devid, r.stripes[1].devid);
        assert_eq!(add(&mut m, 3, Profile::Raid1c3, 1000).stripes.len(), 3);
        assert_eq!(add(&mut m, 4, Profile::Raid1c4, 1000).stripes.len(), 4);
        let z = add(&mut m, 5, Profile::Raid0, 1000);
        assert_eq!(z.stripes.len(), 4);
        assert_eq!(z.stripe_len(), 256);
        let t = add(&mut m, 6, Profile::Raid10, 1000);
        assert_eq!(t.stripes.len(), 4);
        assert_eq!(t.stripe_len(), 512);
    }

    #[test]
    fn locate_covers_every_block_once_per_copy() {
        let mut m = map(&[(1, 10_000), (2, 10_000), (3, 10_000), (4, 10_000)]);
        for (g, p) in Profile::ALL.into_iter().enumerate() {
            let c = add(&mut m, g as u64, p, 1000);
            let mut seen = alloc::collections::BTreeSet::new();
            let mut off = 0;
            while off < c.blocks {
                let (copies, run) = c.locate(off);
                assert_eq!(copies.len(), p.copies());
                assert!(run > 0);
                for s in copies {
                    for i in 0..run {
                        assert!(seen.insert((s.devid, s.phys + i)), "{:?}", p);
                        let st = c.stripes.iter().find(|x| {
                            x.devid == s.devid
                                && s.phys >= x.phys
                                && s.phys < x.phys + c.stripe_len()
                        });
                        assert!(st.is_some(), "{:?} outside its stripe", p);
                    }
                }
                off += run;
            }
            assert_eq!(seen.len() as u64, c.blocks * p.copies() as u64);
        }
    }

    #[test]
    fn placement_respects_space_and_devices() {
        let m = map(&[(1, 2000)]);
        assert!(m.place(Profile::Raid1, 1000, &[], &[]).is_none());
        assert!(m.place(Profile::Raid0, 1000, &[], &[]).is_none());
        // Shrinks to what fits.
        let (n, _) = m.place(Profile::Dup, 1500, &[], &[]).unwrap();
        assert_eq!(n, 750);
        assert!(m.place(Profile::Single, 1000, &[1], &[]).is_none());
        // Held ranges are not reused.
        let held = [(1, 0, 1990)];
        assert!(m.place(Profile::Single, 256, &[], &held).is_none());
        let m = map(&[(1, 3000), (2, 3000), (3, 3000)]);
        // RAID10 needs an even count of at least four.
        assert!(m.place(Profile::Raid10, 1000, &[], &[]).is_none());
        assert_eq!(m.place(Profile::Raid0, 1000, &[2], &[]).unwrap().1.len(), 2);
    }

    #[test]
    fn maps_round_trip_and_reject_damage() {
        let mut m = map(&[(1, 50_000), (2, 60_000), (7, 70_000)]);
        for (g, p) in Profile::ALL.into_iter().enumerate() {
            if p.min_devices() <= 3 {
                add(&mut m, g as u64 * 3, p, 2000);
            }
        }
        let b = m.encode();
        assert_eq!(ChunkMap::decode(&b).unwrap(), m);
        for i in 0..b.len() {
            let mut bad = b.clone();
            bad[i] ^= 0x10;
            assert!(ChunkMap::decode(&bad).is_err(), "byte {}", i);
        }
        assert!(ChunkMap::decode(&b[..b.len() - 1]).is_err());
        // Overlapping stripes are corrupt.
        let mut o = m.clone();
        let c = o.chunks.values().next().unwrap().clone();
        o.chunks.insert(100, c);
        assert!(o.check().is_err());
        assert_eq!(m.end(), 15 * GROUP_BLOCKS + 2000);
        assert_eq!(m.free_group(), 1);
    }

    #[test]
    fn sys_chunks_fit_the_superblock() {
        let c = Chunk {
            kind: ChunkKind::Metadata,
            profile: Profile::Raid1c4,
            blocks: 4096,
            stripes: (1..=4).map(|d| Stripe { devid: d, phys: 16 }).collect(),
        };
        let mut b = [0u8; 64];
        c.encode_sys(&mut b);
        assert_eq!(Chunk::decode_sys(&b).unwrap(), c);
        b[2] = 5;
        assert!(Chunk::decode_sys(&b).is_err());
    }
}
