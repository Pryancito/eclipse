//! Repair (`btrfs check --repair`, `xfs_repair`) of a mounted filesystem.
//!
//! [`CowFs::repair`] makes the filesystem pass [`CowFs::check`] again,
//! keeping every file it can still read. It runs on a writable mount and
//! changes things only through the trees and the commit, so a crash in the
//! middle leaves the state of some commit: the old one, the repaired one, or
//! (on a large repair that had to commit on the way) one in between, which
//! is consistent and which a second run finishes.
//!
//! In order:
//!
//! 1. **Scrub.** Bad copies of tree nodes and data blocks on a mirrored
//!    volume are rewritten from a good copy. A tree node with no good copy
//!    stops the repair there: whatever hangs below it is unknown, so freeing
//!    "unowned" blocks could free live ones. Data blocks with no good copy
//!    are only reported (they read as EIO, as before).
//! 2. **Space.** Reference counts and allocation bitmaps are rebuilt from
//!    what the trees actually point at: a block in use is exactly one held by
//!    a group header, the journal, the chunk map, the bitmap checksum table,
//!    a tree node or a file extent, and its count is the number of pointers
//!    to it. Two extents claiming the same blocks become a shared (reflinked)
//!    range rather than a double allocation. A tree node that shares a block
//!    with anything else cannot be fixed this way and is reported. The
//!    fsync log area (the root tree's `LOG_TREE` extent) counts as held like
//!    any other extent and is never freed or written by repair; on parity
//!    (RAID5/6) chunks a rebuilt bitmap keeps its row size, so new data still
//!    never lands in a row holding committed data, and on a tiered volume
//!    it keeps its tier. Every write goes through a normal commit (and so
//!    through the volume layer).
//! 3. **Names**, subvolume by subvolume. Items of inodes that do not exist
//!    (and items that do not decode, overlapping or misplaced extents,
//!    extents of special files) are deleted, their blocks released.
//!    `DIR_INDEX` is the truth for directory entries: entries naming a
//!    missing inode, a second name of a directory or a duplicate name are
//!    dropped, and `DIR_ITEM` and `INODE_REF` are rebuilt to match it.
//!    Inodes left without a name, and directories cut off from the root by a
//!    cycle, are linked into `lost+found` in the subvolume's root directory as
//!    `#<inode>` (the directory is made when needed, as `e2fsck` does). Link
//!    counts, `..`, `nbytes`, directory sizes, the inode count, the next
//!    inode number and the quota counter are then recomputed.
//! 4. **Checksums.** Checksums of blocks no file extent references are
//!    dropped, and a referenced block of a file that keeps checksums but has
//!    none gets one computed from what is on disk (repair cannot tell whether
//!    such a block is still what was written).
//!
//! Space is rebuilt again after the names (deleted items free blocks), and
//! the result is checked: [`RepairReport::still_damaged`] holds what
//! [`check`](CowFs::check) still finds, if anything.

use super::*;
use crate::fs::RepairReport;

/// Name of the directory orphans are linked into.
const LOST_FOUND: &[u8] = b"lost+found";

/// Counts per block range: start -> (length, count >= 1), merged when
/// neighbours have the same count.
#[derive(Default)]
struct Counts(BTreeMap<u64, (u64, u32)>);

impl Counts {
    /// Split the entry holding `at` so one starts there.
    fn cut(&mut self, at: u64) {
        if let Some((&s, &(l, c))) = self.0.range(..at).next_back() {
            if s + l > at {
                self.0.insert(s, (at - s, c));
                self.0.insert(at, (s + l - at, c));
            }
        }
    }

    /// One more holder of `start..start+len`.
    fn add(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let end = start + len;
        self.cut(start);
        self.cut(end);
        let inside: Vec<(u64, u64)> = self
            .0
            .range(start..end)
            .map(|(&s, &(l, _))| (s, l))
            .collect();
        let mut pos = start;
        for (s, l) in inside {
            if s > pos {
                self.0.insert(pos, (s - pos, 1));
            }
            if let Some(e) = self.0.get_mut(&s) {
                e.1 += 1;
            }
            pos = s + l;
        }
        if pos < end {
            self.0.insert(pos, (end - pos, 1));
        }
        self.merge(start, end);
    }

    fn merge(&mut self, start: u64, end: u64) {
        let keys: Vec<u64> = self
            .0
            .range(..=end)
            .rev()
            .take_while(|(&s, &(l, _))| s + l >= start)
            .map(|(&s, _)| s)
            .collect();
        for s in keys.into_iter().rev() {
            let Some(&(l, c)) = self.0.get(&s) else {
                continue;
            };
            if let Some(&(nl, nc)) = self.0.get(&(s + l)) {
                if nc == c {
                    self.0.remove(&(s + l));
                    self.0.insert(s, (l + nl, c));
                }
            }
        }
    }

    fn held(&self) -> RangeSet {
        let mut r = RangeSet::default();
        for (&s, &(l, _)) in &self.0 {
            r.insert(s, l);
        }
        r
    }

