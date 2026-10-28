//! One GPU context per DRM file when a process drives the card through more
//! than one at once (`nvidia.ctx_per_file`, on by itself with
//! `nvidia.wlr_vulkan`).
//!
//! # Why
//!
//! Linux gives every open of the render node its own VM: `VM_INIT` builds a
//! uvmm in the file's `nouveau_cli`, and `VM_BIND`, `EXEC` and the channels
//! live in it. Here the VM, the RM context, its ring and its GR state are
//! keyed by pid (`nouveau_pid_ctx`, `nouveau_uvmm`). That is the same thing as
//! long as a process has one NVK device, which is what a GL client and labwc
//! on GLES2 have.
//!
//! labwc on Vulkan has two, both alive for the whole session: wlroots' own
//! `VkDevice`, and the one zink makes under GBM for the allocator. NVK hands
//! out GPU VAs from its own heap, the same addresses in both devices, and
//! keeps its channel state (shader heap, descriptor pools, local memory) per
//! device. Sharing one context, the second device's binds replaced the
//! first's mappings and its state replaced the first's on the one GR context.
//! wlroots' first frame then hung the ring, and every frame after it was
//! `VK_ERROR_DEVICE_LOST`.
//!
//! # How
//!
//! Every file that touches a context gets a key, and the key is what the
//! context code calls the pid. A file takes the process's own pid when no
//! other file of the process holds it, so a process with one device runs
//! exactly as before. A second file active at the same time gets a key of its
//! own ([`key_for`](Registry::key_for)): a pid with [`KEY_BIT`] and a serial
//! above it. What belongs to the process and not to the VM (GEM objects,
//! syncobjs, PRIME holds) stays the process's: [`pid_of`] is how those checks
//! read a key.
//!
//! NVK opens a throwaway context on each file it enumerates with (a channel
//! allocated and freed to read the engine classes) and keeps the file open.
//! A file whose last channel is gone and that never bound memory gives its key
//! back, so the pdev's file does not hold the pid for the life of the
//! instance, and a key of its own is torn down right there.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;

use super::NvidiaGpu;
use crate::display::nouveau_uapi as nv;

/// Set on every key that is not a plain pid. Below the zombie tombstones
/// (`1 << 62`) and every holder id `gem_mmap` reserves; the pid stays in the
/// low 32 bits, so `ctx_alloc`'s `owner_pid as u32` still files the RM client
/// under the real pid for `nvidia-smi`.
pub(super) const KEY_BIT: u64 = 1 << 61;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn the per-file contexts on (`nvidia.ctx_per_file`, `nvidia.wlr_vulkan`).
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The process a key belongs to. A plain pid, a tombstone or 0 is itself.
pub(super) fn pid_of(key: u64) -> u64 {
    if key & KEY_BIT != 0 && key < 1 << 62 {
        key & 0xffff_ffff
    } else {
        key
    }
}

/// The ioctls whose state lives in the VM, and so in the file's context. GEM
/// objects are the process's (`GEM_NEW`, `GEM_INFO`, `CPU_PREP`): they keep
/// the pid.
pub(super) fn routes_by_file(request: u32) -> bool {
    (request >> 8) & 0xff == 0x64
        && matches!(
            request & 0xff,
            nv::NR_VM_INIT
                | nv::NR_VM_BIND
                | nv::NR_EXEC
                | nv::NR_CHANNEL_ALLOC
                | nv::NR_CHANNEL_FREE
                | nv::NR_NVIF
        )
}

struct Entry {
    file: usize,
    pid: u64,
    /// 0 while the file holds no key (it gave it back, idle).
    key: u64,
    /// What its `VM_INIT` reserved for the kernel, replayed into the uvmm
    /// of every key the file is given later.
    vm_init: Option<(u64, u64)>,
    channels: u32,
    bound: bool,
}

/// The files of one card, by the address of their `DrmFileState`.
pub(super) struct Registry {
    entries: Mutex<Vec<Entry>>,
    /// Keys of closed files, for the next caller to tear down outside the
    /// file's `Drop`.
    closed: Mutex<Vec<u64>>,
    serial: Mutex<u64>,
}

