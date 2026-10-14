//! Online resize (`btrfs filesystem resize`).
//!
//! Growing only makes the new space usable. Shrinking first empties what
//! is cut off: the allocator is fenced off it, then every tree node and
//! data extent there is moved the way a change would move it, through
//! copy-on-write. Nodes are copied top-down ([`Store::relocate`]), so a
//! node shared with a snapshot stays shared below its copies. Data is
//! copied once per block, whatever the number of files, snapshots or
//! clones that point at it, and each of those references is then moved
//! to the copy, so sharing survives too. Each step commits like any other
//! change, so a crash leaves either the old size or the new one.
//!
//! On one device that is not a volume the cut is the end of the logical
//! space. On a volume, the chunks with a copy past the new end of the
//! device move inside it if there is room; a chunk with no room to go is
//! emptied as above and dropped.

use super::*;

/// Where the data blocks being moved went: old run start -> (length, new
/// start), plus compressed extents (moved whole) by their old start.
#[derive(Default)]
struct DataMoves {
    runs: BTreeMap<u64, (u64, u64)>,
    comp: BTreeMap<u64, u64>,
    /// Runs allocated for the copies, each holding one reference of its
    /// own until the moves are done.
    allocated: Vec<(u64, u64)>,
}

/// The key right after `k`.
fn next_key(k: Key) -> Option<Key> {
    if let Some(o) = k.offset.checked_add(1) {
        return Some(Key::new(k.objectid, k.ty, o));
    }
    if let Some(t) = k.ty.checked_add(1) {
        return Some(Key::new(k.objectid, t, 0));
    }
    k.objectid.checked_add(1).map(|o| Key::new(o, 0, 0))
}

/// Whether `start..start+len` touches `set`.
fn touches(set: &RangeSet, start: u64, len: u64) -> bool {
    set.iter().any(|(s, l)| s < start + len && start < s + l)
}

impl CowFs {
    /// `btrfs filesystem resize [devid:]size|max`: make device `devid`
    /// `size` bytes (rounded down to whole blocks), or all of the device
    /// for `None`. A size given this way stays: mounting no longer grows
    /// the filesystem to its device until a resize to `None`. Returns the
    /// new size in bytes.
    pub fn resize(&mut self, devid: u32, size: Option<u64>) -> Result<u64> {
        self.writable()?;
        let r = match self.vol.clone() {
            None => self.resize_single(devid, size),
            Some(vol) => self.resize_member(&vol, devid, size),
        }?;
        if size.is_some() {
            self.sb.features |= FEAT_FIXED_SIZE;
        } else {
            self.sb.features &= !FEAT_FIXED_SIZE;
        }
        // The superblock goes out with the next commit even if nothing
        // else changed.
        self.dev.write_at(0, &self.sb.encode())?;
        self.flush()?;
        Ok(r * BS)
    }

    /// Resize a filesystem that is not a volume: its logical space is its
    /// device's first blocks.
    fn resize_single(&mut self, devid: u32, size: Option<u64>) -> Result<u64> {
        if devid != 1 {
            return Err(Error::NotFound);
        }
        let device = self.raw.size() / BS;
        let want = size.map_or(device, |s| s / BS);
        if want > device {
            return Err(Error::Invalid);
        }
        let old = self.sb.total_blocks;
        if want >= old {
            self.grow_single(want)?;
        } else {
            self.shrink_single(want)?;
        }
        Ok(self.sb.total_blocks)
    }

