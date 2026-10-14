//! Devices, profiles and balance (format v0.4 volumes, see
//! [`crate::volume`]).
//!
//! A filesystem made on one device is not a volume until it needs to be:
//! the first `add_device` or `balance` turns its groups into single-copy
//! chunks on device 1, where they already are, so nothing moves.
//!
//! Moving a chunk keeps its logical blocks: its used blocks are copied to
//! the new stripes, the map is switched, and the old stripes stay untouched
//! (held) until the commit that switches the map is on disk, so a crash
//! leaves either layout whole.

use super::*;

impl CowFs {
    /// The volume, turning a one-device filesystem into one first.
    pub(super) fn ensure_volume(&mut self) -> Result<Arc<Volume>> {
        if let Some(v) = &self.vol {
            return Ok(v.clone());
        }
        self.writable()?;
        self.flush()?;
        if !self.dev.is_empty() {
            return Err(Error::Corrupt("uncommitted blocks"));
        }
        let mut map = ChunkMap::default();
        map.devices.insert(1, self.raw.size() / BS);
        for g in self.sp.alloc.groups().iter().filter(|g| !g.is_hole()) {
            map.chunks.insert(
                g.id as u64,
                Chunk {
                    kind: g.kind,
                    profile: Profile::Single,
                    blocks: g.block_count,
                    stripes: vec![Stripe {
                        devid: 1,
                        phys: g.start_block,
                    }],
                },
            );
        }
        map.check()?;
        let sys = map
            .chunks
            .get(&0)
            .cloned()
            .ok_or(Error::Corrupt("no groups"))?;
        let mut devs = BTreeMap::new();
        devs.insert(1, self.raw.clone());
        let vol = Arc::new(Volume::new(devs, map, sb_for_device));
        self.set_volume(vol.clone());
        self.sb.vol = Some(VolumeSb {
            devid: 1,
            num_devices: 1,
            chunk_map: 0,
            chunk_map_len: 0,
            data_profile: Profile::Single,
            meta_profile: Profile::Single,
            sys,
        });
        self.sp.profiles = (Profile::Single, Profile::Single);
        self.sp.chunks_dirty = true;
        self.flush()?;
        Ok(vol)
    }

    /// mkfs of a volume: the filesystem just made on a slice of the first
    /// device becomes a volume whose group 0 holds metadata only.
    pub(crate) fn make_volume_metadata(&mut self) -> Result<()> {
        let vol = self.ensure_volume()?;
        vol.with_map(|m| {
            if let Some(c) = m.chunks.get_mut(&0) {
                c.kind = ChunkKind::Metadata;
            }
        });
        self.sp.alloc.groups_mut()[0].kind = ChunkKind::Metadata;
        self.sp.chunks_dirty = true;
        self.flush()
    }

    fn set_volume(&mut self, vol: Arc<Volume>) {
        let raw: Arc<dyn BlockDevice> = vol.clone();
        self.raw = raw.clone();
        self.dev = Arc::new(Overlay::new(raw.clone()));
        self.st.set_device(raw);
        self.st.mirrors = Some(vol.clone());
        self.st.repair = !self.read_only;
        self.sp.vol = Some(vol.clone());
        self.vol = Some(vol);
    }

    /// Devices in the map that are attached.
    fn present_devices(&self, vol: &Volume) -> usize {
        let map = vol.map();
        map.devices
            .keys()
            .filter(|d| vol.device(**d).is_some() && !self.sp.avoid.contains(d))
            .count()
    }

    /// Before copying chunks: fix every bad copy that has a good one, so
    /// the copies made are of good blocks.
    fn prepare_moves(&mut self, vol: &Volume) -> Result<()> {
        if vol.map().chunks.values().any(|c| c.profile.copies() > 1) {
            let r = self.scrub()?;
            if !r.is_clean() {
                warn!("eclipsefs: moving chunks with unrecoverable blocks in them");
            }
        }
        Ok(())
    }

    /// Copy group `g` to new stripes with `profile` and switch to them.
    fn relocate(&mut self, g: u64, profile: Profile) -> Result<()> {
        self.flush()?;
        let vol = self.vol.clone().ok_or(Error::Invalid)?;
        let old = vol
            .map()
            .chunks
            .get(&g)
            .cloned()
            .ok_or(Error::Corrupt("chunk"))?;
        let avoid = self.sp.avoid.clone();
        let held = vol.held();
        // Group 0's chunk is described by the superblock: never striped.
        let profile = match profile {
            Profile::Raid0 | Profile::Raid10 if g == 0 => Profile::Raid1,
            p => p,
        };
        let stripes = vol
            .with_map(|m| m.place_moving(&old, profile, &avoid, &held))
            .ok_or(Error::NoSpace)?;
        let new = Chunk {
            kind: old.kind,
            profile,
            blocks: old.blocks,
            stripes,
        };
        let runs = self
            .sp
            .alloc
            .groups()
            .get(g as usize)
            .ok_or(Error::Corrupt("chunk without a group"))?
            .used_runs();
        for (start, n) in runs {
            vol.copy_between(&old, &new, start, n)?;
        }
        vol.sync()?;
        let len = old.stripe_len();
        for s in old.stripes.iter().filter(|s| !new.stripes.contains(s)) {
            vol.hold((s.devid, s.phys, len));
        }
        vol.with_map(|m| m.chunks.insert(g, new));
        self.sp.chunks_dirty = true;
        self.flush()
    }

