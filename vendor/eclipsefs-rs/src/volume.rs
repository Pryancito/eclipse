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
//! | RAID5    | 1 + P  | every device, 4 KiB     | 2       |
//! | RAID6    | 1 + P+Q| every device, 4 KiB     | 3       |
//!
//! RAID5 and RAID6 chunks keep rows of `k` data blocks (one per device)
//! plus P (XOR) and, for RAID6, Q (Reed-Solomon, see [`crate::raid56`]),
//! rotating which device holds the parity from one row to the next. A row
//! is one block on each device, at the same place, so `R` consecutive rows
//! are `R` contiguous blocks on every device.
//!
//! **No write hole.** btrfs RAID5/6 can lose committed data when a crash
//! lands between a data block and its row's parity: the parity no longer
//! matches the *other* blocks of the row, which a later rebuild then gets
//! wrong. EclipseFS never writes a row that holds committed data: the
//! allocator ([`crate::blocks`]) hands out blocks of a parity chunk only
//! from rows that are wholly free or that the open transaction already
//! started, so every write lands in a row whose other blocks are free or
//! uncommitted, both of which a crash discards anyway. The free tail of a
//! row a transaction left half used waits until the whole row is freed.
//! Writes compute parity from what the row holds on disk (reconstruct
//! write), so a row left inconsistent by a crash is right again after its
//! next write. The one row rewritten in place, holding the group's bitmap,
//! goes through the journal, which rewrites it after a crash.
//!
//! Devices may be given a [`Tier`] (fast or slow): metadata and new data
//! go to chunks on fast devices while they have room, and
//! `tier_migrate` moves data chunks that are no longer being written to
//! the slow ones (bcachefs's metadata, foreground and background targets).
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
    Raid5,
    Raid6,
}

impl Profile {
    pub const ALL: [Profile; 9] = [
        Profile::Single,
        Profile::Dup,
        Profile::Raid0,
        Profile::Raid1,
        Profile::Raid10,
        Profile::Raid1c3,
        Profile::Raid1c4,
        Profile::Raid5,
        Profile::Raid6,
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
            Profile::Raid5 => 7,
            Profile::Raid6 => 8,
        }
    }

    pub fn from_id(id: u8) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.id() == id)
            .ok_or(Error::Corrupt("chunk profile"))
    }

    /// Whole copies of every block (parity profiles keep one, plus
    /// parity).
    pub fn copies(self) -> usize {
        match self {
            Profile::Single | Profile::Raid0 | Profile::Raid5 | Profile::Raid6 => 1,
            Profile::Dup | Profile::Raid1 | Profile::Raid10 => 2,
            Profile::Raid1c3 => 3,
            Profile::Raid1c4 => 4,
        }
    }

    /// Fewest devices it can be laid out on.
    pub fn min_devices(self) -> usize {
        match self {
            Profile::Single | Profile::Dup => 1,
            Profile::Raid0 | Profile::Raid1 | Profile::Raid5 => 2,
            Profile::Raid1c3 | Profile::Raid6 => 3,
            Profile::Raid10 | Profile::Raid1c4 => 4,
        }
    }

    /// Copies that may be lost without losing data.
    pub fn tolerates(self) -> usize {
        match self {
            Profile::Single | Profile::Raid0 => 0,
            Profile::Raid5 | Profile::Raid6 => self.parity(),
            p => p.copies() - 1,
        }
    }

    pub fn striped(self) -> bool {
        matches!(self, Profile::Raid0 | Profile::Raid10)
    }

    /// Parity blocks per row: 1 for RAID5, 2 for RAID6, else 0.
    pub fn parity(self) -> usize {
        match self {
            Profile::Raid5 => 1,
            Profile::Raid6 => 2,
            _ => 0,
        }
    }

    /// Raw blocks a block of data takes on `n` devices, as a fraction
    /// `(raw, data)`.
    pub fn overhead(self, n: usize) -> (u64, u64) {
        match self.parity() {
            0 => (self.copies() as u64, 1),
            p => {
                let n = n.max(p + 1) as u64;
                (n, n - p as u64)
            }
        }
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
            Profile::Raid5 => "raid5",
            Profile::Raid6 => "raid6",
        }
    }

    /// The profile called `name` (as [`name`](Self::name) spells it).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// How fast a device is, for tiered placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Tier {
    /// Not tiered (the default): used for anything.
    #[default]
    Any,
    /// SSD/NVMe: metadata, and new data until it is migrated.
    Fast,
    /// HDD: where data ends up.
    Slow,
}

impl Tier {
    pub fn id(self) -> u8 {
        match self {
            Tier::Any => 0,
            Tier::Fast => 1,
            Tier::Slow => 2,
        }
    }

    pub fn from_id(id: u8) -> Result<Self> {
        match id {
            0 => Ok(Tier::Any),
            1 => Ok(Tier::Fast),
            2 => Ok(Tier::Slow),
            _ => Err(Error::Corrupt("device tier")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tier::Any => "none",
            Tier::Fast => "fast",
            Tier::Slow => "slow",
        }
    }

    /// `fast`/`ssd`, `slow`/`hdd` or `none`/`any`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "fast" | "ssd" | "nvme" => Some(Tier::Fast),
            "slow" | "hdd" | "rotational" => Some(Tier::Slow),
            "none" | "any" => Some(Tier::Any),
            _ => None,
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
        Profile::Raid5 | Profile::Raid6 => {
            blocks.div_ceil(n.saturating_sub(profile.parity()).max(1) as u64)
        }
        _ => blocks,
    }
}

impl Chunk {
    pub fn stripe_len(&self) -> u64 {
        stripe_len(self.profile, self.blocks, self.stripes.len())
    }

    /// Data blocks per row of a parity chunk.
    pub fn data_cols(&self) -> usize {
        self.stripes.len() - self.profile.parity()
    }