    /// Cut the logical space at `want` blocks.
    fn shrink_single(&mut self, want: u64) -> Result<()> {
        // No last group shorter than a group may be.
        let rem = want % GROUP_BLOCKS;
        let target = if rem != 0 && rem < MIN_GROUP_BLOCKS {
            want - rem
        } else {
            want
        };
        let journal_end = self.sb.journal_start + self.sb.journal_blocks as u64;
        if target < MIN_GROUP_BLOCKS.max(journal_end) {
            return Err(Error::Invalid);
        }
        let old = self.sb.total_blocks;
        let mut cut = RangeSet::default();
        cut.insert(target, old - target);
        self.evacuate(&cut)?;
        // Everything past the end is free now (but group headers).
        let first_cut = target.div_ceil(GROUP_BLOCKS) as usize;
        {
            let groups = self.sp.alloc.groups();
            let clear = groups[first_cut..]
                .iter()
                .all(|g| g.free_blocks + 2 == g.block_count);
            let partial = groups
                .get((target / GROUP_BLOCKS) as usize)
                .filter(|_| rem != 0 && target == want)
                .is_none_or(|g| (rem..g.block_count).all(|o| !g.is_used(o)));
            if !clear || !partial {
                return Err(Error::NoSpace);
            }
        }
        let groups = self.sp.alloc.groups_mut();
        groups.truncate(first_cut);
        if target % GROUP_BLOCKS != 0 {
            let last = groups.last_mut().ok_or(Error::Corrupt("no groups"))?;
            last.shrink(target % GROUP_BLOCKS)?;
        }
        self.sb.total_blocks = target;
        self.st.total_blocks = target;
        self.sp.reserve = metadata_reserve(target);
        Ok(())
    }

    /// Resize member `devid` of a volume.
    fn resize_member(&mut self, vol: &Arc<Volume>, devid: u32, size: Option<u64>) -> Result<u64> {
        let cur = *vol.map().devices.get(&devid).ok_or(Error::NotFound)?;
        let dev = vol
            .device(devid)
            .ok_or(Error::Unsupported("device missing"))?;
        let device = dev.size() / BS;
        let want = size.map_or(device, |s| s / BS);
        if want > device || want < DEV_RESERVED + MIN_CHUNK {
            return Err(Error::Invalid);
        }
        if want == cur {
            return Ok(want);
        }
        if want > cur {
            vol.with_map(|m| m.devices.insert(devid, want));
            self.sp.chunks_dirty = true;
            return Ok(want);
        }
        self.flush()?;
        self.prepare_moves(vol)?;
        // New chunks stay inside from now on.
        vol.with_map(|m| m.devices.insert(devid, want));
        self.sp.chunks_dirty = true;
        if let Err(e) = self.empty_device_tail(vol, devid, want) {
            vol.with_map(|m| m.devices.insert(devid, cur));
            self.sp.chunks_dirty = true;
            self.flush()?;
            return Err(e);
        }
        Ok(want)
    }

    /// Move or empty every chunk with a copy past block `limit` of `devid`.
    fn empty_device_tail(&mut self, vol: &Arc<Volume>, devid: u32, limit: u64) -> Result<()> {
        loop {
            let map = vol.map();
            let Some((&g, c)) = map.chunks.iter().find(|(_, c)| {
                let len = c.stripe_len();
                c.stripes
                    .iter()
                    .any(|s| s.devid == devid && s.phys + len > limit)
            }) else {
                return Ok(());
            };
            let c = c.clone();
            let empty = self
                .sp
                .alloc
                .groups()
                .get(g as usize)
                .is_some_and(|grp| grp.free_blocks + 2 == grp.block_count);
            if g != 0 && empty {
                self.drop_chunk(g)?;
                continue;
            }
            match self.move_stripes_inside(vol, g, &c, devid, limit) {
                Err(Error::NoSpace) if g != 0 => {
                    // Nowhere to put it whole: empty it into the others.
                    let mut cut = RangeSet::default();
                    cut.insert(g * GROUP_BLOCKS, c.blocks);
                    self.evacuate(&cut)?;
                    let grp = &self.sp.alloc.groups()[g as usize];
                    if grp.free_blocks + 2 != grp.block_count {
                        return Err(Error::NoSpace);
                    }
                    self.drop_chunk(g)?;
                }
                r => r?,
            }
        }
    }