    /// Give an empty chunk's space back to its devices.
    fn drop_chunk(&mut self, g: u64) -> Result<()> {
        let vol = self.vol.clone().ok_or(Error::Invalid)?;
        let Some(old) = vol.with_map(|m| m.chunks.remove(&g)) else {
            return Ok(());
        };
        let len = old.stripe_len();
        for s in &old.stripes {
            vol.hold((s.devid, s.phys, len));
        }
        self.sp.alloc.set_group(AllocationGroup::hole(g as u32));
        self.sp.chunks_dirty = true;
        self.flush()
    }

    /// Add `dev` to the filesystem (`btrfs device add`). New chunks may
    /// go there at once; [`balance`](Self::balance) spreads the old ones.
    pub fn add_device(&mut self, dev: Arc<dyn BlockDevice>) -> Result<u32> {
        self.writable()?;
        let size = dev.size() / BS;
        if size < DEV_RESERVED + MIN_CHUNK {
            return Err(Error::NoSpace);
        }
        let mut buf = vec![0u8; BS as usize];
        if dev.read_at(0, &mut buf).is_ok() {
            if let Ok(sb) = SuperV4::decode(&buf) {
                if sb.uuid == self.sb.uuid {
                    return Err(Error::Exists);
                }
            }
        }
        let vol = self.ensure_volume()?;
        let id = vol.with_map(|m| m.devices.keys().next_back().map_or(1, |d| d + 1));
        // Its superblock first, so a mount finds it whichever way the
        // commit below goes.
        dev.write_at(0, &sb_for_device(&self.sb.encode(), id))?;
        dev.sync()?;
        vol.with_map(|m| m.devices.insert(id, size));
        vol.attach(id, dev);
        self.sp.chunks_dirty = true;
        if let Err(e) = self.flush() {
            vol.detach(id);
            vol.with_map(|m| m.devices.remove(&id));
            return Err(e);
        }
        Ok(id)
    }

    /// Move everything off device `devid` and drop it (`btrfs device
    /// remove`); it may be one that is missing. What is left must be
    /// enough devices for every profile in use.
    pub fn remove_device(&mut self, devid: u32) -> Result<()> {
        self.writable()?;
        let vol = self.vol.clone().ok_or(Error::NotFound)?;
        let map = vol.map();
        if !map.devices.contains_key(&devid) {
            return Err(Error::NotFound);
        }
        let left = map.devices.len() - 1;
        let (d, m) = self.sp.profiles;
        let need = map
            .chunks
            .values()
            .map(|c| c.profile.min_devices())
            .chain([d.min_devices(), m.min_devices()])
            .max()
            .unwrap_or(1);
        if left == 0 || need > left {
            return Err(Error::Unsupported("too few devices left for the profiles"));
        }
        self.prepare_moves(&vol)?;
        if !self.sp.avoid.contains(&devid) {
            self.sp.avoid.push(devid);
        }
        let moved = (|| {
            for (g, c) in vol.map().chunks {
                if c.stripes.iter().any(|s| s.devid == devid) {
                    self.relocate(g, c.profile)?;
                }
            }
            Ok(())
        })();
        if let Err(e) = moved {
            if vol.device(devid).is_some() {
                self.sp.avoid.retain(|&x| x != devid);
            }
            return Err(e);
        }
        let dev = vol.detach(devid);
        vol.with_map(|m| m.devices.remove(&devid));
        self.sp.chunks_dirty = true;
        if let Err(e) = self.flush() {
            if let Some(dev) = dev {
                vol.attach(devid, dev);
            }
            return Err(e);
        }
        self.sp.avoid.retain(|&x| x != devid);
        if let Some(dev) = dev {
            // No longer one of ours.
            dev.write_at(0, &[0u8; BS as usize])?;
            dev.sync()?;
        }
        Ok(())
    }

    /// Put `new` in the place of device `devid` (`btrfs replace`), which
    /// may be missing.
    pub fn replace_device(&mut self, devid: u32, new: Arc<dyn BlockDevice>) -> Result<u32> {
        let vol = self.vol.clone().ok_or(Error::NotFound)?;
        if !vol.map().devices.contains_key(&devid) {
            return Err(Error::NotFound);
        }
        let id = self.add_device(new)?;
        if let Err(e) = self.remove_device(devid) {
            // Leave the filesystem as it was.
            let _ = self.remove_device(id);
            return Err(e);
        }
        Ok(id)
    }