    /// The reference map these counts make (ranges held twice or more).
    fn refs(&self) -> Result<RefMap> {
        let mut out: Vec<(u64, u64, u32)> = Vec::new();
        for (&s, &(l, c)) in &self.0 {
            if c < 2 {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.0 + last.1 == s && last.2 == c => last.1 += l,
                _ => out.push((s, l, c)),
            }
        }
        RefMap::from_entries(out)
    }
}

/// A reference map's entries with neighbours of the same count joined.
fn merged(m: &RefMap) -> Vec<(u64, u64, u32)> {
    let mut out: Vec<(u64, u64, u32)> = Vec::new();
    for (s, l, c) in m.entries() {
        match out.last_mut() {
            Some(last) if last.0 + last.1 == s && last.2 == c => last.1 += l,
            _ => out.push((s, l, c)),
        }
    }
    out
}

/// Whether two range sets share a block.
fn overlap(a: &RangeSet, b: &RangeSet) -> bool {
    a.iter().any(|(s, l)| b.within(s, s + l).next().is_some())
}

/// Whether a file extent may hold `s..s+l` (no tree node, fixed area or
/// past the end, see [`CowFs::repair_space`]).
fn claimable(poison: &RangeSet, total: u64, s: u64, l: u64) -> bool {
    s.checked_add(l)
        .is_some_and(|e| e <= total && poison.within(s, e).next().is_none())
}

/// Whether `name` can be an entry of `dir`: what create() accepts in a
/// plain directory, ciphertext of at least a block in an encrypted one.
fn valid_name(s: &Scan, dir: u64, name: &[u8]) -> bool {
    if s.contexts.contains(&dir) {
        name.len() >= 16
    } else {
        core::str::from_utf8(name).is_ok_and(|n| check_name(n).is_ok())
    }
}

/// One directory entry, as `DIR_INDEX` holds it.
#[derive(Clone)]
struct Entry {
    dir: u64,
    index: u64,
    name: Vec<u8>,
    ino: u64,
    kind: FileKind,
}

/// What one pass over a subvolume's items found.
#[derive(Default)]
struct Scan {
    inodes: BTreeMap<u64, InodeItem>,
    entries: Vec<Entry>,
    /// `DIR_ITEM`s as found: (directory, hash) -> names.
    items: BTreeMap<(u64, u64), Vec<DirEnt>>,
    /// `INODE_REF`s as found: (inode, key offset) -> value.
    refs: BTreeMap<(u64, u64), Vec<u8>>,
    /// Objects with items but no (usable) inode.
    homeless: BTreeSet<u64>,
    /// Single items to delete.
    bad: Vec<Key>,
    /// Bytes of file data per inode (extents that stay).
    bytes: BTreeMap<u64, u64>,
    /// Inodes with an encryption context.
    contexts: BTreeSet<u64>,
    max_ino: u64,
}

impl CowFs {
    /// See [`EclipseFS::repair`](crate::EclipseFS::repair).
    pub fn repair(&mut self) -> Result<RepairReport> {
        self.writable()?;
        // 1. Copies, and whether every tree node can be read at all.
        let mut r = RepairReport {
            scrub: self.scrub()?,
            ..Default::default()
        };
        if !r.scrub.bad_tree_nodes.is_empty() {
            warn!(
                "eclipsefs: repair: tree nodes {:?} cannot be read",
                r.scrub.bad_tree_nodes
            );
            r.still_damaged = Some("tree nodes cannot be read");
            return Ok(r);
        }
        // 2. Space, from the committed trees.
        let poison = match self.repair_space(&mut r)? {
            Ok(poison) => poison,
            Err(why) => {
                r.still_damaged = Some(why);
                return Ok(r);
            }
        };
        // 3. Names. Copying a leaf shared with a snapshot references every
        // extent in it, damaged ones included.
        self.sp.lenient = true;
        let names = self.repair_names(&mut r, &poison);
        self.sp.lenient = false;
        names?;
        // Deleted items released blocks the old counts knew nothing about.
        if let Err(why) = self.repair_space(&mut r)? {
            r.still_damaged = Some(why);
            return Ok(r);
        }
        // 4. Checksums.
        self.repair_csums(&mut r)?;
        self.flush()?;
        self.raw.sync()?;
        r.still_damaged = match self.check() {
            Ok(_) => None,
            Err(Error::Corrupt(why)) => Some(why),
            Err(e) => return Err(e),
        };
        if r.changed() {
            warn!("eclipsefs: repair: {:?}", r);
        }
        Ok(r)
    }

    /// Step 3 on every subvolume (but those being received), committed.
    fn repair_names(&mut self, r: &mut RepairReport, poison: &RangeSet) -> Result<()> {
        let home = self.subvol;
        for (id, item) in self.root_items()? {
            if item.flags & ROOT_FLAG_RECEIVING != 0 {
                continue;
            }
            self.enter(id)?;
            self.repair_subvol(r, poison)?;
            if self.st.dirty_count() >= DIRTY_NODES_LIMIT {
                self.flush()?;
            }
        }
        self.enter(home)?;
        self.flush()
    }

