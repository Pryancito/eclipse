extern crate std;

use super::super::rm_host_shims::{reset_fake_rm, FAKE_RM};
use super::*;
use crate::display::nouveau_uapi as nv;
use crate::test_globals::LOCK;
use core::mem::size_of;

const P: u64 = 67_001;
const COMP: u64 = 67_000;
const F1: usize = 0x1000;
const F2: usize = 0x2000;
const VA: u64 = 1 << 33;
const MIB: u64 = 1 << 20;

/// Turns the hatch on for one test and off again, panic or not.
struct Hatch;

impl Hatch {
    fn on() -> Self {
        set_enabled(true);
        Self
    }
}

impl Drop for Hatch {
    fn drop(&mut self) {
        set_enabled(false);
    }
}

fn wr<T>(nr: u32) -> u32 {
    (3 << 30) | ((size_of::<T>() as u32) << 16) | (0x64 << 8) | nr
}

/// An attached GPU whose clients can get contexts, with a compositor that is
/// someone else.
fn gpu() -> NvidiaGpu {
    nv::set_enabled(true);
    let gpu = NvidiaGpu::for_test(0x1f06, 8192);
    reset_fake_rm();
    nv::set_compositor_ctx0(false);
    *gpu.rm_device_instance.lock() = Some(0);
    FAKE_RM.lock().ladder_built = true;
    gpu.ctx0_owner.store(COMP, Ordering::Release);
    gpu
}

fn call<T>(gpu: &NvidiaGpu, nr: u32, req: &mut T, file: usize) -> Result<usize, i32> {
    gpu.ioctl_for_file(wr::<T>(nr), req as *mut T as usize, P, file)
}

fn vm_init(gpu: &NvidiaGpu, file: usize) {
    let mut r = nv::DrmNouveauVmInit {
        kernel_managed_addr: 0,
        kernel_managed_size: 1 << 32,
    };
    assert_eq!(call(gpu, nv::NR_VM_INIT, &mut r, file), Ok(0));
}

fn gem_new(gpu: &NvidiaGpu, file: usize) -> u32 {
    let mut r = nv::DrmNouveauGemNew {
        info: nv::DrmNouveauGemInfo {
            handle: 0,
            domain: nv::NOUVEAU_GEM_DOMAIN_GART,
            size: MIB,
            offset: 0,
            map_handle: 0,
            tile_mode: 0,
            tile_flags: 0,
        },
        channel_hint: 0,
        align: 0,
    };
    assert_eq!(call(gpu, nv::NR_GEM_NEW, &mut r, file), Ok(0));
    r.info.handle
}

fn map(gpu: &NvidiaGpu, file: usize, handle: u32) -> Result<usize, i32> {
    let mut op = nv::DrmNouveauVmBindOp {
        op: nv::VM_BIND_OP_MAP,
        flags: nv::PTE_KIND_GENERIC,
        handle,
        pad: 0,
        addr: VA,
        bo_offset: 0,
        range: MIB,
    };
    let mut r = nv::DrmNouveauVmBind {
        op_count: 1,
        flags: 0,
        wait_count: 0,
        sig_count: 0,
        wait_ptr: 0,
        sig_ptr: 0,
        op_ptr: &mut op as *mut _ as u64,
    };
    call(gpu, nv::NR_VM_BIND, &mut r, file)
}

fn channel_alloc(gpu: &NvidiaGpu, file: usize) -> i32 {
    let mut r = nv::DrmNouveauChannelAlloc {
        fb_ctxdma_handle: 0,
        tt_ctxdma_handle: 0,
        channel: -1,
        pushbuf_domains: 0,
        notifier_handle: 0xffff_ffff,
        subchan: [nv::DrmNouveauChannelAllocSubchan {
            handle: 0,
            grclass: 0,
        }; 8],
        nr_subchan: 7,
    };
    assert_eq!(call(gpu, nv::NR_CHANNEL_ALLOC, &mut r, file), Ok(0));
    r.channel
}