    /// Rewrite every chunk (`btrfs balance`), converting data and metadata
    /// to new profiles if given; chunks with nothing in them are given
    /// back. Also spreads chunks over devices added since.
    pub fn balance(
        &mut self,
        data: Option<Profile>,
        meta: Option<Profile>,
    ) -> Result<BalanceReport> {
        self.writable()?;
        let vol = self.ensure_volume()?;
        let devices = self.present_devices(&vol);
        let d = data.unwrap_or(self.sp.profiles.0);
        let m = meta.unwrap_or(self.sp.profiles.1);
        if d.min_devices() > devices || m.min_devices() > devices {
            return Err(Error::Unsupported("not enough devices for that profile"));
        }
        self.sp.profiles = (d, m);
        self.sp.chunks_dirty = true;
        self.prepare_moves(&vol)?;
        let mut rep = BalanceReport::default();
        for (g, c) in vol.map().chunks {
            self.flush()?;
            let empty = self
                .sp
                .alloc
                .groups()
                .get(g as usize)
                .is_some_and(|grp| grp.free_blocks + 2 == grp.block_count);
            if g != 0 && empty {
                self.drop_chunk(g)?;
                rep.freed += 1;
                continue;
            }
            let target = if c.kind == ChunkKind::Data { d } else { m };
            self.relocate(g, target)?;
            rep.relocated += 1;
        }
        self.flush()?;
        Ok(rep)
    }

    /// Devices that missed commits get every used block of their copies
    /// from a device that did not (at a read-write mount).
    pub(super) fn resync_stale(&mut self) -> Result<()> {
        let Some(vol) = self.vol.clone() else {
            return Ok(());
        };
        let stale = vol.stale();
        if stale.is_empty() {
            return Ok(());
        }
        warn!(
            "eclipsefs: devices {:?} missed commits, resyncing them",
            stale
        );
        // Committed blocks only: nothing is open yet.
        for (g, c) in vol.map().chunks {
            if !c.stripes.iter().any(|s| stale.contains(&s.devid)) {
                continue;
            }
            let runs = self
                .sp
                .alloc
                .groups()
                .get(g as usize)
                .ok_or(Error::Corrupt("chunk without a group"))?
                .used_runs();
            for (start, n) in runs {
                vol.resync(&c, start, n)?;
            }
        }
        vol.sync()?;
        vol.set_stale(Vec::new());
        // Every superblock at the same generation again.
        self.dev.write_at(0, &self.sb.encode())?;
        self.flush()
    }

    /// Data and metadata profiles of new chunks.
    pub fn profiles(&self) -> (Profile, Profile) {
        self.sp.profiles
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        let Some(vol) = &self.vol else {
            return vec![DeviceInfo {
                devid: 1,
                size_bytes: self.raw.size(),
                allocated_bytes: self.sp.alloc.total_blocks() * BS,
                present: true,
            }];
        };
        let map = vol.map();
        let held = vol.held();
        let free = map.unallocated(&held);
        map.devices
            .iter()
            .map(|(&id, &size)| DeviceInfo {
                devid: id,
                size_bytes: size * BS,
                allocated_bytes: (size - free.get(&id).copied().unwrap_or(0)) * BS,
                present: vol.device(id).is_some(),
            })
            .collect()
    }

    pub fn chunks(&self) -> Vec<ChunkInfo> {
        let groups = self.sp.alloc.groups();
        let used = |g: u64| {
            groups
                .get(g as usize)
                .map(|grp| (grp.block_count - grp.free_blocks) * BS)
                .unwrap_or(0)
        };
        match &self.vol {
            None => groups
                .iter()
                .map(|grp| ChunkInfo {
                    logical: grp.start_block * BS,
                    kind: ChunkKind::Mixed,
                    profile: Profile::Single,
                    size_bytes: grp.block_count * BS,
                    used_bytes: used(grp.id as u64),
                    stripes: vec![(1, grp.start_block * BS)],
                })
                .collect(),
            Some(vol) => vol
                .map()
                .chunks
                .iter()
                .map(|(&g, c)| ChunkInfo {
                    logical: g * GROUP_BLOCKS * BS,
                    kind: c.kind,
                    profile: c.profile,
                    size_bytes: c.blocks * BS,
                    used_bytes: used(g),
                    stripes: c.stripes.iter().map(|s| (s.devid, s.phys * BS)).collect(),
                })
                .collect(),
        }
    }

    /// A volume's devices grew: their new sizes become usable.
    pub(super) fn grow_volume(&mut self, vol: &Volume) -> Result<bool> {
        let mut grew = false;
        for id in vol.present() {
            let Some(d) = vol.device(id) else { continue };
            let size = d.size() / BS;
            vol.with_map(|m| {
                if let Some(s) = m.devices.get_mut(&id) {
                    if size > *s {
                        *s = size;
                        grew = true;
                    }
                }
            });
        }
        if grew {
            self.sp.chunks_dirty = true;
            self.flush()?;
        }
        Ok(grew)
    }
}