    /// Give chunk `g` new places on `devid`, inside `limit`, for the copies
    /// it has past it (or, on a volume of several devices, wherever its
    /// profile allows off `devid`).
    fn move_stripes_inside(
        &mut self,
        vol: &Arc<Volume>,
        g: u64,
        old: &Chunk,
        devid: u32,
        limit: u64,
    ) -> Result<()> {
        let len = old.stripe_len();
        let mut taken = vol.held();
        let map = vol.map();
        let mut stripes = old.stripes.clone();
        let mut room = true;
        for s in stripes.iter_mut() {
            if s.devid != devid || s.phys + len <= limit {
                continue;
            }
            match map
                .free_extents(devid, &taken)
                .into_iter()
                .find(|e| e.1 >= len)
            {
                Some((at, _)) => {
                    s.phys = at;
                    taken.push((devid, at, len));
                }
                None => room = false,
            }
        }
        if room {
            let new = Chunk {
                stripes,
                ..old.clone()
            };
            return self.switch_chunk(g, old, new);
        }
        if map.devices.len() > 1 {
            // Off this device altogether, if the others have the room.
            self.sp.avoid.push(devid);
            let r = self.relocate(g, old.profile);
            self.sp.avoid.retain(|&d| d != devid);
            return r;
        }
        Err(Error::NoSpace)
    }

    /// Empty the logical blocks of `cut`: move every node and data block
    /// there elsewhere, and commit. Group headers stay.
    fn evacuate(&mut self, cut: &RangeSet) -> Result<()> {
        // Nothing of this transaction may be rewritten in place in there.
        self.flush()?;
        for (s, l) in cut.iter() {
            self.sp.alloc.fence(s, s + l);
        }
        let r = self.evacuate_fenced(cut);
        let r = r.and_then(|_| self.flush());
        self.sp.alloc.unfence();
        r
    }

    fn evacuate_fenced(&mut self, cut: &RangeSet) -> Result<()> {
        self.zcache = None;
        let cur = self.subvol;
        let mut moves = DataMoves::default();
        let ids: Vec<u64> = self.root_items()?.into_iter().map(|(id, _)| id).collect();
        let moving = |b: u64| cut.contains(b);
        let r = (|| {
            for id in ids {
                self.enter(id)?;
                self.move_extents(cut, &mut moves)?;
                self.st.relocate(&mut self.sp, &mut self.fs_root, &moving)?;
            }
            Ok(())
        })();
        // The copies keep only the references moved to them.
        for (s, l) in core::mem::take(&mut moves.allocated) {
            self.sp_decref(s, l)?;
        }
        r?;
        self.enter(cur)?;
        self.st
            .relocate(&mut self.sp, &mut self.refs_root, &moving)?;
        self.st
            .relocate(&mut self.sp, &mut self.csum_root, &moving)?;
        self.st
            .relocate(&mut self.sp, &mut self.root_tree, &moving)?;
        Ok(())
    }

    /// Point every file extent of the current subvolume that has blocks in
    /// `cut` at copies of them outside it.
    fn move_extents(&mut self, cut: &RangeSet, moves: &mut DataMoves) -> Result<()> {
        let mut from = Key::MIN;
        loop {
            let batch = self.st.range(&self.fs_root, &from, &Key::MAX, 256)?;
            let Some(&(last, _)) = batch.last() else {
                return Ok(());
            };
            for (k, d) in batch {
                if k.ty != EXTENT_DATA {
                    continue;
                }
                match FileExtent::decode(&d)? {
                    FileExtent::Inline(_) => {}
                    FileExtent::Comp(c) => {
                        if !touches(cut, c.disk, c.disk_blocks) {
                            continue;
                        }
                        let new = self.move_comp(c.disk, c.disk_blocks, moves)?;
                        crate::btree::Space::incref(&mut self.sp, new, c.disk_blocks)?;
                        self.update(k, FileExtent::Comp(Comp { disk: new, ..c }).encode())?;
                        self.sp_decref(c.disk, c.disk_blocks)?;
                    }
                    e @ (FileExtent::Reg { .. } | FileExtent::Prealloc { .. }) => {
                        let (disk, blocks, prealloc) = match e {
                            FileExtent::Reg { disk, blocks } => (disk, blocks, false),
                            FileExtent::Prealloc { disk, blocks } => (disk, blocks, true),
                            _ => unreachable!(),
                        };
                        if !touches(cut, disk, blocks) {
                            continue;
                        }
                        let pieces = self.move_run(disk, blocks, cut, moves)?;
                        let mut off = 0;
                        for (i, &(old, n, new)) in pieces.iter().enumerate() {
                            let key = Key::new(k.objectid, EXTENT_DATA, k.offset + off * BS);
                            if new != old {
                                crate::btree::Space::incref(&mut self.sp, new, n)?;
                            }
                            let item = Ext::item(new, n, prealloc);
                            if i == 0 {
                                self.update(key, item)?;
                            } else {
                                self.insert(key, item)?;
                            }
                            if new != old {
                                self.sp_decref(old, n)?;
                            }
                            off += n;
                        }
                    }
                }
            }
            match next_key(last) {
                Some(k) => from = k,
                None => return Ok(()),
            }
        }
    }