    /// Stripe that holds column `col` of row `r` of a parity chunk:
    /// columns `0..k` are data, then P, then Q, rotated by one stripe per
    /// row so parity is spread over every device.
    pub fn col_stripe(&self, r: u64, col: usize) -> usize {
        let n = self.stripes.len();
        (col + (r % n as u64) as usize) % n
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
            Profile::Raid5 | Profile::Raid6 => {
                // The data block only; its parity is the volume's business.
                let k = self.data_cols() as u64;
                let r = off / k;
                let i = self.col_stripe(r, (off % k) as usize);
                (vec![at(s[i], r)], 1)
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
                Profile::Raid5 => n >= 2 && distinct,
                Profile::Raid6 => n >= 3 && distinct,
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
        if !c.valid() || c.profile.striped() || c.profile.parity() > 0 {
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
    /// Device id -> tier, for devices that have one (map version 2).
    pub tiers: BTreeMap<u32, Tier>,
}

/// A physical range: device, first block, blocks.
pub type PhysRange = (u32, u64, u64);

/// A device's free blocks, id and free extents.
type DevRoom = (u64, u32, Vec<(u64, u64)>);

impl ChunkMap {
    /// `EFSCHMAP`, version, devices, (version 2: device tiers), chunks,
    /// CRC32C of everything before it. Version 1 unless a device has a
    /// tier, so maps without tiers stay readable by older drivers.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAP_MAGIC);
        let version: u32 = if self.tiers.is_empty() { 1 } else { 2 };
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(self.devices.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        for (&id, &size) in &self.devices {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
        }
        if version == 2 {
            out.extend_from_slice(&(self.tiers.len() as u32).to_le_bytes());
            for (&id, &t) in &self.tiers {
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&[t.id(), 0, 0, 0]);
            }
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
        if b.len() < 28 || &b[0..8] != MAP_MAGIC || !(1..=2).contains(&le32(b, 8)) {
            return Err(Error::Corrupt("chunk map header"));
        }
        let version = le32(b, 8);
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
        if version == 2 {
            if at + 4 > b.len() {
                return Err(Error::Corrupt("chunk map length"));
            }
            let n = le32(b, at) as usize;
            at += 4;
            for _ in 0..n {
                if at + 8 > b.len() {
                    return Err(Error::Corrupt("chunk map length"));
                }
                let t = Tier::from_id(b[at + 4])?;
                if b[at + 5..at + 8] != [0, 0, 0] || t == Tier::Any {
                    return Err(Error::Corrupt("device tier"));
                }
                map.tiers.insert(le32(b, at), t);
                at += 8;
            }
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
        if self.tiers.keys().any(|d| !self.devices.contains_key(d)) {
            return Err(Error::Corrupt("tier of an unknown device"));
        }
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

    /// The tier of device `devid`.
    pub fn tier(&self, devid: u32) -> Tier {
        self.tiers.get(&devid).copied().unwrap_or_default()
    }

    /// Set (or, with [`Tier::Any`], clear) the tier of device `devid`.
    pub fn set_tier(&mut self, devid: u32, tier: Tier) {
        if tier == Tier::Any {
            self.tiers.remove(&devid);
        } else {
            self.tiers.insert(devid, tier);
        }
    }

    /// Whether both a fast and a slow device are there: only then does
    /// placement look at tiers.
    pub fn tiered(&self) -> bool {
        self.tiers.values().any(|&t| t == Tier::Fast)
            && self.tiers.values().any(|&t| t == Tier::Slow)
    }

    /// The tier a chunk is on: that of all its devices, or `Any` when they
    /// differ.
    pub fn chunk_tier(&self, c: &Chunk) -> Tier {
        let mut it = c.stripes.iter().map(|s| self.tier(s.devid));
        let first = it.next().unwrap_or_default();
        if it.all(|t| t == first) {
            first
        } else {
            Tier::Any
        }
    }

    /// Devices not on `tier` (to give [`place`](Self::place) as `avoid`).
    pub fn off_tier(&self, tier: Tier) -> Vec<u32> {
        self.devices
            .keys()
            .copied()
            .filter(|&d| self.tier(d) != tier)
            .collect()
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
        if old.profile.striped()
            || profile.striped()
            || old.profile.parity() > 0
            || profile.parity() > 0
        {
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
            Profile::Raid0 | Profile::Raid10 | Profile::Raid5 | Profile::Raid6 => {
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
                .map(|c| match c.profile {
                    // Also every guess at which other data block of the
                    // row is rotten (RAID6 decodes any two erasures).
                    Profile::Raid6 => 2 + c.data_cols(),
                    p => p.copies() + p.parity(),
                })
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
        let ok = devs.iter().any(|(_, d)| d.read_at(inner, buf).is_ok());
        ok.then_some(()).ok_or(Error::Io)
    }

    /// Read `buf` at `offset` from copy `k` of every block (no fallback).
    /// In a parity chunk, copy 0 is the block as stored, copy 1 the block
    /// rebuilt from the rest of its row and P, copy 2 (RAID6) rebuilt
    /// without P either, and copies 3.. (RAID6) rebuilt as if one other
    /// data block of the row were lost too (the first, second... other
    /// one): with checksums above to tell which guess is right, that
    /// rebuilds a rotten block next to a missing or rotten one.
    pub fn read_copy(&self, offset: u64, buf: &mut [u8], k: usize) -> Result<()> {
        for (at, len, par) in self.segments(offset, buf.len()) {
            let dst = &mut buf[at..at + len];
            let pos = offset + at as u64;
            match par {
                Some((g, c)) => {
                    let mode = if k == 0 {
                        ReadMode::Direct
                    } else {
                        ReadMode::Rebuild(k)
                    };
                    self.parity_read_bytes(&c, pos - g * GROUP_BLOCKS * BS, dst, mode)?
                }
                None => self.plain_read_copy(pos, dst, k)?,
            }
        }
        Ok(())
    }

    fn plain_read_copy(&self, offset: u64, buf: &mut [u8], k: usize) -> Result<()> {
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

    /// Write `buf` at `offset` to copy `k` only (repairs). In a parity
    /// chunk, copy 0 writes the data blocks as given and copy 1 (2)
    /// rewrites P (Q) of their rows to match them.
    pub fn write_copy(&self, offset: u64, buf: &[u8], k: usize) -> Result<()> {
        for (at, len, par) in self.segments(offset, buf.len()) {
            let src = &buf[at..at + len];
            let pos = offset + at as u64;
            match par {
                Some((g, c)) => {
                    let rel = pos - g * GROUP_BLOCKS * BS;
                    if !rel.is_multiple_of(BS) || !(len as u64).is_multiple_of(BS) {
                        return Err(Error::Invalid);
                    }
                    self.parity_write_copy(&c, rel / BS, src, k)?
                }
                None => self.plain_write_copy(pos, src, k)?,
            }
        }
        Ok(())
    }

    fn plain_write_copy(&self, offset: u64, buf: &[u8], k: usize) -> Result<()> {
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
        if from.profile.parity() > 0 || to.profile.parity() > 0 {
            return self.copy_layouts(from, to, start, n);
        }
        let mut off = start;
        let mut buf = Vec::new();
        while off < start + n {
            let (src, a) = from.locate(off);
            let (dst, b) = to.locate(off);
            let run = a.min(b).min(start + n - off).min(256);
            buf.resize((run * BS) as usize, 0);
            let stale = self.stale();
            let mut src = src;
            src.sort_by_key(|s| stale.contains(&s.devid));
            let done = src.iter().any(|s| {
                self.device(s.devid)
                    .is_some_and(|d| d.read_at(s.phys * BS, &mut buf).is_ok())
            });
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
        if c.profile.parity() > 0 {
            return self.parity_resync(c, start, n);
        }
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
                let got = fresh.iter().any(|s| {
                    self.device(s.devid)
                        .is_some_and(|d| d.read_at(s.phys * BS, &mut buf).is_ok())
                });
                if got {
                    for (s, d) in old.iter().filter_map(|s| Some((s, self.device(s.devid)?))) {
                        d.write_at(s.phys * BS, &buf)?;
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
        for (at, len, par) in self.segments(offset, buf.len()) {
            let dst = &mut buf[at..at + len];
            let pos = offset + at as u64;
            match par {
                Some((g, c)) => {
                    self.parity_read_bytes(&c, pos - g * GROUP_BLOCKS * BS, dst, ReadMode::Normal)?
                }
                None => self.plain_read(pos, dst)?,
            }
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        for (at, len, par) in self.segments(offset, buf.len()) {
            let src = &buf[at..at + len];
            let pos = offset + at as u64;
            match par {
                Some((g, c)) => self.parity_write_bytes(&c, pos - g * GROUP_BLOCKS * BS, src)?,
                None => self.plain_write(pos, src)?,
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

impl Volume {
    fn plain_read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
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

    fn plain_write(&self, offset: u64, buf: &[u8]) -> Result<()> {
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
}

/// Rows of a parity chunk one step of an I/O handles (bounds its buffers).
const ROW_BATCH: u64 = 64;

/// How a parity read gets its blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    /// As stored, rebuilt from parity where a device is missing or fails.
    Normal,
    /// As stored, or fail.
    Direct,
    /// Rebuilt from the rest of the row: with P (1), or with Q alone (2).
    Rebuild(usize),
}

/// One row of a parity chunk: its columns (data, P, Q) and which of them
/// could not be read.
struct Row {
    cols: Vec<Vec<u8>>,
    lost: Vec<usize>,
}

/// Parts of an I/O: `(at, len, group and chunk)`, `None` outside parity
/// chunks.
type Segment = (usize, usize, Option<(u64, Chunk)>);

impl Volume {
    /// Parts of `offset..offset+len` (bytes), split where chunks change
    /// between parity and not.
    fn segments(&self, offset: u64, len: usize) -> Vec<Segment> {
        let gbytes = GROUP_BLOCKS * BS;
        let any = self
            .map
            .with(|m| m.chunks.values().any(|c| c.profile.parity() > 0));
        if !any {
            return vec![(0, len, None)];
        }
        let mut out: Vec<Segment> = Vec::new();
        let mut at = 0usize;
        while at < len {
            let pos = offset + at as u64;
            let g = pos / gbytes;
            let n = ((gbytes - pos % gbytes) as usize).min(len - at);
            let par = self
                .map
                .with(|m| m.chunks.get(&g).filter(|c| c.profile.parity() > 0).cloned());
            match (out.last_mut(), par) {
                (Some((_, l, None)), None) => *l += n,
                (_, p) => out.push((at, n, p.map(|c| (g, c)))),
            }
            at += n;
        }
        out
    }

    /// Stripes of `c` not to read: missing devices, and stale ones while
    /// parity can stand in for them.
    fn parity_bad(&self, c: &Chunk) -> Vec<bool> {
        let stale = self.stale();
        let missing: Vec<bool> = c
            .stripes
            .iter()
            .map(|s| self.device(s.devid).is_none())
            .collect();
        let with_stale: Vec<bool> = c
            .stripes
            .iter()
            .zip(&missing)
            .map(|(s, &m)| m || stale.contains(&s.devid))
            .collect();
        if with_stale.iter().filter(|&&b| b).count() <= c.profile.parity() {
            with_stale
        } else {
            missing
        }
    }

    /// Rows `r0..r0+rows` of stripe `s` (`None` if its device is bad or
    /// the read fails).
    fn stripe_rows(
        &self,
        c: &Chunk,
        bad: &[bool],
        s: usize,
        r0: u64,
        rows: u64,
    ) -> Option<Vec<u8>> {
        if bad[s] {
            return None;
        }
        let st = c.stripes[s];
        let d = self.device(st.devid)?;
        let mut buf = vec![0u8; (rows * BS) as usize];
        d.read_at((st.phys + r0) * BS, &mut buf).ok()?;
        Some(buf)
    }

    /// Every column of row `r`, as read (lost ones zeroed).
    fn read_row(&self, c: &Chunk, bad: &[bool], r: u64) -> Row {
        let n = c.stripes.len();
        let mut row = Row {
            cols: Vec::with_capacity(n),
            lost: Vec::new(),
        };
        for col in 0..n {
            match self.stripe_rows(c, bad, c.col_stripe(r, col), r, 1) {
                Some(b) => row.cols.push(b),
                None => {
                    row.cols.push(vec![0u8; BS as usize]);
                    row.lost.push(col);
                }
            }
        }
        row
    }

    /// Bytes at chunk-relative `off` of parity chunk `c`.
    fn parity_read_bytes(&self, c: &Chunk, off: u64, dst: &mut [u8], mode: ReadMode) -> Result<()> {
        if off.is_multiple_of(BS) && (dst.len() as u64).is_multiple_of(BS) {
            return self.parity_read(c, off / BS, dst, mode);
        }
        let first = off / BS;
        let end = (off + dst.len() as u64).div_ceil(BS);
        let mut tmp = vec![0u8; ((end - first) * BS) as usize];
        self.parity_read(c, first, &mut tmp, mode)?;
        let skip = (off - first * BS) as usize;
        dst.copy_from_slice(&tmp[skip..skip + dst.len()]);
        Ok(())
    }

    /// Whole blocks from block `b0` of parity chunk `c` into `dst`.
    fn parity_read(&self, c: &Chunk, b0: u64, dst: &mut [u8], mode: ReadMode) -> Result<()> {
        let k = c.data_cols() as u64;
        let n = c.stripes.len();
        let bs = BS as usize;
        let end = b0 + dst.len() as u64 / BS;
        if end > c.blocks {
            return Err(Error::Corrupt("block in no chunk"));
        }
        let bad = self.parity_bad(c);
        let mut b = b0;
        while b < end {
            let r0 = b / k;
            let r1 = ((end - 1) / k).min(r0 + ROW_BATCH - 1);
            let rows = r1 - r0 + 1;
            let stop = ((r1 + 1) * k).min(end);
            let mut bufs: Vec<Option<Option<Vec<u8>>>> = vec![None; n];
            let mut direct = matches!(mode, ReadMode::Normal | ReadMode::Direct);
            if direct {
                for x in b..stop {
                    let s = c.col_stripe(x / k, (x % k) as usize);
                    if bufs[s].is_none() {
                        bufs[s] = Some(self.stripe_rows(c, &bad, s, r0, rows));
                    }
                    let Some(Some(sb)) = &bufs[s] else {
                        direct = false;
                        break;
                    };
                    let i = (x / k - r0) as usize;
                    let o = ((x - b0) as usize) * bs;
                    dst[o..o + bs].copy_from_slice(&sb[i * bs..(i + 1) * bs]);
                }
                if !direct && mode == ReadMode::Direct {
                    return Err(Error::Io);
                }
            }
            if !direct {
                for (s, slot) in bufs.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(self.stripe_rows(c, &bad, s, r0, rows));
                    }
                }
                for x in b..stop {
                    let r = x / k;
                    let i = (r - r0) as usize;
                    let col = (x % k) as usize;
                    let mut row = Row {
                        cols: Vec::with_capacity(n),
                        lost: Vec::new(),
                    };
                    for cc in 0..n {
                        match &bufs[c.col_stripe(r, cc)] {
                            Some(Some(sb)) => row.cols.push(sb[i * bs..(i + 1) * bs].to_vec()),
                            _ => {
                                row.cols.push(vec![0u8; bs]);
                                row.lost.push(cc);
                            }
                        }
                    }
                    let mut miss = row.lost.clone();
                    if let ReadMode::Rebuild(m) = mode {
                        miss.push(col);
                        if m >= 2 && c.profile.parity() < 2 {
                            return Err(Error::Invalid);
                        }
                        if m == 2 {
                            miss.push(k as usize);
                        } else if m >= 3 {
                            let other = (0..k as usize)
                                .filter(|&o| o != col)
                                .nth(m - 3)
                                .ok_or(Error::Invalid)?;
                            miss.push(other);
                        }
                        miss.sort_unstable();
                        miss.dedup();
                    }
                    if miss.contains(&col) {
                        crate::raid56::recover(&mut row.cols, k as usize, &miss)?;
                    }
                    let o = ((x - b0) as usize) * bs;
                    dst[o..o + bs].copy_from_slice(&row.cols[col]);
                }
            }
            b = stop;
        }
        Ok(())
    }

    /// Bytes at chunk-relative `off` of parity chunk `c`; a partial block
    /// is merged with what it holds.
    fn parity_write_bytes(&self, c: &Chunk, off: u64, src: &[u8]) -> Result<()> {
        if off.is_multiple_of(BS) && (src.len() as u64).is_multiple_of(BS) {
            return self.parity_write(c, off / BS, src);
        }
        let first = off / BS;
        let end = (off + src.len() as u64).div_ceil(BS);
        let mut tmp = vec![0u8; ((end - first) * BS) as usize];
        self.parity_read(c, first, &mut tmp, ReadMode::Normal)?;
        let skip = (off - first * BS) as usize;
        tmp[skip..skip + src.len()].copy_from_slice(src);
        self.parity_write(c, first, &tmp)
    }

    /// Recompute P (and Q) of `row` from its data columns.
    fn regen(row: &mut Row, k: usize, parity: usize) {
        let (data, par) = row.cols.split_at_mut(k);
        let d: Vec<&[u8]> = data.iter().map(|b| &b[..]).collect();
        let (p, q) = par.split_at_mut(1);
        crate::raid56::gen(
            &d,
            &mut p[0],
            q.first_mut().map(|q| &mut q[..]).filter(|_| parity == 2),
        );
    }

    /// Whole blocks from block `b0` of parity chunk `c`, with their rows'
    /// parity. Rows only partly written take the rest from the disk
    /// (rebuilt from the old parity where a device is missing).
    fn parity_write(&self, c: &Chunk, b0: u64, src: &[u8]) -> Result<()> {
        let k = c.data_cols() as u64;
        let n = c.stripes.len();
        let parity = c.profile.parity();
        let bs = BS as usize;
        let end = b0 + src.len() as u64 / BS;
        if end > c.blocks {
            return Err(Error::Corrupt("block in no chunk"));
        }
        let bad = self.parity_bad(c);
        let mut b = b0;
        while b < end {
            let r0 = b / k;
            let r1 = ((end - 1) / k).min(r0 + ROW_BATCH - 1);
            let rows = r1 - r0 + 1;
            let stop = ((r1 + 1) * k).min(end);
            let mut out: Vec<Vec<u8>> = vec![vec![0u8; rows as usize * bs]; n];
            for r in r0..=r1 {
                let (first, last) = (r * k, r * k + k);
                let (lo, hi) = (first.max(b), last.min(stop));
                let mut row = if lo == first && hi == last {
                    Row {
                        cols: vec![vec![0u8; bs]; n],
                        lost: Vec::new(),
                    }
                } else {
                    let mut row = self.read_row(c, &bad, r);
                    let kept_lost = row
                        .lost
                        .iter()
                        .any(|&col| col < k as usize && !(lo..hi).contains(&(first + col as u64)));
                    if kept_lost {
                        crate::raid56::recover(&mut row.cols, k as usize, &row.lost)?;
                    }
                    row
                };
                for x in lo..hi {
                    let o = ((x - b0) as usize) * bs;
                    row.cols[(x - first) as usize].copy_from_slice(&src[o..o + bs]);
                }
                Self::regen(&mut row, k as usize, parity);
                let i = (r - r0) as usize;
                for (col, data) in row.cols.iter().enumerate() {
                    out[c.col_stripe(r, col)][i * bs..(i + 1) * bs].copy_from_slice(data);
                }
            }
            let mut wrote = 0;
            for (s, buf) in out.iter().enumerate() {
                let st = c.stripes[s];
                if let Some(d) = self.device(st.devid) {
                    d.write_at((st.phys + r0) * BS, buf)?;
                    wrote += 1;
                }
            }
            if wrote + parity < n {
                return Err(Error::Io);
            }
            b = stop;
        }
        Ok(())
    }

    /// Repair writes (see [`write_copy`](Self::write_copy)).
    fn parity_write_copy(&self, c: &Chunk, b0: u64, src: &[u8], which: usize) -> Result<()> {
        let k = c.data_cols() as u64;
        let parity = c.profile.parity();
        if which > parity {
            // A guess (RAID6 copies 3..) has no place of its own.
            return Err(Error::Unsupported("rebuilt copy"));
        }
        let bad = self.parity_bad(c);
        for (i, img) in src.chunks(BS as usize).enumerate() {
            let x = b0 + i as u64;
            let (r, col) = (x / k, (x % k) as usize);
            let (target, data) = if which == 0 {
                (col, img.to_vec())
            } else {
                let target = k as usize + which - 1;
                let mut row = self.read_row(c, &bad, r);
                let mut miss = row.lost.clone();
                miss.retain(|&m| m != col && m != target);
                if !miss.is_empty() {
                    miss.push(col);
                    crate::raid56::recover(&mut row.cols, k as usize, &miss)?;
                }
                row.cols[col].copy_from_slice(img);
                Self::regen(&mut row, k as usize, parity);
                (target, row.cols.swap_remove(target))
            };
            let st = c.stripes[c.col_stripe(r, target)];
            self.device(st.devid)
                .ok_or(Error::Io)?
                .write_at((st.phys + r) * BS, &data)?;
        }
        Ok(())
    }

    /// Blocks `start..start+n` of a group from layout `from` to layout
    /// `to` when either keeps parity.
    fn copy_layouts(&self, from: &Chunk, to: &Chunk, start: u64, n: u64) -> Result<()> {
        let mut off = start;
        let mut buf = Vec::new();
        while off < start + n {
            let run = (start + n - off).min(256);
            buf.resize((run * BS) as usize, 0);
            self.layout_read(from, off, &mut buf)?;
            self.layout_write(to, off, &buf)?;
            off += run;
        }
        Ok(())
    }

    /// Blocks from `off` of a chunk laid out as `c` (which need not be
    /// the map's).
    fn layout_read(&self, c: &Chunk, off: u64, buf: &mut [u8]) -> Result<()> {
        if c.profile.parity() > 0 {
            return self.parity_read(c, off, buf, ReadMode::Normal);
        }
        let stale = self.stale();
        let mut done = 0u64;
        let n = buf.len() as u64 / BS;
        while done < n {
            let (mut src, run) = c.locate(off + done);
            let run = run.min(n - done);
            src.sort_by_key(|s| stale.contains(&s.devid));
            let dst = &mut buf[(done * BS) as usize..((done + run) * BS) as usize];
            let ok = src.iter().any(|s| {
                self.device(s.devid)
                    .is_some_and(|d| d.read_at(s.phys * BS, dst).is_ok())
            });
            if !ok {
                return Err(Error::Io);
            }
            done += run;
        }
        Ok(())
    }

    fn layout_write(&self, c: &Chunk, off: u64, buf: &[u8]) -> Result<()> {
        if c.profile.parity() > 0 {
            return self.parity_write(c, off, buf);
        }
        let mut done = 0u64;
        let n = buf.len() as u64 / BS;
        while done < n {
            let (dst, run) = c.locate(off + done);
            let run = run.min(n - done);
            let src = &buf[(done * BS) as usize..((done + run) * BS) as usize];
            for s in dst {
                self.device(s.devid)
                    .ok_or(Error::Io)?
                    .write_at(s.phys * BS, src)?;
            }
            done += run;
        }
        Ok(())
    }

    /// Rebuild what stale devices hold of rows covering `start..start+n`
    /// from the other devices.
    fn parity_resync(&self, c: &Chunk, start: u64, n: u64) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        let stale = self.stale();
        let k = c.data_cols() as u64;
        let old: Vec<bool> = c
            .stripes
            .iter()
            .map(|s| stale.contains(&s.devid) || self.device(s.devid).is_none())
            .collect();
        if !old.iter().any(|&o| o) {
            return Ok(());
        }
        for r in start / k..=(start + n - 1) / k {
            let mut row = self.read_row(c, &old, r);
            if row.lost.len() > c.profile.parity() {
                continue;
            }
            crate::raid56::recover(&mut row.cols, k as usize, &row.lost)?;
            for &col in &row.lost {
                let st = c.stripes[c.col_stripe(r, col)];
                let Some(d) = self.device(st.devid) else {
                    continue;
                };
                d.write_at((st.phys + r) * BS, &row.cols[col])?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn map(devs: &[(u32, u64)]) -> ChunkMap {
        ChunkMap {
            devices: devs.iter().copied().collect(),
            chunks: BTreeMap::new(),
            tiers: BTreeMap::new(),
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
        assert_eq!(m.end(), 24 * GROUP_BLOCKS + 2000);
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

    /// An in-memory device.
    struct Mem(Spin<Vec<u8>>);

    impl Mem {
        fn new(blocks: u64) -> Arc<Self> {
            Arc::new(Mem(Spin::new(vec![0u8; (blocks * BS) as usize])))
        }
        fn poke(&self, at: u64, v: u8) {
            self.0.with(|d| d[at as usize] ^= v);
        }
    }

    impl BlockDevice for Mem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.0.with(|d| {
                let o = offset as usize;
                let src = d.get(o..o + buf.len()).ok_or(Error::Io)?;
                buf.copy_from_slice(src);
                Ok(())
            })
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            self.0.with(|d| {
                let o = offset as usize;
                d.get_mut(o..o + buf.len())
                    .ok_or(Error::Io)?
                    .copy_from_slice(buf);
                Ok(())
            })
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.with(|d| d.len() as u64)
        }
    }

    fn noop_sb(b: &[u8], _: u32) -> Vec<u8> {
        b.to_vec()
    }

    /// A volume of `n` devices with group 1 a `p` chunk of `blocks`.
    fn parity_volume(n: u32, p: Profile, blocks: u64) -> (Volume, Vec<Arc<Mem>>, Chunk) {
        let size = 20_000;
        let mut m = map(&(1..=n).map(|d| (d, size)).collect::<Vec<_>>());
        m.chunks.insert(
            0,
            Chunk {
                kind: ChunkKind::Metadata,
                profile: Profile::Single,
                blocks: 64,
                stripes: vec![Stripe {
                    devid: 1,
                    phys: DEV_RESERVED,
                }],
            },
        );
        let held = [(1, DEV_RESERVED, 64)];
        let (got, stripes) = m.place(p, blocks, &[], &held).unwrap();
        assert_eq!(got, blocks);
        let c = Chunk {
            kind: ChunkKind::Data,
            profile: p,
            blocks,
            stripes,
        };
        m.chunks.insert(1, c.clone());
        m.check().unwrap();
        let mems: Vec<Arc<Mem>> = (0..n).map(|_| Mem::new(size)).collect();
        let devs = mems
            .iter()
            .enumerate()
            .map(|(i, d)| (i as u32 + 1, d.clone() as Arc<dyn BlockDevice>))
            .collect();
        (Volume::new(devs, m, noop_sb), mems, c)
    }

    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    const G1: u64 = GROUP_BLOCKS * BS;

    /// Every row's parity matches its data, as stored.
    fn parity_consistent(v: &Volume, c: &Chunk) {
        let k = c.data_cols();
        let bad = vec![false; c.stripes.len()];
        for r in 0..c.stripe_len() {
            let row = v.read_row(c, &bad, r);
            let mut p = vec![0u8; BS as usize];
            let mut q = vec![0u8; BS as usize];
            let d: Vec<&[u8]> = row.cols[..k].iter().map(|b| &b[..]).collect();
            crate::raid56::gen(&d, &mut p, (c.profile.parity() == 2).then_some(&mut q[..]));
            assert_eq!(row.cols[k], p, "P of row {}", r);
            if c.profile.parity() == 2 {
                assert_eq!(row.cols[k + 1], q, "Q of row {}", r);
            }
        }
    }

    #[test]
    fn parity_chunks_place_and_locate() {
        for (n, p) in [
            (2, Profile::Raid5),
            (4, Profile::Raid5),
            (3, Profile::Raid6),
            (5, Profile::Raid6),
        ] {
            let (_, _, c) = parity_volume(n, p, 1000);
            assert_eq!(c.stripes.len(), n as usize);
            assert_eq!(c.data_cols(), n as usize - p.parity());
            assert_eq!(c.stripe_len(), 1000u64.div_ceil(c.data_cols() as u64));
            // Every data block lands once, never on a parity column.
            let mut seen = alloc::collections::BTreeSet::new();
            for off in 0..c.blocks {
                let (copies, run) = c.locate(off);
                assert_eq!((copies.len(), run), (1, 1));
                assert!(seen.insert((copies[0].devid, copies[0].phys)));
            }
            // Parity rotates over every device.
            let k = c.data_cols();
            let holders: alloc::collections::BTreeSet<usize> =
                (0..n as u64).map(|r| c.col_stripe(r, k)).collect();
            assert_eq!(holders.len(), n as usize);
        }
    }

    #[test]
    fn parity_writes_read_back_and_keep_rows_consistent() {
        for (n, p) in [
            (2, Profile::Raid5),
            (3, Profile::Raid5),
            (4, Profile::Raid6),
            (6, Profile::Raid6),
        ] {
            let (v, _, c) = parity_volume(n, p, 700);
            let mut model = pattern((700 * BS) as usize, n);
            v.write_at(G1, &model).unwrap();
            let mut back = vec![0u8; model.len()];
            v.read_at(G1, &mut back).unwrap();
            assert!(back == model, "{:?} on {}", p, n);
            parity_consistent(&v, &c);
            // Partial rows, unaligned bytes, single blocks.
            for (i, (at, len)) in [
                (5 * BS, BS),
                (17 * BS + 100, 3 * BS),
                (BS - 1, 2),
                (99 * BS, 77 * BS),
            ]
            .into_iter()
            .enumerate()
            {
                let data = pattern(len as usize, 1000 + i as u32);
                v.write_at(G1 + at, &data).unwrap();
                model[at as usize..(at + len) as usize].copy_from_slice(&data);
                let mut b = vec![0u8; len as usize];
                v.read_at(G1 + at, &mut b).unwrap();
                assert_eq!(b, data);
            }
            v.read_at(G1, &mut back).unwrap();
            assert!(back == model);
            parity_consistent(&v, &c);
        }
    }

    #[test]
    fn parity_survives_missing_devices() {
        for (n, p) in [
            (3, Profile::Raid5),
            (4, Profile::Raid6),
            (5, Profile::Raid6),
        ] {
            let (v, _, _) = parity_volume(n, p, 900);
            let model = pattern((900 * BS) as usize, 7 + n);
            v.write_at(G1, &model).unwrap();
            let gone: Vec<u32> = (1..=p.parity() as u32).collect();
            for &d in &gone {
                v.detach(d);
            }
            let mut back = vec![0u8; model.len()];
            v.read_at(G1, &mut back).unwrap();
            assert!(back == model, "{:?} without {:?}", p, gone);
            // Degraded writes still land (in parity) and read back.
            let mut model = model;
            let data = pattern(10 * BS as usize, 99);
            v.write_at(G1 + 33 * BS, &data).unwrap();
            model[(33 * BS) as usize..(43 * BS) as usize].copy_from_slice(&data);
            v.read_at(G1, &mut back).unwrap();
            assert!(back == model);
            // One more missing device is too many.
            v.detach(p.parity() as u32 + 1);
            assert!(v.read_at(G1, &mut back).is_err());
        }
    }

    #[test]
    fn parity_copies_rebuild_and_repair() {
        for (n, p) in [(4, Profile::Raid5), (5, Profile::Raid6)] {
            let (v, mems, c) = parity_volume(n, p, 400);
            let model = pattern((400 * BS) as usize, 3);
            v.write_at(G1, &model).unwrap();
            let blk = 41u64;
            let want = &model[(blk * BS) as usize..((blk + 1) * BS) as usize];
            let ways = if p == Profile::Raid6 {
                2 + c.data_cols()
            } else {
                2
            };
            assert_eq!(v.copies(GROUP_BLOCKS + blk), ways);
            // Rot the block where it is stored.
            let (at, _) = c.locate(blk);
            mems[at[0].devid as usize - 1].poke(at[0].phys * BS + 9, 0x40);
            let mut img = vec![0u8; BS as usize];
            v.read_copy(G1 + blk * BS, &mut img, 0).unwrap();
            assert_ne!(img, want);
            for k in 1..=p.parity() {
                v.read_copy(G1 + blk * BS, &mut img, k).unwrap();
                assert_eq!(img, want, "{:?} copy {}", p, k);
            }
            v.write_copy(G1 + blk * BS, want, 0).unwrap();
            v.read_copy(G1 + blk * BS, &mut img, 0).unwrap();
            assert_eq!(img, want);
            parity_consistent(&v, &c);
            // Rot P instead: copy 1 is wrong, the data and Q are fine.
            let r = blk / c.data_cols() as u64;
            let ps = c.stripes[c.col_stripe(r, c.data_cols())];
            mems[ps.devid as usize - 1].poke((ps.phys + r) * BS + 3, 0x08);
            v.read_copy(G1 + blk * BS, &mut img, 1).unwrap();
            assert_ne!(img, want);
            if p == Profile::Raid6 {
                v.read_copy(G1 + blk * BS, &mut img, 2).unwrap();
                assert_eq!(img, want);
            }
            v.write_copy(G1 + blk * BS, want, 1).unwrap();
            v.read_copy(G1 + blk * BS, &mut img, 1).unwrap();
            assert_eq!(img, want);
            parity_consistent(&v, &c);
            if p == Profile::Raid6 {
                // Rot the block and a neighbour in its row: P alone cannot
                // help, but one of the guesses can.
                let k = c.data_cols() as u64;
                let other = r * k + (blk + 1) % k;
                for b in [blk, other] {
                    let (at, _) = c.locate(b);
                    mems[at[0].devid as usize - 1].poke(at[0].phys * BS + 1, 0x11);
                }
                let mut hits = 0;
                for g in 1..ways {
                    if v.read_copy(G1 + blk * BS, &mut img, g).is_ok() && img == want {
                        hits += 1;
                    }
                }
                assert_eq!(hits, 1);
                assert!(v.write_copy(G1 + blk * BS, want, 3).is_err());
            }
        }
    }

    #[test]
    fn layouts_convert_to_and_from_parity() {
        let (v, _, c5) = parity_volume(4, Profile::Raid5, 300);
        let model = pattern((300 * BS) as usize, 11);
        v.write_at(G1, &model).unwrap();
        let map = v.map();
        let held: Vec<PhysRange> = c5
            .stripes
            .iter()
            .map(|s| (s.devid, s.phys, c5.stripe_len()))
            .chain([(1, DEV_RESERVED, 64)])
            .collect();
        let s6 = map.place_exact(Profile::Raid6, 300, &[], &held).unwrap();
        let c6 = Chunk {
            kind: ChunkKind::Data,
            profile: Profile::Raid6,
            blocks: 300,
            stripes: s6,
        };
        v.copy_between(&c5, &c6, 0, 300).unwrap();
        v.with_map(|m| m.chunks.insert(1, c6.clone()));
        let mut back = vec![0u8; model.len()];
        v.read_at(G1, &mut back).unwrap();
        assert!(back == model);
        parity_consistent(&v, &c6);
        // And to RAID1.
        let mut held2 = held.clone();
        held2.extend(
            c6.stripes
                .iter()
                .map(|s| (s.devid, s.phys, c6.stripe_len())),
        );
        let s1 = map.place_exact(Profile::Raid1, 300, &[], &held2).unwrap();
        let c1 = Chunk {
            kind: ChunkKind::Data,
            profile: Profile::Raid1,
            blocks: 300,
            stripes: s1,
        };
        v.copy_between(&c6, &c1, 0, 300).unwrap();
        v.with_map(|m| m.chunks.insert(1, c1));
        v.read_at(G1, &mut back).unwrap();
        assert!(back == model);
    }

    #[test]
    fn stale_devices_resync_their_parity_columns() {
        let (v, mems, c) = parity_volume(4, Profile::Raid6, 200);
        let model = pattern((200 * BS) as usize, 5);
        v.write_at(G1, &model).unwrap();
        // Device 2 "missed" the writes: wipe what it holds.
        let st = c.stripes.iter().find(|s| s.devid == 2).unwrap();
        mems[1]
            .write_at(st.phys * BS, &vec![0u8; (c.stripe_len() * BS) as usize])
            .unwrap();
        v.set_stale(vec![2]);
        let mut back = vec![0u8; model.len()];
        v.read_at(G1, &mut back).unwrap();
        assert!(back == model, "stale device read as current");
        v.resync(&c, 0, 200).unwrap();
        v.set_stale(Vec::new());
        parity_consistent(&v, &c);
        v.read_at(G1, &mut back).unwrap();
        assert!(back == model);
    }

    #[test]
    fn tiers_round_trip_and_steer_placement() {
        let mut m = map(&[(1, 50_000), (2, 50_000), (3, 50_000)]);
        assert!(!m.tiered());
        let v1 = m.encode();
        assert_eq!(le32(&v1, 8), 1);
        m.set_tier(1, Tier::Fast);
        m.set_tier(2, Tier::Slow);
        m.set_tier(3, Tier::Slow);
        assert!(m.tiered());
        let b = m.encode();
        assert_eq!(le32(&b, 8), 2);
        assert_eq!(ChunkMap::decode(&b).unwrap(), m);
        for i in 0..b.len() {
            let mut bad = b.clone();
            bad[i] ^= 0x10;
            assert!(ChunkMap::decode(&bad).is_err(), "byte {}", i);
        }
        let (_, s) = m
            .place(Profile::Single, 1000, &m.off_tier(Tier::Fast), &[])
            .unwrap();
        assert_eq!(s[0].devid, 1);
        let (_, s) = m
            .place(Profile::Raid1, 1000, &m.off_tier(Tier::Slow), &[])
            .unwrap();
        let c = Chunk {
            kind: ChunkKind::Data,
            profile: Profile::Raid1,
            blocks: 1000,
            stripes: s,
        };
        assert_eq!(m.chunk_tier(&c), Tier::Slow);
        // A tier of a device the map does not know is corrupt.
        m.tiers.insert(9, Tier::Fast);
        assert!(m.check().is_err());
        m.set_tier(9, Tier::Any);
        assert!(m.check().is_ok());
        assert_eq!(Tier::from_name("ssd"), Some(Tier::Fast));
        assert_eq!(Profile::from_name("raid6"), Some(Profile::Raid6));
    }

    #[test]
    fn profiles_tiers_and_small_maps() {
        assert_eq!(Profile::Raid5.tolerates(), 1);
        assert_eq!(Profile::Raid6.tolerates(), 2);
        for t in [Tier::Any, Tier::Fast, Tier::Slow] {
            assert_eq!(Tier::from_id(t.id()).unwrap(), t);
            assert_eq!(Tier::from_name(t.name()), Some(t));
        }
        assert_eq!(Tier::from_name("hdd"), Some(Tier::Slow));
        assert_eq!(Tier::from_name("any"), Some(Tier::Any));
        assert_eq!(Tier::from_name("tape"), None);

        // Group 0 is never striped.
        let striped = Chunk {
            kind: ChunkKind::Metadata,
            profile: Profile::Raid0,
            blocks: 64,
            stripes: (1..=2).map(|d| Stripe { devid: d, phys: 16 }).collect(),
        };
        let mut b = [0u8; 64];
        striped.encode_sys(&mut b);
        assert!(Chunk::decode_sys(&b).is_err());

        // A version 2 map that ends before its tiers.
        let mut m = map(&[(1, 50_000)]);
        m.set_tier(1, Tier::Fast);
        let b = m.encode();
        assert_eq!(
            ChunkMap::decode(&b[..36]),
            Err(Error::Corrupt("chunk map length"))
        );

        let mut m = map(&[(1, 1000), (2, 1000)]);
        let one = Stripe {
            devid: 1,
            phys: 950,
        };
        m.chunks.insert(
            0,
            Chunk {
                kind: ChunkKind::Data,
                profile: Profile::Raid1,
                blocks: 100,
                stripes: vec![one],
            },
        );
        assert_eq!(m.check(), Err(Error::Corrupt("chunk stripes")));
        m.chunks.insert(
            0,
            Chunk {
                kind: ChunkKind::Data,
                profile: Profile::Single,
                blocks: 100,
                stripes: vec![one],
            },
        );
        assert_eq!(
            m.check(),
            Err(Error::Corrupt("chunk past the end of its device"))
        );
        assert!(m.free_extents(9, &[]).is_empty());

        m.set_tier(1, Tier::Fast);
        m.set_tier(2, Tier::Slow);
        let both = Chunk {
            kind: ChunkKind::Data,
            profile: Profile::Raid1,
            blocks: 10,
            stripes: vec![one, Stripe { devid: 2, phys: 16 }],
        };
        assert_eq!(m.chunk_tier(&both), Tier::Any);
    }

    /// A single chunk moving off its device goes to the one with the most
    /// room.
    #[test]
    fn a_moving_single_chunk_goes_where_the_room_is() {
        let mut m = map(&[(1, 10_000), (2, 8_000), (3, 6_000), (4, 500)]);
        let c = add(&mut m, 0, Profile::Single, 1000);
        let avoid = [c.stripes[0].devid];
        let s = m.place_moving(&c, Profile::Single, &avoid, &[]).unwrap();
        assert_eq!(s.len(), 1);
        assert_ne!(s[0].devid, avoid[0]);
        let room = |d: u32| m.free_extents(d, &[]).iter().map(|e| e.1).sum::<u64>();
        let other = [2, 3]
            .into_iter()
            .filter(|&d| d != avoid[0] && d != s[0].devid)
            .next()
            .unwrap();
        assert!(room(s[0].devid) >= room(other));
    }

    /// I/O the volume refuses, or cannot do with what is missing.
    #[test]
    fn volume_io_that_cannot_be() {
        let (v, _mems, c) = parity_volume(4, Profile::Raid5, 100);
        let bs = BS as usize;
        // The superblock area: read from any device, never repaired.
        let mut sb = vec![0u8; bs];
        v.read_copy(0, &mut sb, 0).unwrap();
        v.write_copy(0, &sb, 0).unwrap();
        v.write_at(100, &[1; 10]).unwrap();
        // Parity chunks take whole blocks inside the chunk.
        assert_eq!(v.write_copy(G1 + 100, &sb, 0), Err(Error::Invalid));
        let past = G1 + c.blocks * BS;
        assert!(matches!(v.read_at(past, &mut sb), Err(Error::Corrupt(_))));
        assert!(matches!(v.write_at(past, &sb), Err(Error::Corrupt(_))));
        // RAID5 has no second rebuild.
        assert_eq!(v.read_copy(G1, &mut sb, 2), Err(Error::Invalid));
        // Groups with no chunk, read in one piece.
        let g3 = 3 * G1;
        let mut two = vec![0u8; 2 * bs];
        assert!(matches!(
            v.read_at(g3 - BS, &mut two),
            Err(Error::Corrupt(_))
        ));
        // Nothing to resync.
        v.resync(&c, 0, 0).unwrap();
        v.resync(&c, 0, 10).unwrap();

        // Missing devices.
        v.detach(2);
        v.resync(&c, 0, 10).unwrap();
        v.detach(3);
        // A whole row, so nothing needs reading first.
        let row = vec![0u8; 3 * bs];
        assert_eq!(v.write_at(G1, &row), Err(Error::Io));
        v.detach(1);
        assert_eq!(v.write_at(20 * BS, &sb), Err(Error::Io));
        v.detach(4);
        assert_eq!(v.read_copy(0, &mut sb, 0), Err(Error::Io));
        let g0 = v.map().chunks[&0].clone();
        assert_eq!(v.copy_between(&g0, &c, 0, 1), Err(Error::Io));
    }
}
