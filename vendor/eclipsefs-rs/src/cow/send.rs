//! `send` and `receive` (btrfs send/receive): a read-only snapshot as a
//! stream of changes, in full or against an older snapshot the receiving
//! side already has.
//!
//! The stream works on inode numbers rather than paths: a received
//! subvolume keeps the sender's inode numbers, so an incremental stream
//! applies to a snapshot of the one an earlier stream made. It is
//! received into a subvolume nobody sees yet ([`ROOT_FLAG_RECEIVING`]),
//! checked like `fsck` would, and only then named and made read-only, so
//! a failed or interrupted receive leaves nothing behind.
//!
//! Format: a header (`EFSSEND1`, flags, the snapshot's identity, its
//! parent's, its name, its next inode number, CRC32C), then commands
//! `[type u8 | length u32 | CRC32C u32 | payload]`, the last one `END`.

use super::*;

const MAGIC: &[u8; 8] = b"EFSSEND1";
const FLAG_INCREMENTAL: u32 = 1;
/// Largest command payload a receiver accepts.
const MAX_CMD: usize = 1 << 20;
/// Data per `WRITE`.
const WRITE_MAX: u64 = 256 * 1024;

const CMD_END: u8 = 0;
const CMD_UNLINK: u8 = 1;
const CMD_DELETE: u8 = 2;
const CMD_CREATE: u8 = 3;
const CMD_LINK: u8 = 4;
const CMD_WRITE: u8 = 5;
const CMD_CLONE: u8 = 6;
const CMD_PUNCH: u8 = 7;
const CMD_PREALLOC: u8 = 8;
const CMD_XATTRS: u8 = 9;
const CMD_SETATTR: u8 = 10;

const BAD: Error = Error::Corrupt("send stream");

/// Where a sink takes the stream.
pub type SendSink<'a> = &'a mut dyn FnMut(&[u8]) -> Result<()>;
/// Where `receive` reads it from: fill the whole buffer or fail.
pub type ReceiveSource<'a> = &'a mut dyn FnMut(&mut [u8]) -> Result<()>;

/// The identity a stream names a subvolume by: what it was received from,
/// if anything (so streams chain across machines, as on btrfs), else its
/// own UUID.
fn identity(item: &RootItem) -> [u8; 16] {
    if item.received_uuid != [0; 16] {
        item.received_uuid
    } else {
        item.uuid
    }
}

struct Writer<'a> {
    out: SendSink<'a>,
    bytes: u64,
}

impl Writer<'_> {
    fn cmd(&mut self, ty: u8, payload: &[u8]) -> Result<()> {
        let mut h = [0u8; 9];
        h[0] = ty;
        h[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        h[5..9].copy_from_slice(&crc32c(payload).to_le_bytes());
        (self.out)(&h)?;
        (self.out)(payload)?;
        self.bytes += (h.len() + payload.len()) as u64;
        Ok(())
    }
}

/// Payload builder.
#[derive(Default)]
struct P(Vec<u8>);

impl P {
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    fn bytes(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
}

/// Payload reader; every short read is a corrupt stream.
struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(BAD);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.0)
    }
    fn ino(&mut self) -> Result<u64> {
        let ino = self.u64()?;
        if !(FIRST_INO..=INO_MASK).contains(&ino) {
            return Err(BAD);
        }
        Ok(ino)
    }
}

/// What changed for one inode between the two snapshots.
#[derive(Default)]
struct InodeChange {
    created: bool,
    deleted: bool,
    meta: bool,
    xattrs: bool,
    /// File blocks whose mapping changed.
    data: RangeSet,
}

/// What a stream summed up to (see [`CowFs::send`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendStats {
    pub bytes: u64,
    pub inodes: u64,
    /// File bytes sent as data and as clones of data sent before.
    pub data_bytes: u64,
    pub cloned_bytes: u64,
}

impl CowFs {
    /// Read-only subvolume whose root directory is `h`: its id and item.
    fn snapshot_item(&mut self, h: u64) -> Result<(u64, RootItem)> {
        let (sv, ino) = split(h);
        if ino != FIRST_INO {
            return Err(Error::Invalid);
        }
        self.enter(sv)?;
        if self.fs_item.flags & ROOT_FLAG_RDONLY == 0 {
            return Err(Error::Unsupported("send needs a read-only snapshot"));
        }
        Ok((sv, self.fs_item.clone()))
    }

    /// Write the read-only snapshot whose root directory is `snap` to
    /// `out`: everything in it, or with `parent` (another read-only
    /// snapshot, normally an older one of the same subvolume) only what
    /// changed since it.
    pub fn send(&mut self, snap: u64, parent: Option<u64>, out: SendSink) -> Result<SendStats> {
        if !self.read_only {
            // Trees as committed, so nothing changes underneath.
            self.flush()?;
        }
        let (sv, item) = self.snapshot_item(snap)?;
        let (parent_sv, parent) = match parent {
            Some(p) => {
                let (id, it) = self.snapshot_item(p)?;
                (id, Some(it))
            }
            None => (0, None),
        };
        let name = match self.backref(sv)? {
            Some((_, e)) => e.name,
            None => Vec::new(),
        };
        let mut w = Writer { out, bytes: 0 };
        let mut stats = SendStats::default();
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        let flags = if parent.is_some() {
            FLAG_INCREMENTAL
        } else {
            0
        };
        h.extend_from_slice(&flags.to_le_bytes());
        h.extend_from_slice(&identity(&item));
        h.extend_from_slice(&parent.as_ref().map_or([0; 16], identity));
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&name);
        h.extend_from_slice(&item.next_ino.to_le_bytes());
        h.extend_from_slice(&crc32c(&h).to_le_bytes());
        (w.out)(&h)?;
        w.bytes += h.len() as u64;