    fn update(&mut self, key: Key, v: Vec<u8>) -> Result<()> {
        self.st.update(&mut self.sp, &mut self.fs_root, key, v)
    }

    /// Where data blocks `disk..disk+blocks` live once those in `cut` move:
    /// `(old start, length, new start)` pieces in order, merged where both
    /// sides run on. Copies are made the first time a block is asked for.
    fn move_run(
        &mut self,
        disk: u64,
        blocks: u64,
        cut: &RangeSet,
        moves: &mut DataMoves,
    ) -> Result<Vec<(u64, u64, u64)>> {
        let end = disk + blocks;
        let mut out: Vec<(u64, u64, u64)> = Vec::new();
        let mut pos = disk;
        while pos < end {
            let piece = if !cut.contains(pos) {
                let mut e = pos + 1;
                while e < end && !cut.contains(e) {
                    e += 1;
                }
                (pos, e - pos, pos)
            } else if let Some((&s, &(l, ns))) = moves
                .runs
                .range(..=pos)
                .next_back()
                .filter(|(&s, &(l, _))| pos < s + l)
            {
                let n = (s + l).min(end) - pos;
                (pos, n, ns + (pos - s))
            } else {
                let mut e = pos + 1;
                while e < end && cut.contains(e) {
                    e += 1;
                }
                let next = moves.runs.range(pos..).next().map_or(u64::MAX, |(&s, _)| s);
                let want = e.min(next) - pos;
                let goal = out.last().map(|&(_, n, ns)| ns + n);
                let (nb, got) = self.sp.alloc_data(goal, want).ok_or(Error::NoSpace)?;
                moves.allocated.push((nb, got));
                self.copy_blocks(pos, nb, got)?;
                moves.runs.insert(pos, (got, nb));
                (pos, got, nb)
            };
            match out.last_mut() {
                Some(l) if l.0 + l.1 == piece.0 && l.2 + l.1 == piece.2 => l.1 += piece.1,
                _ => out.push(piece),
            }
            pos += piece.1;
        }
        Ok(out)
    }

    /// The new place of the compressed extent at `disk`, copied whole.
    fn move_comp(&mut self, disk: u64, blocks: u64, moves: &mut DataMoves) -> Result<u64> {
        if let Some(&n) = moves.comp.get(&disk) {
            return Ok(n);
        }
        let nb = self.sp.alloc_data_run(None, blocks).ok_or(Error::NoSpace)?;
        moves.allocated.push((nb, blocks));
        self.copy_blocks(disk, nb, blocks)?;
        moves.comp.insert(disk, nb);
        Ok(nb)
    }

    /// Copy disk blocks `from..from+n` to `to..to+n` with their checksums.
    fn copy_blocks(&mut self, from: u64, to: u64, n: u64) -> Result<()> {
        let sums = self.csums(from, n)?;
        let mut done = 0;
        let mut buf = Vec::new();
        while done < n {
            let k = (n - done).min(WRITE_CHUNK);
            buf.resize((k * BS) as usize, 0);
            self.raw.read_at((from + done) * BS, &mut buf)?;
            self.raw.write_at((to + done) * BS, &buf)?;
            done += k;
        }
        for (i, c) in sums.into_iter().enumerate() {
            if c.is_some() {
                self.sp.csums.insert(to + i as u64, c);
            }
        }
        Ok(())
    }
}