impl Registry {
    pub(super) fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            serial: Mutex::new(0),
        }
    }

    /// The key `file` of `pid` works under, and, when the key was just given,
    /// the `VM_INIT` range its uvmm needs.
    pub(super) fn key_for(&self, file: usize, pid: u64) -> (u64, Option<(u64, u64)>) {
        let mut entries = self.entries.lock();
        let pid_held = entries.iter().any(|e| e.pid == pid && e.key == pid);
        let i = match entries.iter().position(|e| e.file == file && e.pid == pid) {
            Some(i) => i,
            None => {
                entries.push(Entry {
                    file,
                    pid,
                    key: 0,
                    vm_init: None,
                    channels: 0,
                    bound: false,
                });
                entries.len() - 1
            }
        };
        if entries[i].key != 0 {
            return (entries[i].key, None);
        }
        let key = if pid_held {
            let mut serial = self.serial.lock();
            *serial = (*serial + 1) & 0x1fff_ffff;
            if *serial == 0 {
                *serial = 1;
            }
            KEY_BIT | (*serial << 32) | (pid & 0xffff_ffff)
        } else {
            pid
        };
        entries[i].key = key;
        (key, entries[i].vm_init)
    }

    pub(super) fn note_vm_init(&self, file: usize, pid: u64, range: (u64, u64)) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.iter_mut().find(|e| e.file == file && e.pid == pid) {
            e.vm_init.get_or_insert(range);
        }
    }

    pub(super) fn note_bound(&self, file: usize, pid: u64) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.iter_mut().find(|e| e.file == file && e.pid == pid) {
            e.bound = true;
        }
    }

    pub(super) fn note_channel_alloc(&self, file: usize, pid: u64) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.iter_mut().find(|e| e.file == file && e.pid == pid) {
            e.channels += 1;
        }
    }

    /// A channel of `file` went. When it was the last one and the file never
    /// bound memory, the file gives its key back; a key of its own comes
    /// back to the caller to tear down.
    pub(super) fn note_channel_free(&self, file: usize, pid: u64) -> Option<u64> {
        let mut entries = self.entries.lock();
        let e = entries
            .iter_mut()
            .find(|e| e.file == file && e.pid == pid)?;
        e.channels = e.channels.saturating_sub(1);
        if e.channels != 0 || e.bound || e.key == 0 {
            return None;
        }
        let key = core::mem::replace(&mut e.key, 0);
        (key != pid).then_some(key)
    }

    /// The file closed (`DrmFileState::drop`). Its key is torn down by the
    /// next [`take_closed`](Self::take_closed), not here.
    pub(super) fn file_closed(&self, file: usize) {
        let gone: Vec<Entry> = {
            let mut entries = self.entries.lock();
            let (gone, kept) = core::mem::take(&mut *entries)
                .into_iter()
                .partition(|e| e.file == file);
            *entries = kept;
            gone
        };
        let mut closed = self.closed.lock();
        closed.extend(gone.iter().filter(|e| e.key & KEY_BIT != 0).map(|e| e.key));
    }

    pub(super) fn take_closed(&self) -> Vec<u64> {
        core::mem::take(&mut *self.closed.lock())
    }

    /// The process exited: every key of its own, its files forgotten.
    pub(super) fn take_pid(&self, pid: u64) -> Vec<u64> {
        let mut keys: Vec<u64> = Vec::new();
        self.entries.lock().retain(|e| {
            if e.pid != pid {
                return true;
            }
            if e.key & KEY_BIT != 0 {
                keys.push(e.key);
            }
            false
        });
        self.closed.lock().retain(|&k| {
            if pid_of(k) != pid {
                return true;
            }
            keys.push(k);
            false
        });
        keys
    }
}

impl NvidiaGpu {
    /// `ioctl_owned` for an ioctl of the DRM file at `file`: a VM ioctl runs
    /// under the file's key when per-file contexts are on, anything else under
    /// the pid as always.
    pub(super) fn ioctl_for_file(
        &self,
        request: u32,
        arg: usize,
        pid: u64,
        file: usize,
    ) -> Result<usize, i32> {
        if !enabled() || pid == 0 || file == 0 || !routes_by_file(request) {
            return crate::scheme::DrmScheme::ioctl_owned(self, request, arg, pid);
        }
        for key in self.file_ctx.take_closed() {
            self.release_file_key(key, "its file closed");
        }
        let (key, replay) = self.file_ctx.key_for(file, pid);
        if let Some((addr, size)) = replay {
            self.replay_vm_init(key, addr, size);
        }
        if key != pid {
            static FIRST: AtomicBool = AtomicBool::new(false);
            if !FIRST.swap(true, Ordering::Relaxed) {
                crate::klog_warn!(
                    "[nouveau-uapi] pid={} drives this card from a second DRM file at once \
                     (two NVK devices): that file gets a GPU context of its own, key={:#x}, \
                     as Linux gives each file its own VM",
                    pid,
                    key
                );
            }
        }
        let r = crate::scheme::DrmScheme::ioctl_owned(self, request, arg, key);
        if r.is_ok() {
            match request & 0xff {
                nv::NR_VM_INIT => {
                    // The arm read the struct, so it is the caller's.
                    let req = unsafe { &*(arg as *const nv::DrmNouveauVmInit) };
                    self.file_ctx.note_vm_init(
                        file,
                        pid,
                        (req.kernel_managed_addr, req.kernel_managed_size),
                    );
                }
                nv::NR_VM_BIND => self.file_ctx.note_bound(file, pid),
                nv::NR_CHANNEL_ALLOC => self.file_ctx.note_channel_alloc(file, pid),
                nv::NR_CHANNEL_FREE => {
                    if let Some(own) = self.file_ctx.note_channel_free(file, pid) {
                        self.release_file_key(own, "its last channel went and it bound nothing");
                    }
                }
                _ => {}
            }
        }
        r
    }

    /// A key that was given back before is given again: its uvmm comes back
    /// with the range the file's `VM_INIT` asked for.
    fn replay_vm_init(&self, key: u64, addr: u64, size: u64) {
        let mut uvmm = self.nouveau_uvmm.lock();
        if !uvmm.iter().any(|u| u.pid == key) {
            uvmm.push(super::NouveauUvmm {
                pid: key,
                kernel_managed_addr: addr,
                kernel_managed_size: size,
            });
        }
    }

    /// Tear down a file's own context: what a process exit does, for the key.
    fn release_file_key(&self, key: u64, why: &str) {
        crate::klog_info!(
            "[nouveau-uapi] GPU context of key={:#x} (pid={}) released: {}",
            key,
            pid_of(key),
            why
        );
        crate::scheme::DrmScheme::nouveau_release_process(self, key);
    }

    /// The process exited: its files' own contexts go first, while the
    /// process's GEM memory (in the pid's context) is still there for them to
    /// be unmapped from.
    pub(super) fn release_file_keys(&self, pid: u64) {
        if pid & KEY_BIT != 0 {
            return;
        }
        for key in self.file_ctx.take_pid(pid) {
            self.release_file_key(key, "process exit");
        }
    }

    /// `DrmFileState::drop`.
    pub(super) fn file_ctx_closed(&self, file: usize) {
        self.file_ctx.file_closed(file);
    }
}

#[cfg(test)]
mod tests;
