//! Checksums of the allocator bitmaps.
//!
//! A bitmap block is 4 KiB of bits with no room for a checksum, so the
//! CRC32C of every group's bitmap lives in a table in group 0 that the
//! superblock points at ([`FEAT_BITMAP_CSUM`]). The table is written through
//! the journal with the bitmaps and the superblock, so the three always
//! agree after a commit or a replay.
//!
//! Mounting checks every bitmap against the table. One that does not match
//! (or a table block that is itself damaged) is taken from another copy on
//! a mirrored volume, or else rebuilt from the trees: a block is in use
//! exactly when a group header, the journal, the chunk map, the table, a
//! tree node or a file extent holds it, which is what fsck checks. A
//! filesystem without the table gets one with its next commit.

use super::*;

impl CowFs {
    /// Check the bitmaps just read (`crcs`, `None` for one that did not
    /// decode) against the table, repairing those that do not match.
    pub(super) fn load_bitmap_sums(&mut self, crcs: Vec<Option<u32>>) -> Result<()> {
        let n = self.sp.alloc.groups().len();
        let Some((at, len)) = self.sb.bitmap_sums else {
            // Without the feature the bitmaps loaded strictly.
            self.bitmap_sums = crcs.iter().map(|c| c.unwrap_or(0)).collect();
            self.sums_pending = !self.read_only;
            return Ok(());
        };
        let mut want: Vec<Option<u32>> = vec![None; n];
        let mut buf = vec![0u8; BS as usize];
        for k in 0..len as usize {
            let b = at + k as u64;
            let mut sums = self.read_checked(b, &mut buf, decode_sums_block);
            if sums.is_none() {
                warn!("eclipsefs: bitmap checksum block {} is damaged", k);
                self.sums_dirty.insert(k);
                sums = Some(Vec::new());
            }
            for (i, c) in sums.unwrap_or_default().into_iter().enumerate() {
                if let Some(w) = want.get_mut(k * SUMS_PER_BLOCK + i) {
                    *w = Some(c);
                }
            }
        }
        self.bitmap_sums = vec![0; n];
        let mut suspect = Vec::new();
        for i in 0..n {
            if self.sp.alloc.groups()[i].is_hole() {
                continue;
            }
            match (crcs.get(i).copied().flatten(), want[i]) {
                (Some(c), Some(w)) if c == w => self.bitmap_sums[i] = c,
                (_, Some(w)) if self.bitmap_from_copy(i, w, &mut buf) => {
                    warn!("eclipsefs: bitmap of group {} repaired from a copy", i);
                    self.bitmap_sums[i] = w;
                }
                _ => suspect.push(i),
            }
        }
        if !suspect.is_empty() {
            warn!(
                "eclipsefs: rebuilding the bitmaps of groups {:?} from the trees",
                suspect
            );
            self.rebuild_groups(&suspect)?;
        }
        self.sums_pending = !self.read_only && !self.sums_dirty.is_empty();
        Ok(())
    }

    /// Block `b` through the journal, decoded by `decode`; on a mirrored
    /// volume, the first copy that decodes when the read one does not
    /// (unless the journal holds the block).
    fn read_checked<T>(
        &self,
        b: u64,
        buf: &mut [u8],
        decode: impl Fn(&[u8]) -> Option<T>,
    ) -> Option<T> {
        if let Some(v) = self.dev.read_at(b * BS, buf).ok().and_then(|_| decode(buf)) {
            return Some(v);
        }
        let vol = self.vol.as_ref().filter(|_| !self.dev.contains(b))?;
        (0..vol.copies(b))
            .find_map(|k| vol.read_copy(b * BS, buf, k).ok().and_then(|_| decode(buf)))
    }

    /// Replace group `i`'s bitmap with a copy whose checksum is `want`, if
    /// one decodes; it is written back to every copy with the next commit.
    fn bitmap_from_copy(&mut self, i: usize, want: u32, buf: &mut [u8]) -> bool {
        if self.vol.is_none() {
            return false;
        }
        let (count, kind) = {
            let g = &self.sp.alloc.groups()[i];
            (g.block_count, g.kind)
        };
        let b = i as u64 * GROUP_BLOCKS + 1;
        let found = self.read_checked(b, buf, |bytes| {
            (crc32c(bytes) == want)
                .then(|| AllocationGroup::from_bytes(i as u32, count, bytes).ok())
                .flatten()
        });
        let Some(mut g) = found else {
            return false;
        };
        g.kind = kind;
        g.mark_dirty();
        self.sp.alloc.groups_mut()[i] = g;
        true
    }

    /// Rebuild the bitmaps of groups `which` from what holds their blocks.
    fn rebuild_groups(&mut self, which: &[usize]) -> Result<()> {
        let owned = self.owned_blocks()?;
        for &i in which {
            let g = self.group_from(i, &owned);
            self.set_rebuilt_group(i, g);
        }
        Ok(())
    }

    /// Group `i`'s bitmap as `owned` (every block held, see
    /// [`owned_blocks`](Self::owned_blocks)) says it should be.
    pub(super) fn group_from(&self, i: usize, owned: &RangeSet) -> AllocationGroup {
        let (count, kind, row, tier) = {
            let g = &self.sp.alloc.groups()[i];
            (g.block_count, g.kind, g.row(), g.tier)
        };
        let start = i as u64 * GROUP_BLOCKS;
        let mut g = AllocationGroup::new(i as u32, count);
        g.kind = kind;
        // Placement by tier stays as it was.
        g.tier = tier;
        // A parity chunk keeps handing out whole rows only.
        g.set_row(row);
        for (s, l) in owned.within(start, start + count) {
            g.mark_used(s - start, l);
        }
        g
    }