    // -----------------------------------------------------------------------
    // Space
    // -----------------------------------------------------------------------

    /// Rebuild reference counts and bitmaps from the committed trees
    /// (commits first). Returns the blocks file extents may not claim
    /// (another extent's claim on them is not counted, and the extent is
    /// to go), or why not, with nothing changed: a tree node or the fsync
    /// log area shares its blocks with something else.
    fn repair_space(
        &mut self,
        r: &mut RepairReport,
    ) -> Result<core::result::Result<RangeSet, &'static str>> {
        self.flush()?;
        let total = self.sb.total_blocks;
        let mut counts = Counts::default();
        let mut fixed = RangeSet::default();
        for g in self.sp.alloc.groups().iter().filter(|g| !g.is_hole()) {
            fixed.insert(g.start_block, 2);
        }
        fixed.insert(self.journal.start, self.journal.blocks);
        if let Some((at, n)) = self.sb.bitmap_sums {
            fixed.insert(at, n as u64);
        }
        if let Some(vs) = &self.sb.vol {
            fixed.insert(vs.chunk_map, (vs.chunk_map_len as u64).div_ceil(BS));
        }
        for (s, l) in fixed.iter() {
            counts.add(s, l);
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
                let root = RootItem::decode(&d)?.root;
                stack.push((root.bytenr, root.level, Some(root.gen)));
            }
        }
        for &(b, _, _) in &stack {
            counts.add(b, 1);
        }
        // Tree nodes first: file extents may not claim their blocks.
        let roots = stack.clone();
        let mut nodes = RangeSet::default();
        while let Some((b, level, gen)) = stack.pop() {
            if nodes.contains(b) {
                continue;
            }
            nodes.insert(b, 1);
            if let Body::Internal(ptrs) = &self.st.node(b, level, gen)?.body {
                for p in ptrs {
                    counts.add(p.bytenr, 1);
                    stack.push((p.bytenr, level - 1, Some(p.gen)));
                }
            }
        }
        if overlap(&nodes, &fixed) || nodes.iter().any(|(s, l)| s + l > total) {
            warn!("eclipsefs: repair: blocks of tree nodes are claimed twice");
            return Ok(Err("a tree node shares its block"));
        }
        let mut poison = fixed.clone();
        for (s, l) in nodes.iter() {
            poison.insert(s, l);
        }
        let mut bad = RangeSet::default();
        let mut log_bad = false;
        let mut seen = RangeSet::default();
        stack = roots;
        while let Some((b, level, gen)) = stack.pop() {
            if seen.contains(b) {
                continue;
            }
            seen.insert(b, 1);
            match &self.st.node(b, level, gen)?.body {
                Body::Internal(ptrs) => {
                    stack.extend(ptrs.iter().map(|p| (p.bytenr, level - 1, Some(p.gen))));
                }
                Body::Leaf(items) => {
                    for (k, d) in items {
                        if let Some((s, l)) = extent_of(k, d) {
                            match s.checked_add(l) {
                                Some(e) if e <= total && poison.within(s, e).next().is_none() => {
                                    counts.add(s, l)
                                }
                                // The fsync log area is the root tree's:
                                // nothing would drop it, and freeing what
                                // it claims could hand out live blocks.
                                _ if k.objectid == LOG_TREE => log_bad = true,
                                // Past the end or on metadata: not counted,
                                // the extent goes with the names.
                                _ => bad.insert(s, l.min(total.saturating_sub(s))),
                            }
                        }
                    }
                }
            }
        }
        if log_bad {
            warn!("eclipsefs: repair: the fsync log area overlaps metadata");
            return Ok(Err("the fsync log area shares its blocks"));
        }
        for (s, l) in bad.iter() {
            poison.insert(s, l);
        }
        // Shared tree nodes are fine: several parents, one node.
        let refs = counts.refs()?;
        // Compared block by block: the same counts can be split into
        // entries differently.
        let (old, new) = (merged(&self.sp.refs), merged(&refs));
        if old != new {
            let a: BTreeSet<_> = old.iter().collect();
            let b: BTreeSet<_> = new.iter().collect();
            r.refcounts_fixed += a.symmetric_difference(&b).count() as u64;
            self.sp.refs = refs;
            self.sp.refs_dirty = true;
        }
        let held = counts.held();
        let groups: Vec<usize> = self
            .sp
            .alloc
            .groups()
            .iter()
            .enumerate()
            .filter(|(_, g)| !g.is_hole())
            .map(|(i, _)| i)
            .collect();
        for i in groups {
            let g = self.group_from(i, &held);
            let old = &self.sp.alloc.groups()[i];
            if g.to_bytes() != old.to_bytes() {
                r.bitmap_blocks_fixed += (2..g.block_count)
                    .filter(|&o| g.is_used(o) != old.is_used(o))
                    .count() as u64;
                debug!("eclipsefs: repair: bitmap of group {} rebuilt", i);
                self.set_rebuilt_group(i, g);
            }
        }
        self.flush()?;
        Ok(Ok(poison))
    }

    // -----------------------------------------------------------------------
    // Names
    // -----------------------------------------------------------------------

    /// Every item of the current subvolume, a batch at a time.
    fn scan_subvol(&mut self, poison: &RangeSet) -> Result<Scan> {
        let mut s = Scan::default();
        let total = self.sb.total_blocks;
        let mut last_end: BTreeMap<u64, u64> = BTreeMap::new();
        let mut lo = Key::MIN;
        loop {
            let items = self.st.range(&self.fs_root, &lo, &Key::MAX, PURGE_BATCH)?;
            let Some(last) = items.last().map(|(k, _)| *k) else {
                break;
            };
            for (k, d) in items {
                let o = k.objectid;
                if k.ty == INODE_ITEM {
                    match InodeItem::decode(&d) {
                        Ok(i) if o >= FIRST_INO && k.offset == 0 => {
                            s.max_ino = s.max_ino.max(o);
                            s.inodes.insert(o, i);
                        }
                        _ => {
                            s.homeless.insert(o);
                        }
                    }
                    continue;
                }
                // The inode item sorts first among an object's items.
                let Some(owner) = s.inodes.get(&o) else {
                    s.homeless.insert(o);
                    continue;
                };
                let kind = owner.kind();
                match k.ty {
                    INODE_REF => {
                        s.refs.insert((o, k.offset), d);
                    }
                    DIR_ITEM if kind == FileKind::Dir => match decode_dir_item(&d) {
                        Ok(list) => {
                            s.items.insert((o, k.offset), list);
                        }
                        Err(_) => s.bad.push(k),
                    },
                    DIR_INDEX if kind == FileKind::Dir => match decode_pair(&d) {
                        Ok(e) if !e.name.is_empty() && k.offset >= 2 => s.entries.push(Entry {
                            dir: o,
                            index: k.offset,
                            name: e.name,
                            ino: e.ino,
                            kind: e.kind,
                        }),
                        _ => s.bad.push(k),
                    },
                    EXTENT_DATA => {
                        let file = matches!(kind, FileKind::Regular | FileKind::Symlink)
                            && extent_of(&k, &d)
                                .is_none_or(|(s, l)| claimable(poison, total, s, l));
                        let end = last_end.entry(o).or_default();
                        let keep = match FileExtent::decode(&d) {
                            _ if !file => None,
                            Ok(FileExtent::Inline(v)) => {
                                (k.offset == 0 && v.len() <= INLINE_MAX && *end == 0)
                                    .then_some((1, v.len() as u64))
                            }
                            Ok(
                                FileExtent::Reg { blocks, .. }
                                | FileExtent::Prealloc { blocks, .. },
                            ) => Some((blocks, blocks * BS)),
                            Ok(FileExtent::Comp(c)) => Some((c.blocks, c.blocks * BS)),
                            Err(_) => None,
                        };
                        match keep {
                            Some((blocks, bytes))
                                if k.offset % BS == 0 && k.offset / BS >= *end && blocks > 0 =>
                            {
                                *end = k.offset / BS + blocks;
                                *s.bytes.entry(o).or_default() += bytes;
                            }
                            _ => s.bad.push(k),
                        }
                    }
                    XATTR_ITEM => match decode_xattrs(&d) {
                        Ok(list)
                            if !list.is_empty()
                                && list.iter().all(|(n, _)| name_hash(n) == k.offset) => {}
                        _ => s.bad.push(k),
                    },
                    ENC_CONTEXT if k.offset == 0 && self.sb.features & FEAT_ENCRYPT != 0 => {
                        s.contexts.insert(o);
                    }
                    _ => s.bad.push(k),
                }
            }
            match key_after(&last) {
                Some(next) => lo = next,
                None => break,
            }
        }
        // Objects whose items came before their inode (or with none).
        s.homeless.retain(|o| !s.inodes.contains_key(o));
        Ok(s)
    }

    /// Delete one item, releasing the blocks of an extent (unless they
    /// were not its to claim).
    fn drop_item(&mut self, k: Key, poison: &RangeSet) -> Result<()> {
        let d = self.delete(k)?;
        if let Some((s, l)) = extent_of(&k, &d) {
            if claimable(poison, self.sb.total_blocks, s, l) {
                self.sp_decref(s, l)?;
            }
        }
        Ok(())
    }

    /// Delete every item of object `o` of the current subvolume.
    fn drop_object(&mut self, o: u64, poison: &RangeSet) -> Result<u64> {
        let hi = Key::new(o, u8::MAX, u64::MAX);
        let mut lo = Key::new(o, 0, 0);
        let mut n = 0;
        loop {
            let keys: Vec<Key> = self
                .st
                .range(&self.fs_root, &lo, &hi, PURGE_BATCH)?
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            let Some(&last) = keys.last() else {
                break;
            };
            for k in keys {
                self.drop_item(k, poison)?;
                n += 1;
            }
            match key_after(&last) {
                Some(next) => lo = next,
                None => break,
            }
        }
        Ok(n)
    }

    /// Repair the names and inodes of the current subvolume.
    fn repair_subvol(&mut self, r: &mut RepairReport, poison: &RangeSet) -> Result<()> {
        let mut s = self.scan_subvol(poison)?;
        let root = self.fs_item.root_dir;
        if !s
            .inodes
            .get(&root)
            .is_some_and(|i| i.kind() == FileKind::Dir)
        {
            // Nothing to hang the rest on.
            r.unrepaired += 1;
            warn!(
                "eclipsefs: repair: subvolume {} has no root directory",
                self.subvol
            );
            return Ok(());
        }
        // Encryption flag and context go together: a context alone is
        // dropped, a flag alone cleared (below).
        for (&ino, i) in &s.inodes {
            if s.contexts.contains(&ino) && i.flags & FL_ENCRYPT as u64 == 0 {
                s.bad.push(Key::new(ino, ENC_CONTEXT, 0));
            }
        }
        // Inodes past `next_ino` are still inodes: make room for them.
        self.fs_item.next_ino = self.fs_item.next_ino.max(s.max_ino + 1);
        for k in core::mem::take(&mut s.bad) {
            self.drop_item(k, poison)?;
            r.items_removed += 1;
        }
        for o in core::mem::take(&mut s.homeless) {
            r.items_removed += self.drop_object(o, poison)?;
        }

        // Entries: DIR_INDEX decides, one name per directory, no duplicate.
        // Entries naming other subvolumes have to agree with the root tree's
        // backrefs.
        let mut backs: Vec<(u64, DirEnt)> = self
            .backrefs()?
            .into_iter()
            .filter(|(_, p, _)| *p == self.subvol)
            .map(|(id, _, e)| (id, e))
            .collect();
        let mut linked = BTreeSet::new();
        let mut keep: Vec<Entry> = Vec::new();
        let mut dropped: Vec<Entry> = Vec::new();
        let mut named_dirs = BTreeSet::new();
        let mut names_in: BTreeSet<(u64, Vec<u8>)> = BTreeSet::new();
        for mut e in core::mem::take(&mut s.entries) {
            let ok = valid_name(&s, e.dir, &e.name) && !names_in.contains(&(e.dir, e.name.clone()));
            let ok = ok
                && if is_link(e.ino) {
                    let (id, local) = split(e.ino);
                    let named = backs.iter().any(|(b, d)| {
                        *b == id && d.ino == e.dir && d.index == e.index && d.name == e.name
                    });
                    let ok = e.kind == FileKind::Dir
                        && local == FIRST_INO
                        && !linked.contains(&id)
                        && (named || self.unnamed_subvol(id)?);
                    if ok && !named {
                        // A subvolume nothing named: this entry is its name.
                        self.set_backref(id, self.subvol, e.dir, e.index, &e.name)?;
                        backs.push((
                            id,
                            DirEnt {
                                ino: e.dir,
                                index: e.index,
                                kind: FileKind::Dir,
                                name: e.name.clone(),
                            },
                        ));
                        r.entries_fixed += 1;
                    }
                    ok && linked.insert(id)
                } else {
                    match s.inodes.get(&e.ino) {
                        Some(_) if e.ino == root => false,
                        Some(i) if i.kind() == FileKind::Dir && named_dirs.contains(&e.ino) => {
                            false
                        }
                        Some(i) => {
                            if e.kind != i.kind() {
                                e.kind = i.kind();
                                self.upsert(
                                    Key::new(e.dir, DIR_INDEX, e.index),
                                    encode_pair(e.ino, 0, e.kind, &e.name),
                                )?;
                                r.entries_fixed += 1;
                            }
                            if e.kind == FileKind::Dir {
                                named_dirs.insert(e.ino);
                            }
                            true
                        }
                        None => false,
                    }
                };
            if ok {
                names_in.insert((e.dir, e.name.clone()));
                keep.push(e);
            } else {
                dropped.push(e);
            }
        }
        for e in &dropped {
            self.delete(Key::new(e.dir, DIR_INDEX, e.index))?;
            r.entries_removed += 1;
        }
        // Subvolumes named here whose entry is gone get it back.
        for (id, b) in backs {
            if linked.contains(&id) {
                continue;
            }
            let free = s
                .inodes
                .get(&b.ino)
                .is_some_and(|i| i.kind() == FileKind::Dir)
                && valid_name(&s, b.ino, &b.name)
                && b.index >= 2
                && !keep
                    .iter()
                    .any(|e| e.dir == b.ino && (e.index == b.index || e.name == b.name));
            if !free {
                r.unrepaired += 1;
                continue;
            }
            let target = handle(id, FIRST_INO);
            self.upsert(
                Key::new(b.ino, DIR_INDEX, b.index),
                encode_pair(target, 0, FileKind::Dir, &b.name),
            )?;
            linked.insert(id);
            keep.push(Entry {
                dir: b.ino,
                index: b.index,
                name: b.name,
                ino: target,
                kind: FileKind::Dir,
            });
            r.entries_fixed += 1;
        }
        self.sync_name_items(&s, &keep, r)?;

        // Orphans and cycles into lost+found.
        let mut lost = None;
        let orphans: Vec<u64> = s
            .inodes
            .keys()
            .copied()
            .filter(|&i| i != root && !keep.iter().any(|e| e.ino == i))
            .collect();
        for ino in orphans {
            if !self.adopt(&mut s, &mut keep, &mut lost, root, ino)? {
                r.unrepaired += 1;
                continue;
            }
            r.lost_found += 1;
        }
        loop {
            let cut = self.unreachable_dir(&s, &keep, root);
            let Some(ino) = cut else {
                break;
            };
            let pos = keep
                .iter()
                .position(|e| e.ino == ino)
                .ok_or(Error::Corrupt("repair: entry"))?;
            let e = keep.remove(pos);
            self.remove_entry(e.dir, &e.name)?;
            if !self.adopt(&mut s, &mut keep, &mut lost, root, ino)? {
                // Not even lost+found can take it: give up on this one
                // rather than loop.
                keep.push(e);
                r.unrepaired += 1;
                break;
            }
            r.lost_found += 1;
        }

        // Counts and back pointers of every inode.
        let mut names: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut subdirs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut dir_size: BTreeMap<u64, u64> = BTreeMap::new();
        let mut dir_next: BTreeMap<u64, u64> = BTreeMap::new();
        for e in &keep {
            *dir_size.entry(e.dir).or_default() += 2 * e.name.len() as u64;
            let n = dir_next.entry(e.dir).or_default();
            *n = (*n).max(e.index.saturating_add(1));
            if is_link(e.ino) {
                continue;
            }
            names.entry(e.ino).or_default().push(e.dir);
            if e.kind == FileKind::Dir {
                *subdirs.entry(e.dir).or_default() += 1;
            }
        }
        let mut referenced = 0u64;
        let inos: Vec<u64> = s.inodes.keys().copied().collect();
        for ino in inos {
            let mut i = self.inode_raw(ino)?;
            let before = i.clone();
            let dirs = names.get(&ino).map(|v| &v[..]).unwrap_or(&[]);
            if i.kind() == FileKind::Dir {
                i.nlink = 2 + subdirs.get(&ino).copied().unwrap_or(0);
                i.size = dir_size.get(&ino).copied().unwrap_or(0);
                i.dir_next = i.dir_next.max(dir_next.get(&ino).copied().unwrap_or(2));
                if let Some(&p) = dirs.first() {
                    i.parent = p;
                }
            } else {
                i.nlink = dirs.len() as u32;
                if !dirs.is_empty() && !dirs.contains(&i.parent) {
                    i.parent = dirs[0];
                }
            }
            if !s.contexts.contains(&ino) {
                i.flags &= !(FL_ENCRYPT as u64);
            }
            if i.kind() == FileKind::Symlink {
                i.size = i.size.min(MAX_SYMLINK_LEN as u64);
            }
            i.nbytes = s.bytes.get(&ino).copied().unwrap_or(0);
            referenced += i.nbytes;
            if i != before {
                self.put_inode(ino, i)?;
                r.inodes_fixed += 1;
            }
        }
        let count = s.inodes.len() as u64;
        let next = self.fs_item.next_ino.max(s.max_ino + 1);
        if self.fs_item.inode_count != count || self.fs_item.next_ino != next {
            self.fs_item.inode_count = count;
            self.fs_item.next_ino = next;
            r.inodes_fixed += 1;
        }
        if self.quota_on() {
            self.fs_item.referenced = referenced;
        }
        Ok(())
    }

    /// Whether subvolume `id` exists and nothing names it.
    fn unnamed_subvol(&mut self, id: u64) -> Result<bool> {
        if id < FIRST_SUBVOL || id == self.subvol {
            return Ok(false);
        }
        let exists = self
            .st
            .get(&self.root_tree, &Key::new(id, ROOT_ITEM, 0))?
            .is_some();
        Ok(exists && matches!(self.backref(id), Ok(None)))
    }

    /// The inode item as stored (no range check against `next_ino`, which
    /// may be what is wrong).
    fn inode_raw(&mut self, ino: u64) -> Result<InodeItem> {
        let d = self
            .get(Key::new(ino, INODE_ITEM, 0))?
            .ok_or(Error::NotFound)?;
        InodeItem::decode(&d)
    }

    /// Make `DIR_ITEM` and `INODE_REF` say what the entries `keep` say.
    fn sync_name_items(&mut self, s: &Scan, keep: &[Entry], r: &mut RepairReport) -> Result<()> {
        let mut items: BTreeMap<(u64, u64), Vec<DirEnt>> = BTreeMap::new();
        let mut refs: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
        for e in keep {
            items
                .entry((e.dir, name_hash(&e.name)))
                .or_default()
                .push(DirEnt {
                    ino: e.ino,
                    index: e.index,
                    kind: e.kind,
                    name: e.name.clone(),
                });
            if !is_link(e.ino) {
                refs.insert(
                    (e.ino, ref_hash(e.dir, e.index)),
                    encode_pair(e.dir, e.index, e.kind, &e.name),
                );
            }
        }
        let sorted = |l: &[DirEnt]| {
            let mut v: Vec<(u64, u64, u8, Vec<u8>)> = l
                .iter()
                .map(|e| (e.index, e.ino, e.kind.to_disk(), e.name.clone()))
                .collect();
            v.sort();
            v
        };
        for (&(dir, h), list) in &s.items {
            if !items.contains_key(&(dir, h)) && s.inodes.contains_key(&dir) {
                self.delete(Key::new(dir, DIR_ITEM, h))?;
                r.entries_fixed += 1;
            } else if let Some(want) = items.get(&(dir, h)) {
                if sorted(want) != sorted(list) {
                    self.upsert(Key::new(dir, DIR_ITEM, h), encode_dir_item(want))?;
                    r.entries_fixed += 1;
                }
            }
        }
        for (&(dir, h), want) in &items {
            if !s.items.contains_key(&(dir, h)) {
                let v = encode_dir_item(want);
                if v.len() > MAX_ITEM {
                    return Err(Error::NoSpace);
                }
                self.insert(Key::new(dir, DIR_ITEM, h), v)?;
                r.entries_fixed += 1;
            }
        }
        for (&(ino, off), v) in &s.refs {
            if refs.get(&(ino, off)) != Some(v) && s.inodes.contains_key(&ino) {
                if refs.contains_key(&(ino, off)) {
                    self.upsert(Key::new(ino, INODE_REF, off), refs[&(ino, off)].clone())?;
                } else {
                    self.delete(Key::new(ino, INODE_REF, off))?;
                }
                r.entries_fixed += 1;
            }
        }
        for (&(ino, off), v) in &refs {
            if !s.refs.contains_key(&(ino, off)) {
                self.insert(Key::new(ino, INODE_REF, off), v.clone())?;
                r.entries_fixed += 1;
            }
        }
        Ok(())
    }

    /// A directory with a name that cannot be reached from `root` (it sits
    /// in a cycle), if there is one.
    fn unreachable_dir(&self, s: &Scan, keep: &[Entry], root: u64) -> Option<u64> {
        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for e in keep.iter().filter(|e| !is_link(e.ino)) {
            children.entry(e.dir).or_default().push(e.ino);
        }
        let mut seen = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            if !seen.insert(d) {
                continue;
            }
            if let Some(c) = children.get(&d) {
                stack.extend(c.iter().copied());
            }
        }
        // Only directories with a name: one left without (no lost+found
        // could take it) is not in a cycle.
        s.inodes
            .iter()
            .find(|(i, it)| {
                it.kind() == FileKind::Dir && !seen.contains(i) && keep.iter().any(|e| e.ino == **i)
            })
            .map(|(&i, _)| i)
    }

    /// Link `ino` into `lost+found` as `#<ino>`. False when that cannot be
    /// done (the root directory is encrypted).
    fn adopt(
        &mut self,
        s: &mut Scan,
        keep: &mut Vec<Entry>,
        lost: &mut Option<u64>,
        root: u64,
        ino: u64,
    ) -> Result<bool> {
        let lf = match *lost {
            Some(lf) => lf,
            None => {
                let rd = self.inode_raw(root)?;
                if rd.flags & FL_ENCRYPT as u64 != 0 {
                    warn!("eclipsefs: repair: no lost+found in an encrypted root");
                    return Ok(false);
                }
                let found = keep.iter().find(|e| {
                    e.dir == root
                        && e.name == LOST_FOUND
                        && !is_link(e.ino)
                        && s.inodes.get(&e.ino).is_some_and(|i| {
                            i.kind() == FileKind::Dir && i.flags & FL_ENCRYPT as u64 == 0
                        })
                });
                let lf = match found {
                    Some(e) => e.ino,
                    None => self.make_lost_found(s, keep, root)?,
                };
                *lost = Some(lf);
                lf
            }
        };
        if ino == lf {
            return Ok(false);
        }
        let kind = s.inodes.get(&ino).ok_or(Error::NotFound)?.kind();
        let mut name = alloc::format!("#{}", ino).into_bytes();
        while keep.iter().any(|e| e.dir == lf && e.name == name) {
            name.push(b'_');
        }
        let index = self.add_entry(lf, &name, ino, kind)?;
        if let Some(i) = s.inodes.get_mut(&ino) {
            i.parent = lf;
        }
        keep.push(Entry {
            dir: lf,
            index,
            name,
            ino,
            kind,
        });
        Ok(true)
    }

    /// A new `lost+found` (or `lost+found_` ... if the name is taken) in
    /// `root`.
    fn make_lost_found(&mut self, s: &mut Scan, keep: &mut Vec<Entry>, root: u64) -> Result<u64> {
        let mut name = LOST_FOUND.to_vec();
        while keep.iter().any(|e| e.dir == root && e.name == name) {
            name.push(b'_');
        }
        let ino = self.fs_item.next_ino.max(s.max_ino + 1);
        let mut i = InodeItem::new(FileKind::Dir, 0o700, self.now(), self.st.gen);
        i.parent = root;
        self.insert(Key::new(ino, INODE_ITEM, 0), i.encode())?;
        self.fs_item.next_ino = ino + 1;
        s.max_ino = ino;
        s.inodes.insert(ino, i);
        let index = self.add_entry(root, &name, ino, FileKind::Dir)?;
        keep.push(Entry {
            dir: root,
            index,
            name,
            ino,
            kind: FileKind::Dir,
        });
        Ok(ino)
    }

    // -----------------------------------------------------------------------
    // Checksums
    // -----------------------------------------------------------------------

    /// Drop checksums of blocks no extent references, and compute the
    /// missing ones of blocks that need one.
    fn repair_csums(&mut self, r: &mut RepairReport) -> Result<()> {
        self.flush()?;
        let mut data = RangeSet::default();
        let mut need = RangeSet::default();
        let home = self.subvol;
        for (id, _) in self.root_items()? {
            self.enter(id)?;
            let mut nocow = false;
            let mut lo = Key::MIN;
            loop {
                let items = self.st.range(&self.fs_root, &lo, &Key::MAX, PURGE_BATCH)?;
                let Some(last) = items.last().map(|(k, _)| *k) else {
                    break;
                };
                for (k, d) in items {
                    if k.ty == INODE_ITEM {
                        nocow = InodeItem::decode(&d)?.flags & FL_NOCOW as u64 != 0;
                        continue;
                    }
                    let Some((s, l)) = extent_of(&k, &d) else {
                        continue;
                    };
                    if matches!(
                        FileExtent::decode(&d)?,
                        FileExtent::Reg { .. } | FileExtent::Comp(_)
                    ) {
                        data.insert(s, l);
                        if !nocow {
                            need.insert(s, l);
                        }
                    }
                }
                match key_after(&last) {
                    Some(next) => lo = next,
                    None => break,
                }
            }
        }
        self.enter(home)?;
        // Stale ones.
        let mut lo = Key::MIN;
        loop {
            let items = self
                .st
                .range(&self.csum_root, &lo, &Key::MAX, PURGE_BATCH)?;
            let Some(last) = items.last().map(|(k, _)| *k) else {
                break;
            };
            for (k, d) in items {
                if k.ty != CSUM_ITEM || k.objectid % CSUM_BUCKET != 0 {
                    self.st.delete(&mut self.sp, &mut self.csum_root, k)?;
                    r.csums_dropped += 1;
                    continue;
                }
                let bucket = k.objectid;
                let stale: Vec<usize> = match CsumBucket::decode(&d) {
                    Ok(cb) => {
                        cb.0.iter()
                            .enumerate()
                            .filter(|(i, c)| c.is_some() && !data.contains(bucket + *i as u64))
                            .map(|(i, _)| i)
                            .collect()
                    }
                    Err(_) => {
                        self.st.delete(&mut self.sp, &mut self.csum_root, k)?;
                        r.csums_dropped += 1;
                        continue;
                    }
                };
                if !stale.is_empty() {
                    r.csums_dropped += stale.len() as u64;
                    self.edit_csum_bucket(bucket, |cb| {
                        for i in stale {
                            cb.0[i] = None;
                        }
                    })?;
                }
            }
            match key_after(&last) {
                Some(next) => lo = next,
                None => break,
            }
        }
        // Missing ones.
        let mut buf = vec![0u8; BS as usize];
        for (start, len) in need.iter() {
            let mut b = start;
            while b < start + len {
                let n = (start + len - b).min(CSUM_BUCKET - b % CSUM_BUCKET);
                let have = self.csums(b, n)?;
                let mut fill = Vec::new();
                for (i, c) in have.into_iter().enumerate() {
                    if c.is_none() {
                        let at = b + i as u64;
                        self.raw.read_at(at * BS, &mut buf)?;
                        fill.push((at, crc32c(&buf)));
                    }
                }
                if !fill.is_empty() {
                    r.csums_added += fill.len() as u64;
                    let bucket = b / CSUM_BUCKET * CSUM_BUCKET;
                    self.edit_csum_bucket(bucket, |cb| {
                        for (at, c) in fill {
                            cb.0[(at - bucket) as usize] = Some(c);
                        }
                    })?;
                }
                b += n;
            }
        }
        Ok(())
    }
}