        // Every item that differs.
        let items = match &parent {
            Some(p) => self.st.diff(&p.root, &item.root)?,
            None => self
                .st
                .range(&item.root, &Key::MIN, &Key::MAX, usize::MAX)?
                .into_iter()
                .map(|(k, v)| (k, None, Some(v)))
                .collect(),
        };
        self.enter(sv)?;
        let mut changes: BTreeMap<u64, InodeChange> = BTreeMap::new();
        let mut unlinks = Vec::new();
        let mut links = Vec::new();
        for (k, old, new) in items {
            let c = changes.entry(k.objectid);
            match k.ty {
                // The stream carries plaintext, which needs the keys, and
                // a receiving side has no way to keep the policy.
                ENC_CONTEXT => return Err(Error::Unsupported("sending encrypted files")),
                INODE_ITEM => {
                    let c = c.or_default();
                    c.created = old.is_none();
                    c.deleted = new.is_none();
                    c.meta = true;
                }
                DIR_INDEX => {
                    if let Some(o) = old {
                        let e = decode_pair(&o)?;
                        if !is_link(e.ino) {
                            unlinks.push((k.objectid, e.name));
                        }
                    }
                    if let Some(n) = new {
                        let e = decode_pair(&n)?;
                        if !is_link(e.ino) {
                            links.push((k.objectid, k.offset, e));
                        }
                    }
                }
                EXTENT_DATA => {
                    let len = |v: &Option<Vec<u8>>| -> Result<u64> {
                        Ok(match v {
                            None => 0,
                            Some(d) => match FileExtent::decode(d)? {
                                FileExtent::Inline(v) => (v.len() as u64).div_ceil(BS).max(1),
                                FileExtent::Reg { blocks, .. }
                                | FileExtent::Prealloc { blocks, .. } => blocks,
                                FileExtent::Comp(c) => c.blocks,
                            },
                        })
                    };
                    let n = len(&old)?.max(len(&new)?);
                    c.or_default().data.insert(k.offset / BS, n);
                }
                XATTR_ITEM => c.or_default().xattrs = true,
                _ => {}
            }
        }
        // Names first go, then inodes, so nothing is named twice.
        for (dir, name) in unlinks {
            w.cmd(CMD_UNLINK, &P::default().u64(dir).bytes(&name).0)?;
        }
        for (&ino, c) in &changes {
            if c.deleted && !c.created {
                w.cmd(CMD_DELETE, &P::default().u64(ino).0)?;
            }
        }
        for (&ino, c) in &changes {
            if c.created && !c.deleted {
                let i = self.inode(ino)?;
                w.cmd(CMD_CREATE, &P::default().u64(ino).bytes(&i.encode()).0)?;
                stats.inodes += 1;
            }
        }
        for (dir, index, e) in links {
            let p = P::default()
                .u64(dir)
                .u64(index)
                .u64(e.ino)
                .u8(e.kind.to_disk())
                .bytes(&e.name);
            w.cmd(CMD_LINK, &p.0)?;
        }
        // Data, sharing what this stream already sent as clones.
        let mut sent = Sent::default();
        for (&ino, c) in &changes {
            if c.deleted || c.data.is_empty() {
                continue;
            }
            for (s, l) in c.data.iter() {
                let ranges = if c.created || parent.is_none() {
                    vec![(s, s + l)]
                } else {
                    self.changed_blocks(ino, parent_sv, sv, s, s + l)?
                };
                for (a, b) in ranges {
                    self.send_data(&mut w, &mut sent, &mut stats, ino, a, b, c.created)?;
                }
            }
        }
        for (&ino, c) in &changes {
            if c.xattrs && !c.deleted {
                let mut list = Vec::new();
                for (_, d) in self.items_of(ino, XATTR_ITEM, usize::MAX)? {
                    list.extend(decode_xattrs(&d)?);
                }
                let p = P::default().u64(ino).bytes(&encode_xattrs(&list));
                w.cmd(CMD_XATTRS, &p.0)?;
            }
        }
        // Final attributes (sizes, link counts, times) last.
        for (&ino, c) in &changes {
            if !c.deleted && (c.meta || c.created) {
                let i = self.inode(ino)?;
                w.cmd(CMD_SETATTR, &P::default().u64(ino).bytes(&i.encode()).0)?;
            }
        }
        w.cmd(CMD_END, &[])?;
        stats.bytes = w.bytes;
        Ok(stats)
    }

    /// Runs of file blocks `from..to` of `ino` that subvolume `new` maps
    /// differently from `old` (an extent split by an overwrite still maps
    /// its other blocks where they were).
    fn changed_blocks(
        &mut self,
        ino: u64,
        old: u64,
        new: u64,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, u64)>> {
        self.enter(old)?;
        let a = (self.inline_data(ino)?, self.extents(ino, from, to)?);
        self.enter(new)?;
        let b = (self.inline_data(ino)?, self.extents(ino, from, to)?);
        if a.0.is_some() || b.0.is_some() {
            return Ok(vec![(from, to)]);
        }
        // Where block `x` lives: (kind, disk block or extent, offset).
        let at = |exts: &[Ext], x: u64| -> Option<(u8, u64, u64)> {
            let e = exts.iter().find(|e| e.lblk <= x && x < e.lblk + e.blocks)?;
            Some(match e.z {
                None => (u8::from(e.prealloc), e.pblk + (x - e.lblk), 0),
                Some(z) => (2, e.pblk, z.offset + (x - e.lblk)),
            })
        };
        let mut edges: Vec<u64> = [from, to]
            .into_iter()
            .chain(
                a.1.iter()
                    .chain(&b.1)
                    .flat_map(|e| [e.lblk, e.lblk + e.blocks]),
            )
            .filter(|&x| (from..=to).contains(&x))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        let mut out: Vec<(u64, u64)> = Vec::new();
        for w in edges.windows(2) {
            let (s, t) = (w[0], w[1]);
            // Both sides are linear inside a segment: its first block says.
            if at(&a.1, s) != at(&b.1, s) {
                match out.last_mut() {
                    Some(last) if last.1 == s => last.1 = t,
                    _ => out.push((s, t)),
                }
            }
        }
        Ok(out)
    }

    /// Commands for file blocks `from..to` of `ino` as the snapshot has
    /// them.
    #[allow(clippy::too_many_arguments)]
    fn send_data(
        &mut self,
        w: &mut Writer,
        sent: &mut Sent,
        stats: &mut SendStats,
        ino: u64,
        from: u64,
        to: u64,
        created: bool,
    ) -> Result<()> {
        if let Some(v) = self.inline_data(ino)? {
            if from == 0 {
                w.cmd(CMD_WRITE, &P::default().u64(ino).u64(0).bytes(&v).0)?;
                stats.data_bytes += v.len() as u64;
            }
            return Ok(());
        }
        let mut lb = from;
        for e in self.extents(ino, from, to)? {
            let s = e.lblk.max(from);
            let t = (e.lblk + e.blocks).min(to);
            if s > lb && !created {
                let p = P::default().u64(ino).u64(lb * BS).u64((s - lb) * BS);
                w.cmd(CMD_PUNCH, &p.0)?;
            }
            lb = t;
            if e.prealloc {
                let p = P::default().u64(ino).u64(s * BS).u64((t - s) * BS);
                if !created {
                    w.cmd(CMD_PUNCH, &p.0)?;
                }
                let p = P::default().u64(ino).u64(s * BS).u64((t - s) * BS);
                w.cmd(CMD_PREALLOC, &p.0)?;
                continue;
            }
            if let Some((src, src_blk)) = sent.find(&e, s, t) {
                let p = P::default()
                    .u64(ino)
                    .u64(s * BS)
                    .u64((t - s) * BS)
                    .u64(src)
                    .u64(src_blk * BS);
                w.cmd(CMD_CLONE, &p.0)?;
                stats.cloned_bytes += (t - s) * BS;
                continue;
            }
            sent.add(&e, s, t, ino);
            let mut pos = s * BS;
            while pos < t * BS {
                let n = (t * BS - pos).min(WRITE_MAX);
                let mut buf = vec![0u8; n as usize];
                self.read_blocks(ino, pos, &mut buf)?;
                w.cmd(CMD_WRITE, &P::default().u64(ino).u64(pos).bytes(&buf).0)?;
                stats.data_bytes += n;
                pos += n;
            }
        }
        if lb < to && !created {
            let p = P::default().u64(ino).u64(lb * BS).u64((to - lb) * BS);
            w.cmd(CMD_PUNCH, &p.0)?;
        }
        Ok(())
    }

    /// Receive a stream from [`send`](Self::send) as a new read-only
    /// subvolume in directory `dir`, named as the sent one. An
    /// incremental stream needs the subvolume received from (or snapshot
    /// as) its parent. Returns the new subvolume's root directory.
    pub fn receive(&mut self, dir: u64, input: ReceiveSource) -> Result<u64> {
        let d = self.at_rw(dir)?;
        self.growable()?;
        let dir_sv = self.subvol;
        // Header.
        let mut h = vec![0u8; 8 + 4 + 16 + 16 + 2];
        input(&mut h)?;
        if &h[..8] != MAGIC {
            return Err(Error::Unsupported("not an EclipseFS send stream"));
        }
        let flags = u32::from_le_bytes(h[8..12].try_into().unwrap());
        let ident: [u8; 16] = h[12..28].try_into().unwrap();
        let parent: [u8; 16] = h[28..44].try_into().unwrap();
        let nlen = u16::from_le_bytes(h[44..46].try_into().unwrap()) as usize;
        let mut rest = vec![0u8; nlen + 8 + 4];
        input(&mut rest)?;
        h.extend_from_slice(&rest[..nlen + 8]);
        let crc = u32::from_le_bytes(rest[nlen + 8..].try_into().unwrap());
        if crc32c(&h) != crc || flags & !FLAG_INCREMENTAL != 0 {
            return Err(BAD);
        }
        let name = core::str::from_utf8(&rest[..nlen]).map_err(|_| BAD)?;
        let name = check_name(name)?.to_vec();
        let next_ino = u64::from_le_bytes(rest[nlen..nlen + 8].try_into().unwrap());
        if self.dir_lookup(d, &name)?.is_some() {
            return Err(Error::Exists);
        }
        permit(immutable(&self.inode(d)?))?;
        // The subvolume it goes into: empty, or a snapshot of the parent.
        let now = self.now();
        let id = self.new_subvol_id()?;
        let mut item = if flags & FLAG_INCREMENTAL != 0 {
            let items: Vec<(u64, RootItem)> = self.current_root_items()?.into_iter().collect();
            let found = items
                .iter()
                .find(|(_, it)| it.received_uuid == parent)
                .or_else(|| items.iter().find(|(_, it)| it.uuid == parent))
                .filter(|(_, it)| it.flags & ROOT_FLAG_RECEIVING == 0)
                .map(|(i, _)| *i)
                .ok_or(Error::NotFound)?;
            self.flush()?;
            self.enter(found)?;
            let mut item = self.fs_item.clone();
            self.st.share(&mut self.sp, item.root)?;
            item.parent = found;
            item.parent_gen = self.sb.generation;
            item.parent_uuid = item.uuid;
            item.limit = 0;
            item
        } else {
            let root = self.st.create(&mut self.sp, id, 0)?;
            RootItem {
                root,
                next_ino: FIRST_INO,
                inode_count: 0,
                root_dir: FIRST_INO,
                ..Default::default()
            }
        };
        item.flags = ROOT_FLAG_RECEIVING;
        item.otime = now;
        item.uuid = subvol_uuid(&self.sb.uuid, id, self.st.gen, now);
        item.received_uuid = [0; 16];
        self.root_insert(Key::new(id, ROOT_ITEM, 0), item.encode())?;
        self.enter(id)?;
        let r = self.apply_stream(input);
        let r = r.and_then(|_| {
            if next_ino > INO_MASK + 1 {
                return Err(BAD);
            }
            self.fs_item.next_ino = self.fs_item.next_ino.max(next_ino);
            // Whatever the stream says, the result must be a sound tree.
            let mut item = self.fs_item.clone();
            item.root = self.fs_root;
            let (_, _, links, _) = self.check_subvol(&item)?;
            let root_is_dir = self.inode(FIRST_INO)?.kind() == FileKind::Dir;
            (links.is_empty() && root_is_dir).then_some(()).ok_or(BAD)
        });
        if let Err(e) = r {
            warn!("eclipsefs: receive failed: {:?}", e);
            self.enter(dir_sv)?;
            self.drop_tree(id)?;
            self.commit()?;
            return Err(match e {
                Error::Corrupt(_) | Error::NotFound | Error::Exists | Error::Invalid => BAD,
                e => e,
            });
        }
        self.fs_item.flags = ROOT_FLAG_RDONLY;
        self.fs_item.received_uuid = ident;
        self.fs_item.rtime = self.now();
        self.enter(dir_sv)?;
        self.link_subvol(d, &name, id)?;
        self.commit()?;
        Ok(handle(id, FIRST_INO))
    }

    /// Apply commands until `END`.
    fn apply_stream(&mut self, input: ReceiveSource) -> Result<()> {
        loop {
            let mut h = [0u8; 9];
            input(&mut h)?;
            let len = u32::from_le_bytes(h[1..5].try_into().unwrap()) as usize;
            if len > MAX_CMD {
                return Err(BAD);
            }
            let mut payload = vec![0u8; len];
            input(&mut payload)?;
            if crc32c(&payload) != u32::from_le_bytes(h[5..9].try_into().unwrap()) {
                return Err(BAD);
            }
            if h[0] == CMD_END {
                return Ok(());
            }
            self.apply(h[0], &mut R(&payload))?;
            self.commit()?;
        }
    }

    fn apply(&mut self, ty: u8, r: &mut R) -> Result<()> {
        match ty {
            CMD_UNLINK => {
                let dir = r.ino()?;
                let name = r.rest();
                self.remove_entry(dir, name)?;
            }
            CMD_DELETE => {
                let ino = r.ino()?;
                self.inode(ino)?;
                self.purge(ino)?;
            }
            CMD_CREATE => {
                let ino = r.ino()?;
                let mut i = InodeItem::decode(r.rest())?;
                if self.get(Key::new(ino, INODE_ITEM, 0))?.is_some() {
                    return Err(BAD);
                }
                i.size = 0;
                i.nbytes = 0;
                // Streams never carry encrypted files.
                i.flags &= !(FL_ENCRYPT as u64);
                self.insert(Key::new(ino, INODE_ITEM, 0), i.encode())?;
                self.fs_item.inode_count += 1;
                self.fs_item.next_ino = self.fs_item.next_ino.max(ino + 1);
            }
            CMD_LINK => {
                let dir = r.ino()?;
                let index = r.u64()?;
                let ino = r.ino()?;
                let kind = FileKind::from_disk(r.u8()?).ok_or(BAD)?;
                let name = r.rest();
                let name = check_name(core::str::from_utf8(name).map_err(|_| BAD)?)?;
                if self.inode(ino)?.kind() != kind {
                    return Err(BAD);
                }
                self.add_entry_at(dir, name, ino, kind, Some(index))?;
            }
            CMD_WRITE => {
                let ino = r.ino()?;
                let off = r.u64()?;
                let data = r.rest();
                let mut i = self.file(ino)?;
                crate::file_end(off, data.len() as u64).map_err(|_| BAD)?;
                let n = self.write_data(ino, &mut i, off, data)?;
                self.put_inode(ino, i)?;
                (n == data.len()).then_some(()).ok_or(Error::NoSpace)?;
            }
            CMD_CLONE => {
                let ino = r.ino()?;
                let off = r.u64()?;
                let len = r.u64()?;
                let src = r.ino()?;
                let src_off = r.u64()?;
                if [off, len, src_off].iter().any(|v| v % BS != 0) || len == 0 {
                    return Err(BAD);
                }
                crate::file_end(off, len).map_err(|_| BAD)?;
                crate::file_end(src_off, len).map_err(|_| BAD)?;
                let (d0, s0, n) = (off / BS, src_off / BS, len / BS);
                if src == ino && d0 < s0 + n && s0 < d0 + n {
                    return Err(BAD);
                }
                let si = self.file(src)?;
                let mut i = self.file(ino)?;
                // Checksummed and no-COW data never share blocks.
                if self.inline_data(src)?.is_some() || (si.flags ^ i.flags) & FL_NOCOW as u64 != 0 {
                    return Err(BAD);
                }
                if self.inline_data(ino)?.is_some() {
                    self.uninline(ino, &mut i)?;
                }
                self.share_extents(ino, &mut i, (self.subvol, src), s0, d0, n)?;
                i.size = i.size.max(off + len);
                self.put_inode(ino, i)?;
            }
            CMD_PUNCH => {
                let ino = r.ino()?;
                let off = r.u64()?;
                let len = r.u64()?;
                let mut i = self.file(ino)?;
                if self.inline_data(ino)?.is_some() {
                    if off == 0 {
                        self.delete(Self::ext_key(ino, 0))?;
                        i.nbytes = 0;
                    }
                } else {
                    let end = crate::file_end(off, len).map_err(|_| BAD)?;
                    self.unmap(ino, &mut i, off / BS, end.div_ceil(BS))?;
                }
                self.put_inode(ino, i)?;
            }
            CMD_PREALLOC => {
                let ino = r.ino()?;
                let off = r.u64()?;
                let len = r.u64()?;
                let end = crate::file_end(off, len).map_err(|_| BAD)?;
                let mut i = self.file(ino)?;
                if self.inline_data(ino)?.is_some() {
                    self.uninline(ino, &mut i)?;
                }
                let size = i.size;
                self.preallocate(ino, &mut i, off, end)?;
                i.size = size;
                self.put_inode(ino, i)?;
            }
            CMD_XATTRS => {
                let ino = r.ino()?;
                let rest = r.rest();
                let list = if rest.is_empty() {
                    Vec::new()
                } else {
                    decode_xattrs(rest)?
                };
                self.inode(ino)?;
                for (k, _) in self.items_of(ino, XATTR_ITEM, usize::MAX)? {
                    self.delete(k)?;
                }
                let mut by_hash: BTreeMap<u64, Xattrs> = BTreeMap::new();
                for (n, v) in list {
                    let name = core::str::from_utf8(&n).map_err(|_| BAD)?;
                    Self::check_xattr_name(name)?;
                    by_hash.entry(name_hash(&n)).or_default().push((n, v));
                }
                for (h, l) in by_hash {
                    let v = encode_xattrs(&l);
                    if v.len() > MAX_ITEM {
                        return Err(BAD);
                    }
                    self.insert(Key::new(ino, XATTR_ITEM, h), v)?;
                }
            }
            CMD_SETATTR => {
                let ino = r.ino()?;
                let new = InodeItem::decode(r.rest())?;
                let mut i = self.inode(ino)?;
                if new.kind() != i.kind() {
                    return Err(BAD);
                }
                crate::file_end(new.size, 0).map_err(|_| BAD)?;
                if i.kind() == FileKind::Regular || i.kind() == FileKind::Symlink {
                    self.truncate_data(ino, &mut i, new.size)?;
                }
                let nbytes = i.nbytes;
                let flags = new.flags & !(FL_ENCRYPT as u64);
                let i = InodeItem {
                    nbytes,
                    flags,
                    ..new
                };
                self.put_inode(ino, i)?;
            }
            _ => return Err(BAD),
        }
        Ok(())
    }

    /// Inode `ino`, which must hold data (a file or a symlink).
    fn file(&mut self, ino: u64) -> Result<InodeItem> {
        let i = self.inode(ino)?;
        match i.kind() {
            FileKind::Regular | FileKind::Symlink => Ok(i),
            _ => Err(BAD),
        }
    }

    /// Release the tree of subvolume `id`, which nothing names.
    fn drop_tree(&mut self, id: u64) -> Result<()> {
        self.zpending.retain(|&(sv, _), _| sv != id);
        let root = match self.subvols.remove(&id) {
            Some((root, _)) => root,
            None => {
                let d = self
                    .st
                    .get(&self.root_tree, &Key::new(id, ROOT_ITEM, 0))?
                    .ok_or(Error::Corrupt("missing subvolume"))?;
                RootItem::decode(&d)?.root
            }
        };
        self.root_delete(Key::new(id, ROOT_ITEM, 0))?;
        self.st.release(&mut self.sp, root)
    }

    /// Drop subvolumes an interrupted `receive` left behind.
    pub(super) fn drop_unfinished_receives(&mut self) -> Result<()> {
        let left: Vec<u64> = self
            .root_items()?
            .into_iter()
            .filter(|(_, it)| it.flags & ROOT_FLAG_RECEIVING != 0)
            .map(|(id, _)| id)
            .collect();
        if left.is_empty() {
            return Ok(());
        }
        let cur = self.subvol;
        for id in left {
            warn!(
                "eclipsefs: dropping subvolume {} from an unfinished receive",
                id
            );
            if id == cur {
                self.enter(FS_TREE)?;
            }
            self.drop_tree(id)?;
        }
        self.flush()
    }
}