fn channel_free(gpu: &NvidiaGpu, file: usize, channel: i32) {
    let mut r = nv::DrmNouveauChannelFree { channel };
    assert_eq!(call(gpu, nv::NR_CHANNEL_FREE, &mut r, file), Ok(0));
}

/// `(owner, ctx)` of every mapping of `handle`.
fn mappings(gpu: &NvidiaGpu, handle: u32) -> Vec<(u64, u32)> {
    gpu.nouveau_vm_mappings
        .lock()
        .iter()
        .filter(|m| m.gem_handle == handle)
        .map(|m| (m.owner_pid, m.ctx_idx))
        .collect()
}

fn channel_ctx(gpu: &NvidiaGpu, id: i32) -> u32 {
    let chans = gpu.nouveau_channels.lock();
    chans.iter().find(|c| c.id == id).unwrap().ctx_idx
}

#[test]
fn a_key_reads_back_as_its_pid_and_nothing_else_moves() {
    let key = KEY_BIT | (3 << 32) | 4242;
    assert_eq!(pid_of(key), 4242);
    assert_eq!(pid_of(4242), 4242);
    assert_eq!(pid_of(0), 0);
    let tomb = (1 << 62) | 7;
    assert_eq!(pid_of(tomb), tomb, "a zombie's tombstone is not a key");
}

#[test]
fn the_first_file_takes_the_pid_and_a_second_one_at_once_gets_its_own() {
    let r = Registry::new();
    assert_eq!(r.key_for(F1, P), (P, None));
    r.note_bound(F1, P);
    let (k2, _) = r.key_for(F2, P);
    assert_ne!(k2, P);
    assert_eq!(pid_of(k2), P);
    assert_eq!(r.key_for(F1, P).0, P, "a file keeps its key");
    assert_eq!(r.key_for(F2, P).0, k2);
    assert_eq!(
        r.key_for(F1, 99).0,
        99,
        "another process is not a second file"
    );
}

#[test]
fn a_throwaway_channel_gives_the_pid_back_to_the_next_file() {
    let r = Registry::new();
    assert_eq!(r.key_for(F1, P).0, P);
    r.note_vm_init(F1, P, (0, 1 << 32));
    r.note_channel_alloc(F1, P);
    assert_eq!(
        r.note_channel_free(F1, P),
        None,
        "the pid itself is not torn down"
    );
    assert_eq!(
        r.key_for(F2, P).0,
        P,
        "NVK's device file runs as the process"
    );
    // The enumeration file comes back: it is the second file now, and its
    // VM_INIT comes with it.
    let (k1, replay) = r.key_for(F1, P);
    assert_ne!(k1, P);
    assert_eq!(replay, Some((0, 1 << 32)));
}

#[test]
fn a_second_files_idle_key_and_closed_keys_come_back_for_teardown() {
    let r = Registry::new();
    r.key_for(F1, P);
    r.note_bound(F1, P);
    let (k2, _) = r.key_for(F2, P);
    r.note_channel_alloc(F2, P);
    assert_eq!(r.note_channel_free(F2, P), Some(k2));
    let (k3, _) = r.key_for(F2, P);
    r.note_bound(F2, P);
    r.note_channel_alloc(F2, P);
    assert_eq!(
        r.note_channel_free(F2, P),
        None,
        "it bound memory: it stays"
    );
    r.file_closed(F2);
    assert_eq!(r.take_closed(), [k3]);
    assert!(r.take_closed().is_empty());
}

#[test]
fn process_exit_hands_back_every_key_of_its_own() {
    let r = Registry::new();
    r.key_for(F1, P);
    r.note_bound(F1, P);
    let (k2, _) = r.key_for(F2, P);
    r.note_bound(F2, P);
    let (k3, _) = r.key_for(0x3000, P);
    r.file_closed(0x3000);
    let mut keys = r.take_pid(P);
    keys.sort();
    assert_eq!(keys, [k2, k3]);
    assert_eq!(r.key_for(F2, P).0, P, "nothing of the dead process is left");
}