    /// Put a rebuilt bitmap in place of group `i`'s, to be written (with
    /// its checksum) by the next commit.
    pub(super) fn set_rebuilt_group(&mut self, i: usize, mut g: AllocationGroup) {
        g.mark_dirty();
        self.bitmap_sums[i] = crc32c(&g.to_bytes());
        self.sums_dirty.insert(i / SUMS_PER_BLOCK);
        self.sp.alloc.groups_mut()[i] = g;
    }

    /// Every block something committed holds, but the group headers.
    pub(super) fn owned_blocks(&mut self) -> Result<RangeSet> {
        let mut owned = RangeSet::default();
        owned.insert(self.journal.start, self.journal.blocks);
        if let Some(vs) = &self.sb.vol {
            owned.insert(vs.chunk_map, (vs.chunk_map_len as u64).div_ceil(BS));
        }
        if let Some((at, n)) = self.sb.bitmap_sums {
            owned.insert(at, n as u64);
        }
        let mut stack = vec![(
            self.root_tree.bytenr,
            self.root_tree.level,
            Some(self.root_tree.gen),
        )];
        for (k, d) in self.st.range(
            &self.root_tree,
            &Key::new(0, ROOT_ITEM, 0),
            &Key::MAX,
            usize::MAX,
        )? {
            if k.ty == ROOT_ITEM {
                let r = RootItem::decode(&d)?.root;
                stack.push((r.bytenr, r.level, Some(r.gen)));
            }
        }
        let mut nodes = RangeSet::default();
        while let Some((b, level, gen)) = stack.pop() {
            if nodes.contains(b) {
                continue;
            }
            nodes.insert(b, 1);
            match &self.st.node(b, level, gen)?.body {
                Body::Internal(ptrs) => {
                    stack.extend(ptrs.iter().map(|p| (p.bytenr, level - 1, Some(p.gen))));
                }
                Body::Leaf(items) => {
                    for (k, d) in items {
                        if let Some((s, l)) = extent_of(k, d) {
                            owned.insert(s, l);
                        }
                    }
                }
            }
        }
        for (s, l) in nodes.iter() {
            owned.insert(s, l);
        }
        Ok(owned)
    }

    /// Size the table for the current groups, moving it when that changes
    /// (commit, before the deferred frees are released: the new table never
    /// takes blocks the last commit still uses).
    pub(super) fn place_bitmap_sums(&mut self) -> Result<()> {
        let n = self.sp.alloc.groups().len();
        self.bitmap_sums.resize(n, 0);
        for (s, g) in self.bitmap_sums.iter_mut().zip(self.sp.alloc.groups()) {
            if g.is_hole() {
                *s = 0;
            }
        }
        let need = n.div_ceil(SUMS_PER_BLOCK).max(1) as u64;
        if let Some((_, len)) = self.sb.bitmap_sums {
            if len as u64 == need {
                return Ok(());
            }
        }
        if let Some((at, len)) = self.sb.bitmap_sums.take() {
            self.sp.alloc.free(at, len as u64)?;
        }
        match self.sp.alloc.groups_mut()[0].allocate_run(2, need) {
            Some(at) => {
                self.sb.bitmap_sums = Some((at, need as u32));
                self.sums_dirty.extend(0..need as usize);
            }
            None => warn!("eclipsefs: no room in group 0 for the bitmap checksums"),
        }
        Ok(())
    }

    /// The table blocks that changed, through the journal.
    pub(super) fn write_bitmap_sums(&mut self) -> Result<()> {
        self.sums_pending = false;
        let dirty = core::mem::take(&mut self.sums_dirty);
        let Some((at, len)) = self.sb.bitmap_sums else {
            return Ok(());
        };
        for k in dirty.into_iter().filter(|&k| k < len as usize) {
            let from = (k * SUMS_PER_BLOCK).min(self.bitmap_sums.len());
            let to = (from + SUMS_PER_BLOCK).min(self.bitmap_sums.len());
            let b = encode_sums_block(&self.bitmap_sums[from..to]);
            self.dev.write_at((at + k as u64) * BS, &b)?;
        }
        Ok(())
    }

    /// The table on disk agrees with every group's bitmap (fsck).
    pub(super) fn check_bitmap_sums(&mut self) -> Result<()> {
        let Some((at, len)) = self.sb.bitmap_sums else {
            return Ok(());
        };
        let mut buf = vec![0u8; BS as usize];
        let mut table = Vec::new();
        for k in 0..len as u64 {
            self.dev.read_at((at + k) * BS, &mut buf)?;
            table.extend(decode_sums_block(&buf).ok_or(Error::Corrupt("fsck: bitmap checksums"))?);
        }
        // What is on disk (a read-only mount repairs in memory only) and
        // what the allocator holds.
        for (i, g) in self.sp.alloc.groups().iter().enumerate() {
            if g.is_hole() {
                continue;
            }
            self.dev.read_at(g.bitmap_block() * BS, &mut buf)?;
            let want = table.get(i).copied();
            if want != Some(crc32c(&buf)) || want != Some(crc32c(&g.to_bytes())) {
                return Err(Error::Corrupt("fsck: bitmap checksum"));
            }
        }
        Ok(())
    }
}
