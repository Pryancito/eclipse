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
                for (_, d) in self.st.range(
                    &self.fs_root,
                    &Key::new(ino, XATTR_ITEM, 0),
                    &Key::new(ino, XATTR_ITEM, u64::MAX),
                    usize::MAX,
                )? {
                    list.extend(decode_xattrs(&d)?);
                }
                w.cmd(
                    CMD_XATTRS,
                    &P::default().u64(ino).bytes(&encode_xattrs(&list)).0,
                )?;
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
                w.cmd(
                    CMD_PUNCH,
                    &P::default().u64(ino).u64(lb * BS).u64((s - lb) * BS).0,
                )?;
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
            w.cmd(
                CMD_PUNCH,
                &P::default().u64(ino).u64(lb * BS).u64((to - lb) * BS).0,
            )?;
        }
        Ok(())
    }

    /// Receive a stream from [`send`](Self::send) as a new read-only
    /// subvolume in directory `dir`, named as the sent one. An
    /// incremental stream needs the subvolume received from (or snapshot
    /// as) its parent. Returns the new subvolume's root directory.
    pub fn receive(&mut self, dir: u64, input: ReceiveSource) -> Result<u64> {
        let d = self.at_rw(dir)?;
        self.writable()?;
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
        self.st.insert(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_ITEM, 0),
            item.encode(),
        )?;
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
            if !links.is_empty() || self.inode(FIRST_INO)?.kind() != FileKind::Dir {
                return Err(BAD);
            }
            Ok(())
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
                off.checked_add(data.len() as u64).ok_or(BAD)?;
                let n = self.write_data(ino, &mut i, off, data)?;
                self.put_inode(ino, i)?;
                if n < data.len() {
                    return Err(Error::NoSpace);
                }
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
                let (d0, s0, n) = (off / BS, src_off / BS, len / BS);
                if d0.checked_add(n).is_none() || s0.checked_add(n).is_none() {
                    return Err(BAD);
                }
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
                    let end = off.checked_add(len).ok_or(BAD)?;
                    self.unmap(ino, &mut i, off / BS, end.div_ceil(BS))?;
                }
                self.put_inode(ino, i)?;
            }
            CMD_PREALLOC => {
                let ino = r.ino()?;
                let off = r.u64()?;
                let len = r.u64()?;
                let end = off.checked_add(len).ok_or(BAD)?;
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
                for (k, _) in self.st.range(
                    &self.fs_root,
                    &Key::new(ino, XATTR_ITEM, 0),
                    &Key::new(ino, XATTR_ITEM, u64::MAX),
                    usize::MAX,
                )? {
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
                if i.kind() == FileKind::Regular || i.kind() == FileKind::Symlink {
                    self.truncate_data(ino, &mut i, new.size)?;
                }
                let nbytes = i.nbytes;
                let flags = new.flags & !(FL_ENCRYPT as u64);
                self.put_inode(
                    ino,
                    InodeItem {
                        nbytes,
                        flags,
                        ..new
                    },
                )?;
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
        self.st.delete(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_ITEM, 0),
        )?;
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