#[test]
fn two_devices_of_one_process_bind_the_same_va_in_two_contexts() {
    let _g = LOCK.lock();
    let _h = Hatch::on();
    let gpu = gpu();
    vm_init(&gpu, F1);
    vm_init(&gpu, F2);
    let h1 = gem_new(&gpu, F1);
    let h2 = gem_new(&gpu, F2);
    assert_eq!(map(&gpu, F1, h1), Ok(0));
    assert_eq!(map(&gpu, F2, h2), Ok(0));
    let m1 = mappings(&gpu, h1);
    let m2 = mappings(&gpu, h2);
    assert_eq!(
        m1.len(),
        1,
        "the second device's bind did not replace the first's"
    );
    assert_eq!(m2.len(), 1);
    assert_eq!(m1[0].0, P, "the first device runs as the process");
    assert_eq!(pid_of(m2[0].0), P);
    assert_ne!(m1[0].1, m2[0].1, "and each has a VA space of its own");
    let c1 = channel_alloc(&gpu, F1);
    let c2 = channel_alloc(&gpu, F2);
    assert_eq!(
        channel_ctx(&gpu, c1),
        m1[0].1,
        "each channel runs in its device's VM"
    );
    assert_eq!(channel_ctx(&gpu, c2), m2[0].1);
    // The objects stay the process's: both are its own to close.
    for h in [h1, h2] {
        let o = gpu
            .nouveau_gem
            .lock()
            .iter()
            .find(|o| o.handle == h)
            .unwrap()
            .owner_pid;
        assert_eq!(o, P);
    }
    crate::scheme::DrmScheme::nouveau_release_process(&gpu, P);
    assert!(gpu.nouveau_channels.lock().is_empty());
    assert!(gpu.nouveau_vm_mappings.lock().is_empty());
    assert!(gpu.nouveau_pid_ctx.lock().is_empty());
    assert!(gpu.nouveau_uvmm.lock().is_empty());
}

#[test]
fn a_closed_second_file_takes_its_context_with_it_on_the_next_ioctl() {
    let _g = LOCK.lock();
    let _h = Hatch::on();
    let gpu = gpu();
    vm_init(&gpu, F1);
    vm_init(&gpu, F2);
    let h1 = gem_new(&gpu, F1);
    let h2 = gem_new(&gpu, F2);
    assert_eq!(map(&gpu, F1, h1), Ok(0));
    assert_eq!(map(&gpu, F2, h2), Ok(0));
    let ctx2 = mappings(&gpu, h2)[0].1;
    gpu.file_ctx_closed(F2);
    let c = channel_alloc(&gpu, F1);
    assert!(mappings(&gpu, h2).is_empty(), "its VM went with it");
    assert!(gpu.nouveau_pid_ctx.lock().iter().all(|t| t.1 != ctx2));
    assert_eq!(
        mappings(&gpu, h1).len(),
        1,
        "the other device keeps its own"
    );
    channel_free(&gpu, F1, c);
    crate::scheme::DrmScheme::nouveau_release_process(&gpu, P);
}

#[test]
fn with_the_hatch_off_every_file_of_a_process_is_the_process() {
    let _g = LOCK.lock();
    let gpu = gpu();
    vm_init(&gpu, F1);
    vm_init(&gpu, F2);
    let h1 = gem_new(&gpu, F1);
    let h2 = gem_new(&gpu, F2);
    assert_eq!(map(&gpu, F1, h1), Ok(0));
    assert_eq!(map(&gpu, F2, h2), Ok(0));
    assert!(
        mappings(&gpu, h1).is_empty(),
        "one VM: the last binder wins, as before"
    );
    assert_eq!(mappings(&gpu, h2), [(P, mappings(&gpu, h2)[0].1)]);
    crate::scheme::DrmScheme::nouveau_release_process(&gpu, P);
}