/// Extents this stream already sent, to send later references to them as
/// clones (reflinks within the snapshot stay reflinks on the other side).
#[derive(Default)]
struct Sent {
    /// Regular extents: first disk block -> (blocks, inode, file block).
    plain: BTreeMap<u64, (u64, u64, u64)>,
    /// Compressed extents: (disk block, first block they hold) -> (blocks,
    /// inode, file block).
    packed: BTreeMap<(u64, u64), (u64, u64, u64)>,
}

impl Sent {
    fn add(&mut self, e: &Ext, s: u64, t: u64, ino: u64) {
        match e.z {
            None => {
                self.plain.insert(e.pblk + (s - e.lblk), (t - s, ino, s));
            }
            Some(z) => {
                self.packed
                    .insert((e.pblk, z.offset + (s - e.lblk)), (t - s, ino, s));
            }
        }
    }

    /// Where file blocks `s..t` of `e` were sent before: (inode, block).
    fn find(&self, e: &Ext, s: u64, t: u64) -> Option<(u64, u64)> {
        match e.z {
            None => {
                let p = e.pblk + (s - e.lblk);
                let (&start, &(n, ino, lb)) = self.plain.range(..=p).next_back()?;
                (p + (t - s) <= start + n).then_some((ino, lb + (p - start)))
            }
            Some(z) => {
                let o = z.offset + (s - e.lblk);
                let (&(disk, start), &(n, ino, lb)) =
                    self.packed.range(..=(e.pblk, o)).next_back()?;
                (disk == e.pblk && o + (t - s) <= start + n).then_some((ino, lb + (o - start)))
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A command is `[type | length | CRC32C | payload]`, and the byte count
    /// the stats report is what went to the sink.
    #[test]
    fn a_command_is_framed_with_its_length_and_crc() {
        let mut out = Vec::new();
        let mut sink = |b: &[u8]| -> Result<()> {
            out.extend_from_slice(b);
            Ok(())
        };
        let mut w = Writer {
            out: &mut sink,
            bytes: 0,
        };
        w.cmd(CMD_WRITE, b"payload").unwrap();
        w.cmd(CMD_END, &[]).unwrap();
        assert_eq!(w.bytes, 9 + 7 + 9);
        assert_eq!(out[0], CMD_WRITE);
        assert_eq!(u32::from_le_bytes(out[1..5].try_into().unwrap()), 7);
        assert_eq!(
            u32::from_le_bytes(out[5..9].try_into().unwrap()),
            crc32c(b"payload")
        );
        assert_eq!(&out[9..16], b"payload");
        assert_eq!(out[16], CMD_END);
        assert_eq!(out.len(), 25);
    }

    /// A sink that fails stops the stream with its error.
    #[test]
    fn a_failing_sink_stops_the_stream() {
        let mut sink = |_: &[u8]| -> Result<()> { Err(Error::NoSpace) };
        let mut w = Writer {
            out: &mut sink,
            bytes: 0,
        };
        assert_eq!(w.cmd(CMD_END, &[]), Err(Error::NoSpace));
        assert_eq!(w.bytes, 0);
    }

    #[test]
    fn payloads_read_back_as_built() {
        let p = P::default()
            .u64(FIRST_INO + 5)
            .u8(3)
            .u64(u64::MAX)
            .bytes(b"name");
        let mut r = R(&p.0);
        assert_eq!(r.ino().unwrap(), FIRST_INO + 5);
        assert_eq!(r.u8().unwrap(), 3);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.rest(), b"name");
        assert_eq!(r.rest(), b"");
    }

    /// Every short read is a corrupt stream, never a panic: `receive` reads
    /// whatever arrives over the network.
    #[test]
    fn short_payloads_are_corrupt_streams() {
        let p = P::default().u64(FIRST_INO).0;
        for n in 0..8 {
            assert_eq!(R(&p[..n]).u64(), Err(BAD));
            assert_eq!(R(&p[..n]).ino(), Err(BAD));
        }
        assert_eq!(R(&[]).u8(), Err(BAD));
        assert_eq!(R(&[1, 2]).take(3), Err(BAD));
    }

    /// Inode numbers outside the range a subvolume can hold are rejected
    /// before they are used as keys.
    #[test]
    fn inode_numbers_are_range_checked() {
        for bad in [0, 1, FIRST_INO - 1, INO_MASK + 1, u64::MAX] {
            assert_eq!(R(&P::default().u64(bad).0).ino(), Err(BAD), "{}", bad);
        }
        for good in [FIRST_INO, INO_MASK] {
            assert_eq!(R(&P::default().u64(good).0).ino(), Ok(good));
        }
    }

    fn plain(lblk: u64, pblk: u64, blocks: u64) -> Ext {
        Ext {
            lblk,
            pblk,
            blocks,
            prealloc: false,
            z: None,
        }
    }

    /// A range sent before is found again, at the right offset, only when
    /// all of it was sent: half of a clone source is not a clone source.
    #[test]
    fn sent_extents_are_found_again_for_clones() {
        let mut s = Sent::default();
        let e = plain(0, 1000, 16);
        s.add(&e, 4, 12, FIRST_INO + 1);
        let other = plain(100, 1000, 16);
        assert_eq!(s.find(&other, 104, 112), Some((FIRST_INO + 1, 4)));
        assert_eq!(s.find(&other, 106, 108), Some((FIRST_INO + 1, 6)));
        assert_eq!(s.find(&other, 102, 106), None);
        assert_eq!(s.find(&other, 110, 114), None);
        assert_eq!(s.find(&plain(0, 5000, 4), 0, 4), None);
    }

    #[test]
    fn compressed_extents_match_on_disk_block_and_offset() {
        let z = Zext {
            algo: Compression::Zstd,
            disk_blocks: 2,
            ram_blocks: 8,
            offset: 0,
        };
        let e = Ext {
            lblk: 0,
            pblk: 700,
            blocks: 8,
            prealloc: false,
            z: Some(z),
        };
        let mut s = Sent::default();
        s.add(&e, 0, 8, FIRST_INO);
        assert_eq!(s.find(&e, 2, 6), Some((FIRST_INO, 2)));
        // Same offsets in a different compressed extent: not the same data.
        let other = Ext { pblk: 701, ..e };
        assert_eq!(s.find(&other, 2, 6), None);
        // A plain extent at the same disk block is not that data either.
        assert_eq!(s.find(&plain(0, 700, 8), 0, 8), None);
    }

    // -----------------------------------------------------------------------
    // Streams end to end
    // -----------------------------------------------------------------------

    use crate::testdisk::{clock, formatted};

    fn mounted() -> CowFs {
        let mut fs = CowFs::mount(formatted(32), false).unwrap();
        fs.set_clock(clock);
        fs
    }

    fn sent(fs: &mut CowFs, snap: u64, parent: Option<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        fs.send(snap, parent, &mut |b: &[u8]| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        out
    }

    fn recv(fs: &mut CowFs, bytes: &[u8]) -> Result<u64> {
        let mut at = 0;
        fs.receive(fs.root_ino(), &mut |buf: &mut [u8]| {
            let b = bytes.get(at..at + buf.len()).ok_or(Error::Io)?;
            buf.copy_from_slice(b);
            at += buf.len();
            Ok(())
        })
    }

    /// A header for a full stream named "got", then `cmds` and `END`.
    fn stream(flags: u32, next_ino: u64, cmds: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&flags.to_le_bytes());
        h.extend_from_slice(&[3; 16]);
        h.extend_from_slice(&[0; 16]);
        h.extend_from_slice(&3u16.to_le_bytes());
        h.extend_from_slice(b"got");
        h.extend_from_slice(&next_ino.to_le_bytes());
        let crc = crc32c(&h);
        h.extend_from_slice(&crc.to_le_bytes());
        let mut out = h;
        {
            let mut sink = |b: &[u8]| -> Result<()> {
                out.extend_from_slice(b);
                Ok(())
            };
            let mut w = Writer {
                out: &mut sink,
                bytes: 0,
            };
            for (ty, p) in cmds {
                w.cmd(*ty, p).unwrap();
            }
            w.cmd(CMD_END, &[]).unwrap();
        }
        out
    }

    fn inode(kind: FileKind) -> Vec<u8> {
        let mut i = InodeItem::new(kind, 0o755, clock(), 1);
        i.parent = FIRST_INO;
        if kind == FileKind::Dir {
            i.nlink = 2;
        }
        i.encode()
    }

    fn create(ino: u64, kind: FileKind) -> (u8, Vec<u8>) {
        (CMD_CREATE, P::default().u64(ino).bytes(&inode(kind)).0)
    }

    fn link(dir: u64, index: u64, ino: u64, kind: FileKind, name: &[u8]) -> (u8, Vec<u8>) {
        let p = P::default()
            .u64(dir)
            .u64(index)
            .u64(ino)
            .u8(kind.to_disk())
            .bytes(name);
        (CMD_LINK, p.0)
    }

    fn write(ino: u64, off: u64, data: &[u8]) -> (u8, Vec<u8>) {
        (CMD_WRITE, P::default().u64(ino).u64(off).bytes(data).0)
    }

    fn range(ty: u8, ino: u64, off: u64, len: u64) -> (u8, Vec<u8>) {
        (ty, P::default().u64(ino).u64(off).u64(len).0)
    }

    fn clone(ino: u64, off: u64, len: u64, src: u64, src_off: u64) -> (u8, Vec<u8>) {
        let p = P::default()
            .u64(ino)
            .u64(off)
            .u64(len)
            .u64(src)
            .u64(src_off);
        (CMD_CLONE, p.0)
    }

    const F: u64 = FIRST_INO + 1;
    const G: u64 = FIRST_INO + 2;

    /// The root directory and two files, `f` holding two blocks and `g`
    /// a few bytes inline, then `more`.
    fn with_files(more: &[(u8, Vec<u8>)]) -> Vec<(u8, Vec<u8>)> {
        let mut v = alloc::vec![
            create(FIRST_INO, FileKind::Dir),
            create(F, FileKind::Regular),
            create(G, FileKind::Regular),
            link(FIRST_INO, 2, F, FileKind::Regular, b"f"),
            link(FIRST_INO, 3, G, FileKind::Regular, b"g"),
            write(F, 0, &[1; 2 * BS as usize]),
            write(G, 0, b"inline"),
        ];
        v.extend_from_slice(more);
        v
    }

    fn refused(cmds: &[(u8, Vec<u8>)]) {
        let mut fs = mounted();
        let s = stream(0, G + 1, &with_files(cmds));
        assert_eq!(
            recv(&mut fs, &s).err(),
            Some(BAD),
            "{:?}",
            cmds.iter().map(|c| c.0).collect::<Vec<_>>()
        );
        fs.check().unwrap();
    }

    /// Compression still pending in a failed receive goes with its tree.
    #[test]
    fn a_failed_receive_drops_its_pending_compression() {
        let mut fs = mounted();
        fs.set_mount_compression(Some(Compression::Lz4));
        let s = stream(0, G + 1, &with_files(&[write(G + 5, 0, b"x")]));
        assert_eq!(recv(&mut fs, &s).err(), Some(BAD));
        assert!(fs.zpending.is_empty());
        fs.check().unwrap();
    }

    #[test]
    fn a_hand_made_stream_is_received() {
        let mut fs = mounted();
        let s = stream(0, G + 1, &with_files(&[]));
        let h = recv(&mut fs, &s).unwrap();
        let (sv, _) = split(h);
        let f = fs.lookup(h, "f").unwrap();
        assert_eq!(split(f), (sv, F));
        fs.check().unwrap();
    }

    #[test]
    fn headers_that_are_not_streams() {
        let mut fs = mounted();
        let mut s = stream(0, G + 1, &with_files(&[]));
        s[0] = b'X';
        assert!(matches!(recv(&mut fs, &s), Err(Error::Unsupported(_))));
        let mut s = stream(0, G + 1, &[]);
        s[50] ^= 1;
        assert_eq!(recv(&mut fs, &s).err(), Some(BAD));
        let s = stream(4, G + 1, &[]);
        assert_eq!(recv(&mut fs, &s).err(), Some(BAD));
        let s = stream(0, INO_MASK + 2, &with_files(&[]));
        assert_eq!(recv(&mut fs, &s).err(), Some(BAD));
        // A command longer than any is.
        let mut s = stream(0, G + 1, &[]);
        let end = s.len() - 9;
        s[end + 1..end + 5].copy_from_slice(&(MAX_CMD as u32 + 1).to_le_bytes());
        assert_eq!(recv(&mut fs, &s).err(), Some(BAD));
        fs.check().unwrap();
    }

    #[test]
    fn commands_that_do_not_fit_what_is_there() {
        // Created twice.
        refused(&[create(F, FileKind::Regular)]);
        // Linked as what it is not.
        refused(&[link(FIRST_INO, 9, F, FileKind::Dir, b"d")]);
        // Attributes of another kind.
        refused(&[(
            CMD_SETATTR,
            P::default().u64(F).bytes(&inode(FileKind::Dir)).0,
        )]);
        // Data written to a directory.
        refused(&[write(FIRST_INO, 0, b"x")]);
        // A command nobody knows.
        refused(&[(99, Vec::new())]);
        // One xattr bigger than an item.
        let big = encode_xattrs(&[(b"user.x".to_vec(), alloc::vec![0; MAX_ITEM])]);
        refused(&[(CMD_XATTRS, P::default().u64(F).bytes(&big).0)]);
    }

    #[test]
    fn clones_that_cannot_be() {
        refused(&[clone(F, 1, BS, F, 0)]);
        refused(&[clone(F, 0, 0, F, 0)]);
        refused(&[clone(F, u64::MAX / BS * BS, BS, G, 0)]);
        refused(&[clone(F, 0, BS, F, 0)]);
        refused(&[clone(F, 0, BS, G, 0)]);
    }

    /// Clones, holes and preallocation into files still inline.
    #[test]
    fn data_commands_on_inline_files() {
        let mut fs = mounted();
        let h = FIRST_INO + 3;
        let cmds = with_files(&[
            clone(G, BS, BS, F, 0),
            create(h, FileKind::Regular),
            link(FIRST_INO, 4, h, FileKind::Regular, b"h"),
            write(h, 0, b"abc"),
            range(CMD_PUNCH, h, 5, 3),
            range(CMD_PUNCH, h, 0, 3),
            write(h, 0, b"abc"),
            range(CMD_PREALLOC, h, BS, BS),
        ]);
        let got = recv(&mut fs, &stream(0, h + 1, &cmds)).unwrap();
        fs.check().unwrap();
        let g = fs.lookup(got, "g").unwrap();
        let mut buf = [0u8; 6];
        fs.read(g, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"inline");
    }

    /// An unfinished receive is dropped at mount, even while it is the
    /// subvolume entered.
    #[test]
    fn an_unfinished_receive_entered_is_dropped() {
        let mut fs = mounted();
        let root = fs.st.create(&mut fs.sp, FIRST_SUBVOL, 0).unwrap();
        let item = RootItem {
            root,
            next_ino: FIRST_INO,
            root_dir: FIRST_INO,
            flags: ROOT_FLAG_RECEIVING,
            ..Default::default()
        };
        fs.root_insert(Key::new(FIRST_SUBVOL, ROOT_ITEM, 0), item.encode())
            .unwrap();
        fs.enter(FIRST_SUBVOL).unwrap();
        fs.drop_unfinished_receives().unwrap();
        assert_eq!(fs.subvol, FS_TREE);
        fs.check().unwrap();
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    /// A received subvolume is sent under the identity it was received
    /// as, from a read-only mount too; the top subvolume has no name to
    /// send (and so no receiver takes it).
    #[test]
    fn identities_and_names_of_what_is_sent() {
        let mut fs = mounted();
        let top = fs.root_ino();
        let f = fs.create(top, "f", FileKind::Regular, 0o644, 0).unwrap();
        fs.write(f, 0, b"data").unwrap();
        let snap = fs.snapshot(top, top, "snap", true).unwrap();
        let s = sent(&mut fs, snap, None);
        let d = formatted(32);
        let mut other = CowFs::mount(d.clone(), false).unwrap();
        let got = recv(&mut other, &s).unwrap();
        let again = sent(&mut other, got, None);
        assert_eq!(again[12..28], s[12..28]);
        drop(other);
        let mut ro = CowFs::mount(d, true).unwrap();
        assert_eq!(sent(&mut ro, got, None), again);

        fs.set_subvolume_read_only(FS_TREE, true).unwrap();
        let unnamed = sent(&mut fs, top, None);
        assert_eq!(&unnamed[44..46], &[0, 0]);
        assert_eq!(recv(&mut mounted(), &unnamed).err(), Some(Error::Invalid));
    }

    /// Holes punched, blocks preallocated and compressed blocks rewritten
    /// since the parent all reach the other side.
    #[test]
    fn increments_with_holes_preallocation_and_compression() {
        use crate::cowfmt::FALLOC_FL_PUNCH_HOLE;
        let mut fs = mounted();
        let top = fs.root_ino();
        let sv = fs.create_subvolume(top, "sv").unwrap();
        let a = fs.create(sv, "a", FileKind::Regular, 0o644, 0).unwrap();
        fs.write(a, 0, &[1; 4 * BS as usize]).unwrap();
        let z = fs.create(sv, "z", FileKind::Regular, 0o644, 0).unwrap();
        fs.set_compression(z, Some(Compression::Zlib)).unwrap();
        fs.write(z, 0, &[2; 32 * BS as usize]).unwrap();
        let s = fs.create(sv, "s", FileKind::Regular, 0o644, 0).unwrap();
        fs.write(s, 3 * BS, &[3; BS as usize]).unwrap();
        let p1 = fs.snapshot(sv, top, "p1", true).unwrap();

        fs.fallocate(a, FALLOC_FL_PUNCH_HOLE | 1, BS, BS).unwrap();
        fs.write(a, 2 * BS, &[9; BS as usize]).unwrap();
        fs.fallocate(a, 0, 6 * BS, 2 * BS).unwrap();
        fs.fallocate(a, FALLOC_FL_PUNCH_HOLE | 1, 3 * BS, BS)
            .unwrap();
        fs.write(z, 5 * BS, &[8; BS as usize]).unwrap();
        fs.truncate(s, 0).unwrap();
        fs.write(s, 0, b"now inline").unwrap();
        let p2 = fs.snapshot(sv, top, "p2", true).unwrap();

        let full = sent(&mut fs, p1, None);
        let inc = sent(&mut fs, p2, Some(p1));
        let mut other = mounted();
        recv(&mut other, &full).unwrap();
        let got = recv(&mut other, &inc).unwrap();
        other.check().unwrap();
        for (name, len) in [("a", 8 * BS), ("z", 32 * BS), ("s", 10)] {
            let mut want = vec![0u8; len as usize];
            let mut have = vec![0u8; len as usize];
            let h = fs.lookup(p2, name).unwrap();
            fs.read(h, 0, &mut want).unwrap();
            let h = other.lookup(got, name).unwrap();
            other.read(h, 0, &mut have).unwrap();
            assert_eq!(want, have, "{}", name);
        }
    }
}
