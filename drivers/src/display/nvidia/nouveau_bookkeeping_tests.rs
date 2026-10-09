//! The nouveau-uAPI arms that are bookkeeping rather than hardware, on
//! a GPU the RM never attached: the state every ioctl sees before
//! `/proc/gpustep14`, and the only state a host test can reach. Every
//! test takes `LOCK`: the uAPI switch, the live-bytes counter and the
//! `gem_mmap` registry are process globals.

extern crate std;

use super::super::nouveau_uapi as nv;
use super::*;
use core::mem::size_of;
use lock::Mutex as TestMutex;

static LOCK: TestMutex<()> = TestMutex::new(());

const A: u64 = 66_001;
const B: u64 = 66_002;
const STRANGER: u64 = 66_003;
const MIB: u64 = 1024 * 1024;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const DRM_IOCTL_TYPE: u32 = 0x64;

fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (ty << 8) | nr
}

fn wr<T>(nr: u32) -> u32 {
    ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_TYPE, nr, size_of::<T>())
}

fn gpu() -> NvidiaGpu {
    nv::set_enabled(true);
    NvidiaGpu::for_test(0x1f06, 8192)
}

fn call<T>(gpu: &NvidiaGpu, request: u32, req: &mut T, pid: u64) -> Result<usize, i32> {
    gpu.nouveau_ioctl(request, req as *mut T as usize, pid)
}

fn getparam(gpu: &NvidiaGpu, param: u64) -> Result<u64, i32> {
    let mut r = nv::DrmNouveauGetparam { param, value: 0 };
    call(
        gpu,
        wr::<nv::DrmNouveauGetparam>(nv::NR_GETPARAM),
        &mut r,
        A,
    )
    .map(|_| r.value)
}

fn gem_new(gpu: &NvidiaGpu, size: u64, domain: u32, pid: u64) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauGemNew {
        info: nv::DrmNouveauGemInfo {
            handle: 0,
            domain,
            size,
            offset: 0,
            map_handle: 0,
            tile_mode: 0,
            tile_flags: 0,
        },
        channel_hint: 0,
        align: 0,
    };
    call(gpu, wr::<nv::DrmNouveauGemNew>(nv::NR_GEM_NEW), &mut r, pid)
}

fn gem_info(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<nv::DrmNouveauGemInfo, i32> {
    let mut r = nv::DrmNouveauGemInfo {
        handle,
        domain: 0,
        size: 0,
        offset: 0,
        map_handle: 0,
        tile_mode: 0,
        tile_flags: 0,
    };
    call(
        gpu,
        wr::<nv::DrmNouveauGemInfo>(nv::NR_GEM_INFO),
        &mut r,
        pid,
    )
    .map(|_| r)
}

fn cpu_fini(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauGemCpuFini { handle };
    call(
        gpu,
        wr::<nv::DrmNouveauGemCpuFini>(nv::NR_GEM_CPU_FINI),
        &mut r,
        pid,
    )
}

fn cpu_prep_nowait(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<usize, i32> {
    cpu_prep_flags(gpu, handle, nv::NOUVEAU_GEM_CPU_PREP_NOWAIT, pid)
}

fn cpu_prep_flags(gpu: &NvidiaGpu, handle: u32, flags: u32, pid: u64) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauGemCpuPrep { handle, flags };
    call(
        gpu,
        wr::<nv::DrmNouveauGemCpuPrep>(nv::NR_GEM_CPU_PREP),
        &mut r,
        pid,
    )
}

fn channel_alloc(gpu: &NvidiaGpu, pid: u64) -> Result<nv::DrmNouveauChannelAlloc, i32> {
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
    call(
        gpu,
        wr::<nv::DrmNouveauChannelAlloc>(nv::NR_CHANNEL_ALLOC),
        &mut r,
        pid,
    )
    .map(|_| r)
}

fn channel_free(gpu: &NvidiaGpu, channel: i32, pid: u64) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauChannelFree { channel };
    call(
        gpu,
        wr::<nv::DrmNouveauChannelFree>(nv::NR_CHANNEL_FREE),
        &mut r,
        pid,
    )
}

/// `VM_INIT` as NVK issues it first thing (`nouveau_ws_device_alloc`):
/// the kernel-managed range is the low 4 GiB there.
fn vm_init(gpu: &NvidiaGpu, pid: u64) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauVmInit {
        kernel_managed_addr: 0,
        kernel_managed_size: 1 << 32,
    };
    call(gpu, wr::<nv::DrmNouveauVmInit>(nv::NR_VM_INIT), &mut r, pid)
}

/// The VM_BIND/EXEC helpers below run `VM_INIT` for the client first,
/// as every real client has by then; the tests that look at a client
/// WITHOUT one call the ioctls directly.
fn with_uvmm(gpu: &NvidiaGpu, pid: u64) {
    assert_eq!(vm_init(gpu, pid), Ok(0));
}

fn vm_bind(gpu: &NvidiaGpu, pid: u64) -> Result<usize, i32> {
    with_uvmm(gpu, pid);
    let mut r = nv::DrmNouveauVmBind {
        op_count: 1,
        flags: 0,
        wait_count: 0,
        sig_count: 0,
        wait_ptr: 0,
        sig_ptr: 0,
        op_ptr: 0x1000,
    };
    call(gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, pid)
}

/// Restores the global live-bytes counter, panic or not, so one test's
/// objects never count against another's quota.
struct LiveBytes(u64);

impl LiveBytes {
    fn hold() -> Self {
        Self(NOUVEAU_GEM_BYTES.load(Ordering::Relaxed))
    }
    fn delta(&self) -> i64 {
        NOUVEAU_GEM_BYTES.load(Ordering::Relaxed) as i64 - self.0 as i64
    }
}

impl Drop for LiveBytes {
    fn drop(&mut self) {
        NOUVEAU_GEM_BYTES.store(self.0, Ordering::Relaxed);
    }
}

/// A GEM object as `GEM_NEW` would have left it: in the table, counted,
/// and registered with `gem_mmap` when it has a CPU mapping.
fn object(gpu: &NvidiaGpu, owner: u64, size: u64, phys: Option<u64>) -> u32 {
    let handle = gpu.next_gem_handle().unwrap();
    gpu.nouveau_gem.lock().push(nv::NouveauGemObject {
        handle,
        h_memory: 0xcafe_0000 | (handle & 0xffff),
        owner_pid: owner,
        size,
        phys_addr: phys,
        vram_offset: None,
        domain: nv::NOUVEAU_GEM_DOMAIN_GART,
        tile_mode: 0x10,
        tile_flags: 0x600,
    });
    NOUVEAU_GEM_BYTES.fetch_add(size, Ordering::Relaxed);
    if let Some(pa) = phys {
        crate::scheme::gem_mmap::register(handle, pa, size, owner);
    }
    handle
}

/// A VRAM-domain object with no CPU aperture, like a tiled render
/// target NVK allocates.
fn vram_object(gpu: &NvidiaGpu, owner: u64, size: u64) -> u32 {
    let handle = object(gpu, owner, size, None);
    let mut gem = gpu.nouveau_gem.lock();
    let obj = gem.iter_mut().find(|o| o.handle == handle).unwrap();
    obj.domain = nv::NOUVEAU_GEM_DOMAIN_VRAM;
    obj.vram_offset = Some(0x100_0000 * u64::from(handle & 0xff));
    handle
}

/// `gem_aperture` is the only thing that tells a VRAM GEM apart from a
/// handle nobody has heard of, once `resolve_gem_backing_for` has
/// answered `None` for both. The DRM layer prints opposite advice on the
/// strength of it, so it has to read `phys_addr` and not `domain`:
/// whether the CPU can reach the pages is what the caller is asking.
#[test]
fn a_vram_gem_reads_as_vram_and_a_gart_one_as_sysmem() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();

    let gart = object(&gpu, A, MIB, Some(0x8000_0000));
    let vram = vram_object(&gpu, A, MIB);

    assert_eq!(gpu.gem_aperture(gart), GemAperture::Sysmem);
    assert_eq!(gpu.gem_aperture(vram), GemAperture::Vram);
}

/// A handle the driver has never seen is `Unknown`, not `Vram`. Reading
/// a miss as device memory would print the VRAM advice for an ordinary
/// use-after-close and send the reader to the wrong half of the kernel.
#[test]
fn a_handle_the_driver_never_made_is_unknown() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    let real = object(&gpu, A, MIB, Some(0x8000_0000));

    assert_eq!(
        gpu.gem_aperture(real.wrapping_add(0x1_0000)),
        GemAperture::Unknown
    );
}

fn mapping(gpu: &NvidiaGpu, owner: u64, gem_handle: u32, va: u64, size: u64) {
    gpu.nouveau_vm_mappings.lock().push(nv::NouveauVmMapping {
        gem_handle,
        h_virt: 0xbeef,
        owner_pid: owner,
        va,
        size,
        bo_offset: 0,
        pte_kind: 0,
    });
}

fn framebuffer(gpu: &NvidiaGpu, handle_id: u32) -> u32 {
    let id = gpu.next_kms_fb_id.fetch_add(1, Ordering::Relaxed);
    gpu.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
        id,
        handle_id,
        width: 64,
        height: 64,
        pitch: 256,
        phys_addr: 0,
        size: 16384,
        h_memory: 0,
        vram_offset: None,
    });
    id
}

fn has_object(gpu: &NvidiaGpu, handle: u32) -> bool {
    gpu.nouveau_gem.lock().iter().any(|o| o.handle == handle)
}

fn owner_of(gpu: &NvidiaGpu, handle: u32) -> Option<u64> {
    gpu.nouveau_gem
        .lock()
        .iter()
        .find(|o| o.handle == handle)
        .map(|o| o.owner_pid)
}

fn mappings_of(gpu: &NvidiaGpu, handle: u32) -> usize {
    gpu.nouveau_vm_mappings
        .lock()
        .iter()
        .filter(|m| m.gem_handle == handle)
        .count()
}

fn channels_of(gpu: &NvidiaGpu, pid: u64) -> usize {
    gpu.nouveau_channels
        .lock()
        .iter()
        .filter(|c| c.owner_pid == pid)
        .count()
}

#[test]
fn the_dispatch_gates_on_the_switch_the_type_and_the_payload_floor() {
    let _g = LOCK.lock();
    let gpu = gpu();
    nv::set_enabled(false);
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_VENDOR),
        Err(nv::ENOSYS),
        "off by default: byte for byte the old NvidiaGpu::ioctl"
    );
    nv::set_enabled(true);
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_VENDOR), Ok(0x10de));
    let mut r = nv::DrmNouveauGetparam {
        param: nv::NOUVEAU_GETPARAM_PCI_VENDOR,
        value: 0,
    };
    // Not the DRM ioctl type at all.
    assert_eq!(
        call(
            &gpu,
            ioc(IOC_READ | IOC_WRITE, 0x63, nv::NR_GETPARAM, 16),
            &mut r,
            A
        ),
        Err(nv::ENOSYS)
    );
    // A caller whose struct is shorter than the one the arm writes.
    assert_eq!(
        call(
            &gpu,
            ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_TYPE, nv::NR_GETPARAM, 8),
            &mut r,
            A
        ),
        Err(nv::EINVAL)
    );
    // The direction bits are advisory: Linux dispatches by NR alone.
    assert_eq!(
        call(
            &gpu,
            ioc(IOC_WRITE, DRM_IOCTL_TYPE, nv::NR_GETPARAM, 16),
            &mut r,
            A
        ),
        Ok(0)
    );
    assert_eq!(r.value, 0x10de);
    // An NR nouveau never published.
    assert_eq!(
        call(
            &gpu,
            ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_TYPE, 0x40 + 0x50, 16),
            &mut r,
            A
        ),
        Err(nv::ENOSYS)
    );
}

#[test]
fn getparam_enumerates_the_gpu_without_the_rm() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_VENDOR), Ok(0x10de));
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_DEVICE), Ok(0x1f06));
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_BUS_TYPE), Ok(2));
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_FB_SIZE), Ok(8192 * MIB));
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_BAR_SIZE),
        Ok(256 * MIB),
        "the BAR1 aperture, not the board's VRAM"
    );
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_AGP_SIZE), Ok(0));
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_CHIPSET_ID),
        Ok(0x162),
        "BAR0 reads as zero, so the architecture's flagship chip stands in"
    );
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_HAS_VMA_TILEMODE), Ok(1));
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_HAS_PAGEFLIP), Ok(0));
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_EXEC_PUSH_MAX), Ok(64));
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
        Ok(6 | (36 << 8)),
        "no RM: the full TU102 die, gpc in the low byte, tpc above"
    );
    assert_eq!(getparam(&gpu, 99), Err(nv::EINVAL));
    // A GPU the id table does not know still reports VRAM: the
    // architecture floor, never zero (NVK would skip it).
    let unknown = NvidiaGpu::for_test(0x1fff, 0);
    assert_eq!(
        getparam(&unknown, nv::NOUVEAU_GETPARAM_FB_SIZE),
        Ok(4096 * MIB)
    );
}

#[test]
fn gem_new_is_refused_for_its_own_reason_before_it_needs_the_rm() {
    let _g = LOCK.lock();
    let live = LiveBytes::hold();
    let gpu = gpu();
    let gart = nv::NOUVEAU_GEM_DOMAIN_GART;
    assert_eq!(gem_new(&gpu, 4096, 0, A), Err(nv::EOPNOTSUPP));
    assert_eq!(gem_new(&gpu, 0, gart, A), Err(nv::EINVAL));
    assert_eq!(gem_new(&gpu, u32::MAX as u64 + 1, gart, A), Err(nv::EINVAL));
    assert_eq!(
        gem_new(&gpu, GEM_NEW_MAX_SINGLE + 1, gart, A),
        Err(nv::ENOMEM),
        "single-allocation cap"
    );
    assert_eq!(gem_new(&gpu, GEM_NEW_MAX_SINGLE, gart, A), Err(nv::ENODEV));
    // Per-pid quota: what A already holds plus this request.
    object(&gpu, A, GEM_NEW_MAX_PER_PID - 4096, None);
    assert_eq!(gem_new(&gpu, 8192, gart, A), Err(nv::ENOMEM));
    assert_eq!(gem_new(&gpu, 4096, gart, A), Err(nv::ENODEV));
    assert_eq!(
        gem_new(&gpu, 8192, gart, B),
        Err(nv::ENODEV),
        "B's quota is B's"
    );
    // Global quota: min(4 GiB, twice the VRAM), across every pid.
    object(&gpu, B, GEM_NEW_MAX_PER_PID, None);
    assert_eq!(gem_new(&gpu, 8192, gart, STRANGER), Err(nv::ENOMEM));
    assert_eq!(gem_new(&gpu, 4096, gart, STRANGER), Err(nv::ENODEV));
    // Twice a 1 GiB board is the tighter cap.
    let small = NvidiaGpu::for_test(0x1f06, 1024);
    NOUVEAU_GEM_BYTES.store(2048 * MIB - 4096, Ordering::Relaxed);
    assert_eq!(gem_new(&small, 8192, gart, STRANGER), Err(nv::ENOMEM));
    assert_eq!(gem_new(&small, 4096, gart, STRANGER), Err(nv::ENODEV));
    // A request the RM never saw burns no handle.
    let next = gpu.nouveau_gem_next_handle.load(Ordering::Relaxed);
    assert_eq!(gem_new(&gpu, 4096, gart, STRANGER), Err(nv::ENODEV));
    assert_eq!(gpu.nouveau_gem_next_handle.load(Ordering::Relaxed), next);
    drop(live);
}

#[test]
fn a_gem_object_is_seen_by_its_creator_a_prime_holder_and_the_kernel() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    let h1 = object(&gpu, A, 65536, Some(0x1000_0000));
    let h2 = object(&gpu, A, 4096, None);
    mapping(&gpu, A, h1, 0x4000_0000, 65536);

    let info = gem_info(&gpu, h1, A).unwrap();
    assert_eq!(info.size, 65536);
    assert_eq!(info.domain, nv::NOUVEAU_GEM_DOMAIN_GART);
    assert_eq!(info.offset, 0x4000_0000, "the GPU VA it is bound at");
    assert_eq!(info.map_handle, (h1 as u64) << 12);
    assert_eq!((info.tile_mode, info.tile_flags), (0x10, 0x600));
    assert_eq!(
        gem_info(&gpu, h1, B).map(|i| i.size),
        Err(nv::ENOENT),
        "not B's, not shared"
    );
    assert!(crate::scheme::gem_mmap::add_ref(h1, B).is_some());
    assert_eq!(
        gem_info(&gpu, h1, B).map(|i| i.size),
        Ok(65536),
        "a PRIME holder"
    );
    assert_eq!(
        gem_info(&gpu, h1, STRANGER).map(|i| i.size),
        Err(nv::ENOENT)
    );
    assert_eq!(
        gem_info(&gpu, h1, 0).map(|i| i.size),
        Ok(65536),
        "the kernel"
    );
    // Never CPU-mappable: nothing to import, so only the creator.
    let info = gem_info(&gpu, h2, A).unwrap();
    assert_eq!((info.offset, info.map_handle), (0, 0));
    assert!(crate::scheme::gem_mmap::add_ref(h2, B).is_none());
    assert_eq!(gem_info(&gpu, h2, B).map(|i| i.size), Err(nv::ENOENT));
    assert_eq!(
        gem_info(&gpu, h2, 0).map(|i| i.size),
        Ok(4096),
        "the kernel sees it without a holder entry"
    );
    assert_eq!(
        gem_info(&gpu, h2 + 1000, A).map(|i| i.size),
        Err(nv::ENOENT)
    );
    // CPU_PREP / CPU_FINI apply the same rule.
    assert_eq!(cpu_fini(&gpu, h1, A), Ok(0));
    assert_eq!(cpu_fini(&gpu, h1, B), Ok(0));
    assert_eq!(cpu_fini(&gpu, h1, STRANGER), Err(nv::ENOENT));
    assert_eq!(cpu_fini(&gpu, h2, B), Err(nv::ENOENT));
    assert_eq!(
        cpu_prep_nowait(&gpu, h1, A),
        Ok(0),
        "nothing queued: nothing to wait"
    );
    assert_eq!(cpu_prep_nowait(&gpu, h1, STRANGER), Err(nv::ENOENT));
    assert!(crate::scheme::gem_mmap::unregister(h1));
}

#[test]
fn gem_close_frees_on_the_last_holder_and_takes_its_mappings_and_framebuffers() {
    let _g = LOCK.lock();
    let live = LiveBytes::hold();
    let gpu = gpu();
    let h1 = object(&gpu, A, 65536, Some(0x2000_0000));
    let h2 = object(&gpu, A, 4096, None);
    let h3 = object(&gpu, B, 4096, Some(0x3000_0000));
    crate::scheme::gem_mmap::add_ref(h1, B).unwrap();
    mapping(&gpu, A, h1, 0x1_0000, 65536);
    mapping(&gpu, B, h1, 0x2_0000, 65536);
    mapping(&gpu, B, h3, 0x3_0000, 4096);
    let fb = framebuffer(&gpu, h1);
    let fb3 = framebuffer(&gpu, h3);
    gpu.kms_state.lock().crtc_fb = fb;
    let counted = live.delta();

    assert!(!gpu.nouveau_gem_close(h1, STRANGER), "not a holder");
    assert!(has_object(&gpu, h1));
    assert!(gpu.nouveau_gem_close(h1, B), "one holder letting go");
    assert!(has_object(&gpu, h1), "A still holds it");
    assert_eq!(mappings_of(&gpu, h1), 2);
    assert_eq!(live.delta(), counted);
    assert!(gpu.nouveau_gem_close(h1, A), "the last holder");
    assert!(!has_object(&gpu, h1));
    assert_eq!(
        mappings_of(&gpu, h1),
        0,
        "its VM_BIND mappings went with it"
    );
    assert_eq!(mappings_of(&gpu, h3), 1, "another object's did not");
    let fbs: Vec<u32> = gpu.kms_framebuffers.lock().iter().map(|f| f.id).collect();
    assert_eq!(
        fbs,
        [fb3],
        "the fb built on it is gone, the other one stays"
    );
    assert_eq!(
        gpu.kms_state.lock().crtc_fb,
        0,
        "and is no longer being scanned out"
    );
    assert_eq!(live.delta(), counted - 65536);
    assert!(!gpu.nouveau_gem_close(h1, A), "gone is gone");
    // Never exported: its creator alone, and nobody else can even see it.
    assert!(!gpu.nouveau_gem_close(h2, B));
    assert!(gpu.nouveau_gem_close(h2, A));
    assert_eq!(live.delta(), counted - 65536 - 4096);
    assert!(
        gpu.nouveau_gem_close(h3, 0),
        "the kernel closes on anyone's behalf"
    );
    assert!(!gpu.nouveau_gem_close(h3 + 1000, A));
}

#[test]
fn the_gem_handle_slice_is_never_overrun() {
    let _g = LOCK.lock();
    let gpu = gpu();
    let end = gpu.nouveau_gem_handle_end;
    gpu.nouveau_gem_next_handle
        .store(end - 2, Ordering::Relaxed);
    assert_eq!(gpu.next_gem_handle(), Some(end - 2));
    assert_eq!(gpu.next_gem_handle(), Some(end - 1));
    assert_eq!(gpu.next_gem_handle(), None, "the next id is another card's");
    assert_eq!(gpu.next_gem_handle(), None);
    assert_eq!(gpu.nouveau_gem_next_handle.load(Ordering::Relaxed), end);
}

#[test]
fn channels_without_the_rm_are_discovery_only_and_free_is_owner_scoped() {
    let _g = LOCK.lock();
    let gpu = gpu();
    let c = channel_alloc(&gpu, A).unwrap();
    assert_eq!(c.channel, 0);
    assert_eq!(c.notifier_handle, 0, "no RM notifier");
    assert_eq!(c.pushbuf_domains, nv::NOUVEAU_GEM_DOMAIN_VRAM);
    assert_eq!(c.nr_subchan, 0);
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 1);
    assert_eq!(channel_alloc(&gpu, B).unwrap().channel, 2);
    assert!(!gpu.nouveau_rm_vas_ready());
    assert_eq!(
        vm_bind(&gpu, A),
        Err(nv::ENODEV),
        "no VA space was ever built"
    );
    assert_eq!(channel_free(&gpu, 0, B), Err(nv::EINVAL), "not B's channel");
    assert_eq!(channel_free(&gpu, 7, A), Err(nv::EINVAL));
    assert_eq!(channel_free(&gpu, 0, A), Ok(0));
    assert_eq!(channels_of(&gpu, A), 1);
    assert_eq!(
        channel_alloc(&gpu, B).unwrap().channel,
        0,
        "the lowest free id"
    );
    assert_eq!(channel_free(&gpu, 2, 0), Ok(0), "the kernel frees anyone's");
    assert_eq!(channels_of(&gpu, B), 1);
    while gpu.nouveau_channels.lock().len() < nv::MAX_CHANNELS {
        channel_alloc(&gpu, STRANGER).unwrap();
    }
    assert_eq!(channel_alloc(&gpu, A).map(|c| c.channel), Err(nv::EBUSY));
    assert_eq!(channel_free(&gpu, 1, A), Ok(0));
    assert!(channel_alloc(&gpu, A).is_ok());
}

#[test]
fn process_exit_reclaims_only_the_exiting_pids_channels_objects_and_mappings() {
    let _g = LOCK.lock();
    let live = LiveBytes::hold();
    let gpu = gpu();
    channel_alloc(&gpu, A).unwrap();
    channel_alloc(&gpu, A).unwrap();
    channel_alloc(&gpu, B).unwrap();
    let h1 = object(&gpu, A, 4096, None);
    let h2 = object(&gpu, A, 65536, Some(0x2000_0000));
    let h3 = object(&gpu, A, 8192, Some(0x3000_0000));
    crate::scheme::gem_mmap::add_ref(h3, B).unwrap();
    let h4 = object(&gpu, B, 4096, Some(0x4000_0000));
    let h5 = object(&gpu, B, 4096, None);
    let h6 = object(&gpu, STRANGER, 4096, Some(0x6000_0000));
    crate::scheme::gem_mmap::add_ref(h6, B).unwrap();
    let kernels = object(&gpu, 0, 4096, None);
    mapping(&gpu, A, h1, 0x1_0000, 4096);
    mapping(&gpu, A, h2, 0x2_0000, 65536);
    mapping(&gpu, B, h3, 0x3_0000, 8192);
    let fb = framebuffer(&gpu, h2);
    gpu.kms_state.lock().plane_fb = fb;
    let counted = live.delta();

    gpu.nouveau_release_process(0);
    assert_eq!(channels_of(&gpu, A), 2, "pid 0 is nobody");
    assert!(has_object(&gpu, kernels), "and owns nothing to reclaim");

    gpu.nouveau_release_process(A);
    assert_eq!(channels_of(&gpu, A), 0);
    assert_eq!(channels_of(&gpu, B), 1);
    assert!(!has_object(&gpu, h1), "never exported: freed");
    assert!(!has_object(&gpu, h2), "exported, A the only holder: freed");
    assert!(crate::scheme::gem_mmap::lookup(h2).is_none());
    assert_eq!(
        owner_of(&gpu, h3),
        Some(0),
        "B still imports it: orphaned, not freed"
    );
    assert!(crate::scheme::gem_mmap::holds(h3, B));
    assert_eq!(owner_of(&gpu, h4), Some(B));
    assert!(has_object(&gpu, h5), "B's unexported object is B's");
    assert_eq!(mappings_of(&gpu, h1) + mappings_of(&gpu, h2), 0);
    assert_eq!(
        mappings_of(&gpu, h3),
        1,
        "B's mapping of the shared object stays"
    );
    assert!(gpu.kms_framebuffers.lock().is_empty());
    assert_eq!(gpu.kms_state.lock().plane_fb, 0);
    assert_eq!(
        live.delta(),
        counted - 4096 - 65536,
        "the orphan is still live"
    );
    // The orphan now belongs to B alone: B's own GEM_CLOSE frees it,
    // owner or not (the last holder is who Linux frees for).
    assert!(gpu.nouveau_gem_close(h3, B));
    assert!(!has_object(&gpu, h3));
    assert_eq!(mappings_of(&gpu, h3), 0);
    assert_eq!(live.delta(), counted - 4096 - 65536 - 8192);
    // B's exit: its own objects go, its import of a LIVE owner's
    // buffer is let go without detaching that owner.
    gpu.nouveau_release_process(B);
    assert!(!has_object(&gpu, h4));
    assert!(!has_object(&gpu, h5));
    assert_eq!(owner_of(&gpu, h6), Some(STRANGER), "still the owner's");
    assert!(!crate::scheme::gem_mmap::holds(h6, B));
    assert!(crate::scheme::gem_mmap::holds(h6, STRANGER));
    assert!(gpu.nouveau_channels.lock().is_empty());
    gpu.nouveau_release_process(STRANGER);
    assert!(!has_object(&gpu, h6));
    assert!(
        has_object(&gpu, kernels),
        "nobody's exit reclaims the kernel's"
    );
    gpu.nouveau_gem.lock().retain(|o| o.handle != kernels);
    NOUVEAU_GEM_BYTES.fetch_sub(4096, Ordering::Relaxed);
    assert_eq!(live.delta(), 0);
}

#[test]
fn drain_vm_mappings_takes_exactly_what_matches_and_keeps_the_rest_in_order() {
    let _g = LOCK.lock();
    let gpu = gpu();
    mapping(&gpu, A, 1, 0x1000, 4096);
    mapping(&gpu, B, 2, 0x2000, 4096);
    mapping(&gpu, A, 3, 0x3000, 4096);
    mapping(&gpu, B, 4, 0x4000, 4096);
    assert_eq!(
        gpu.drain_vm_mappings("test", |m| m.owner_pid == A, false),
        2
    );
    let left: Vec<u32> = gpu
        .nouveau_vm_mappings
        .lock()
        .iter()
        .map(|m| m.gem_handle)
        .collect();
    assert_eq!(left, [2, 4]);
    assert_eq!(gpu.drain_vm_mappings("test", |_| false, true), 0);
    assert_eq!(gpu.drain_vm_mappings("test", |m| m.va == 0x4000, true), 1);
    let left: Vec<u32> = gpu
        .nouveau_vm_mappings
        .lock()
        .iter()
        .map(|m| m.gem_handle)
        .collect();
    assert_eq!(left, [2]);
}

#[test]
fn gem_info_reports_the_callers_own_binding_never_another_contexts_va() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    let h = object(&gpu, A, 65536, Some(0x5000_0000));
    assert!(crate::scheme::gem_mmap::add_ref(h, B).is_some());
    mapping(&gpu, A, h, 0x4000_0000, 65536);
    assert_eq!(gem_info(&gpu, h, A).unwrap().offset, 0x4000_0000);
    // B imported the object but never bound it: in B's VA space the
    // object is nowhere, and A's VA would point at whatever B has
    // there. Linux: `nouveau_vma_find(nvbo, cli->vmm)` -> NULL -> 0.
    assert_eq!(
        gem_info(&gpu, h, B).unwrap().offset,
        0,
        "the creator's VA means nothing in the importer's context"
    );
    mapping(&gpu, B, h, 0x7000_0000, 65536);
    assert_eq!(gem_info(&gpu, h, B).unwrap().offset, 0x7000_0000);
    assert_eq!(
        gem_info(&gpu, h, A).unwrap().offset,
        0x4000_0000,
        "A keeps its own, whatever B bound"
    );
    assert_eq!(
        gem_info(&gpu, h, 0).unwrap().offset,
        0x4000_0000,
        "the kernel has no VA space: the first binding"
    );
    assert!(crate::scheme::gem_mmap::unregister(h));
}

#[test]
fn vram_used_is_the_sum_of_every_clients_live_vram_objects() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu();
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(0));
    let in_gart = object(&gpu, A, 65536, None);
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED),
        Ok(0),
        "GART is system memory"
    );
    let mine = vram_object(&gpu, A, 3 * MIB);
    let theirs = vram_object(&gpu, B, 5 * MIB);
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED),
        Ok(8 * MIB),
        "the VRAM manager's usage: every client's, as Linux reports it"
    );
    // Another GPU's objects are that GPU's VRAM, not this one's.
    let other = NvidiaGpu::for_test(0x1f06, 8192);
    vram_object(&other, A, 7 * MIB);
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(8 * MIB));
    assert_eq!(
        getparam(&other, nv::NOUVEAU_GETPARAM_VRAM_USED),
        Ok(7 * MIB)
    );
    // Freed objects stop counting.
    gpu.nouveau_gem.lock().retain(|o| o.handle != theirs);
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(3 * MIB));
    gpu.nouveau_gem.lock().retain(|o| o.handle != mine);
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(0));
    gpu.nouveau_gem.lock().retain(|o| o.handle != in_gart);
}

// ----- NVIF: five payloads on one nr, resolved by the header's type -----

const HDR: usize = size_of::<nv::NvifIoctlV0>();
const NEW: usize = size_of::<nv::NvifIoctlNewV0>();
const MTHD: usize = size_of::<nv::NvifIoctlMthdV0>();
const SCLASS: usize = size_of::<nv::NvifIoctlSclassV0>();
const OCLASS: usize = size_of::<nv::NvifSclassOclassV0>();
const INFO: usize = size_of::<nv::NvDeviceInfoV0>();

/// A raw NVIF request: mesa's anonymous `struct { ioctl; body; data }`
/// as bytes, written unaligned exactly as the arm reads it.
struct Nvif(Vec<u8>);

impl Nvif {
    fn new(type_: u8, route: u8, token: u64, object: u64, len: usize) -> Self {
        let mut b = alloc::vec![0u8; len];
        let hdr = nv::NvifIoctlV0 {
            version: 0,
            type_,
            pad02: [0; 4],
            owner: 0,
            route,
            token,
            object,
        };
        unsafe { core::ptr::write_unaligned(b.as_mut_ptr() as *mut nv::NvifIoctlV0, hdr) };
        Nvif(b)
    }

    fn put<T: Copy>(mut self, at: usize, v: T) -> Self {
        assert!(at + size_of::<T>() <= self.0.len());
        unsafe { core::ptr::write_unaligned(self.0.as_mut_ptr().add(at) as *mut T, v) };
        self
    }

    /// Shortens the declared length, keeping the bytes behind it
    /// allocated (and zero): a driver that reads past the length reads
    /// a zero, not the heap, so the only thing a test observes is what
    /// the driver decided from the length itself.
    fn cut(mut self, len: usize) -> Self {
        self.0.truncate(len);
        self
    }

    fn get<T: Copy>(&self, at: usize) -> T {
        assert!(at + size_of::<T>() <= self.0.len());
        unsafe { core::ptr::read_unaligned(self.0.as_ptr().add(at) as *const T) }
    }

    fn send(&mut self, gpu: &NvidiaGpu, pid: u64) -> Result<usize, i32> {
        let req = ioc(IOC_WRITE, DRM_IOCTL_TYPE, nv::NR_NVIF, self.0.len());
        gpu.nouveau_ioctl(req, self.0.as_mut_ptr() as usize, pid)
    }
}

fn new_body(oclass: i32, object: u64) -> nv::NvifIoctlNewV0 {
    nv::NvifIoctlNewV0 {
        version: 0,
        pad01: [0; 6],
        route: 0,
        token: object,
        object,
        handle: 0,
        oclass,
    }
}

/// `nouveau_ws_device_alloc`: 72 bytes, NEW of NV_DEVICE with a selector.
fn device_new(device: u64) -> Nvif {
    Nvif::new(
        nv::NVIF_IOCTL_V0_NEW,
        0,
        0,
        0,
        HDR + NEW + size_of::<nv::NvDeviceV0>(),
    )
    .put(HDR, new_body(nv::NVIF_CLASS_NV_DEVICE, 0xd0d0))
    .put(
        HDR + NEW,
        nv::NvDeviceV0 {
            version: 0,
            pad01: [0; 7],
            device,
        },
    )
}

/// `nouveau_ws_subchan_alloc`: 56 bytes, NEW of an engine class on a
/// channel (route 0xff, token = the channel id).
fn subchan_new(channel: u64, oclass: i32, object: u64) -> Nvif {
    Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0xff, channel, 0, HDR + NEW).put(HDR, new_body(oclass, object))
}

/// `nouveau_ws_device_info`: 136 bytes, MTHD NV_DEVICE_V0_INFO.
fn device_info(method: u8) -> Nvif {
    Nvif::new(nv::NVIF_IOCTL_V0_MTHD, 0, 0, 0xd0d0, HDR + MTHD + INFO).put(
        HDR,
        nv::NvifIoctlMthdV0 {
            version: 0,
            method,
            pad02: [0; 6],
        },
    )
}

/// `nouveau_ws_context_query_classes`: SCLASS with `slots` entries of
/// room, every one pre-filled so a stale slot is visible.
fn sclass(channel: u64, route: u8, count: u8, slots: usize) -> Nvif {
    let mut r = Nvif::new(
        nv::NVIF_IOCTL_V0_SCLASS,
        route,
        channel,
        0,
        HDR + SCLASS + slots * OCLASS,
    )
    .put(
        HDR,
        nv::NvifIoctlSclassV0 {
            version: 0,
            count,
            pad02: [0; 6],
        },
    );
    for i in 0..slots {
        r = r.put(
            HDR + SCLASS + i * OCLASS,
            nv::NvifSclassOclassV0 {
                oclass: 0x7777,
                minver: 7,
                maxver: 7,
            },
        );
    }
    r
}

fn classes_in(r: &Nvif) -> (u8, Vec<i32>) {
    let count = r.get::<nv::NvifIoctlSclassV0>(HDR).count;
    let slots = (r.0.len() - HDR - SCLASS) / OCLASS;
    let list = (0..slots)
        .map(|i| {
            r.get::<nv::NvifSclassOclassV0>(HDR + SCLASS + i * OCLASS)
                .oclass
        })
        .collect();
    (count, list)
}

fn cstr(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).unwrap()
}

#[test]
fn nvif_refuses_a_short_payload_and_an_unknown_type_before_reading_a_body() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0, 0, 0, HDR - 1).send(&gpu, A),
        Err(nv::EINVAL),
        "shorter than the header"
    );
    assert_eq!(
        Nvif::new(0x09, 0, 0, 0, HDR + NEW).send(&gpu, A),
        Err(nv::ENOSYS),
        "a type this driver has no arm for"
    );
    assert_eq!(
        device_new(u64::MAX).cut(HDR + NEW - 1).send(&gpu, A),
        Err(nv::EINVAL),
        "NEW without its 32-byte body, however acceptable the bytes behind"
    );
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_MTHD, 0, 0, 0, HDR + MTHD - 1).send(&gpu, A),
        Err(nv::EINVAL),
        "MTHD without its 8-byte body"
    );
    assert_eq!(
        device_info(nv::NV_DEVICE_V0_INFO)
            .cut(HDR + MTHD + INFO - 1)
            .send(&gpu, A),
        Err(nv::EINVAL),
        "INFO with no room for its 104-byte reply"
    );
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(
        sclass(0, 0xff, 16, 16).cut(HDR + SCLASS - 1).send(&gpu, A),
        Err(nv::EINVAL),
        "SCLASS without its 8-byte body, on a channel that exists"
    );
    // An NVIF request has no fixed floor at the dispatch: the 24-byte
    // DEL is a complete request.
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0x1234, HDR).send(&gpu, A),
        Ok(0)
    );
}

#[test]
fn nvif_new_of_the_device_object_takes_only_the_client_default() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(device_new(u64::MAX).send(&gpu, A), Ok(0), "mesa's ~0");
    assert_eq!(
        device_new(0).send(&gpu, A),
        Err(nv::EINVAL),
        "this node exposes one GPU; selecting another is an error"
    );
    assert_eq!(device_new(1).send(&gpu, A), Err(nv::EINVAL));
    // No class data at all (a 56-byte NEW of NV_DEVICE): the default.
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0, 0, 0, HDR + NEW)
            .put(HDR, new_body(nv::NVIF_CLASS_NV_DEVICE, 1))
            .send(&gpu, A),
        Ok(0)
    );
    // oclass 0 is what mesa sends when SCLASS gave it nothing: refused
    // even on a channel of the caller's, where any real class is fine.
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(subchan_new(0, 0xc597, 1).send(&gpu, A), Ok(0));
    assert_eq!(subchan_new(0, 0, 1).send(&gpu, A), Err(nv::EINVAL));
}

#[test]
fn nvif_device_info_reports_the_board_and_floors_its_vram() {
    let _g = LOCK.lock();
    let gpu = gpu();
    let mut r = device_info(nv::NV_DEVICE_V0_INFO);
    assert_eq!(r.send(&gpu, A), Ok(0));
    let info: nv::NvDeviceInfoV0 = r.get(HDR + MTHD);
    assert_eq!(info.version, 0);
    assert_eq!(
        info.platform,
        nv::NV_DEVICE_INFO_V0_PCIE,
        "discrete: NVK's conformance gate needs DIS"
    );
    assert_eq!(info.chipset, 0x162, "the same chip GETPARAM reports");
    assert_eq!(info.revision, 0, "BAR0 reads as zero");
    assert_eq!(info.family, 0);
    assert_eq!(
        (info.ram_size, info.ram_user),
        (8192 * MIB, 8192 * MIB),
        "ram_user is what mesa takes as vram_size_B"
    );
    assert_eq!(cstr(&info.chip), "TU1xx");
    assert_eq!(cstr(&info.name), "nvidia-test");
    assert_eq!(
        device_info(0x05).send(&gpu, A),
        Err(nv::ENOSYS),
        "the only method is INFO"
    );
    // A board the id table does not know: the architecture's floor,
    // never a 0 that would leave NVK with an empty VRAM heap.
    let unknown = NvidiaGpu::for_test(0x1fff, 0);
    let mut r = device_info(nv::NV_DEVICE_V0_INFO);
    assert_eq!(r.send(&unknown, A), Ok(0));
    let info: nv::NvDeviceInfoV0 = r.get(HDR + MTHD);
    assert_eq!(info.ram_user, 4096 * MIB);
}

#[test]
fn nvif_sclass_lists_the_callers_channels_engines_within_the_room_offered() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(
        sclass(0, 0xff, 16, 16).send(&gpu, A),
        Err(nv::EINVAL),
        "no channel yet"
    );
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(
        sclass(0, 0x00, 16, 16).send(&gpu, A),
        Err(nv::EINVAL),
        "classes hang off a channel: route must be 0xff"
    );
    assert_eq!(
        sclass(0, 0xff, 16, 16).send(&gpu, B),
        Err(nv::EINVAL),
        "not B's channel"
    );
    assert_eq!(sclass(7, 0xff, 16, 16).send(&gpu, A), Err(nv::EINVAL));
    // Mesa's call: 16 slots offered, all five engines come back and
    // the unused tail is cleared (mesa reads every slot).
    let turing = [0x902d, 0xa140, 0xc597, 0xc5c0, 0xc5b5];
    let mut r = sclass(0, 0xff, 16, 16);
    assert_eq!(r.send(&gpu, A), Ok(0));
    let (count, list) = classes_in(&r);
    assert_eq!(count, 5);
    assert_eq!(&list[..5], &turing);
    assert!(
        list[5..].iter().all(|&c| c == 0),
        "no stale slot: {:?}",
        list
    );
    let mut r = sclass(0, 0xff, 16, 16);
    assert_eq!(r.send(&gpu, 0), Ok(0), "the kernel reads anyone's");
    // Less room than engines, said by count: the first ones, and
    // nothing written past what the caller offered.
    let mut r = sclass(0, 0xff, 3, 16);
    assert_eq!(r.send(&gpu, A), Ok(0));
    let (count, list) = classes_in(&r);
    assert_eq!(count, 3);
    assert_eq!(&list[..3], &turing[..3]);
    assert!(
        list[3..].iter().all(|&c| c == 0x7777),
        "beyond count is the caller's: {:?}",
        list
    );
    // Less room than count, said by the payload itself.
    let mut r = sclass(0, 0xff, 16, 2);
    assert_eq!(r.send(&gpu, A), Ok(0));
    assert_eq!(classes_in(&r), (2, alloc::vec![0x902d, 0xa140]));
    // More slots than the protocol's ceiling: capped at 16.
    let mut r = sclass(0, 0xff, 40, 40);
    assert_eq!(r.send(&gpu, A), Ok(0));
    let (count, list) = classes_in(&r);
    assert_eq!(count, 5);
    assert!(list[5..16].iter().all(|&c| c == 0));
    assert!(
        list[16..].iter().all(|&c| c == 0x7777),
        "past 16 is never touched"
    );
    // A second GPU of another architecture answers with its own triple.
    let ampere = NvidiaGpu::for_test(0x2204, 24576);
    assert_eq!(channel_alloc(&ampere, A).unwrap().channel, 0);
    let mut r = sclass(0, 0xff, 16, 16);
    assert_eq!(r.send(&ampere, A), Ok(0));
    assert_eq!(
        &classes_in(&r).1[..5],
        &[0x902d, 0xa140, 0xc797, 0xc7c0, 0xc7b5]
    );
}

#[test]
fn nvif_new_of_a_subchannel_needs_the_callers_channel_and_records_nothing_without_the_rm() {
    let _g = LOCK.lock();
    let gpu = gpu();
    assert_eq!(
        subchan_new(0, 0xc597, 0x1000).send(&gpu, A),
        Err(nv::EINVAL),
        "no channel: Linux's abi16 finds no object for the token"
    );
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
    assert_eq!(
        subchan_new(0, 0xc5c0, 0x1008).send(&gpu, 0),
        Ok(0),
        "the kernel"
    );
    assert_eq!(
        subchan_new(0, 0xc597, 0x1000).send(&gpu, B),
        Err(nv::EINVAL),
        "A's channel is not B's"
    );
    assert_eq!(
        subchan_new(3, 0xc597, 0x1000).send(&gpu, A),
        Err(nv::EINVAL)
    );
    assert!(
        nv::class_objects_drain_pid(A).is_empty(),
        "a discovery channel builds no RM object, so there is nothing to reap"
    );
    assert_eq!(channel_free(&gpu, 0, A), Ok(0));
    assert_eq!(
        subchan_new(0, 0xc597, 0x1000).send(&gpu, A),
        Err(nv::EINVAL),
        "freed: the token means nothing again"
    );
}

#[test]
fn nvif_del_and_the_class_registry_are_scoped_to_the_owner_and_the_channel() {
    let _g = LOCK.lock();
    let gpu = gpu();
    nv::class_object_insert(3, 0x1000, 0x5a5a, A);
    nv::class_object_insert(3, 0x1000, 0x6b6b, B);
    nv::class_object_insert(4, 0x2000, 0x7c7c, A);
    nv::class_object_insert(3, 0x3000, 0x8d8d, A);
    // A DEL names the object by the cookie mesa passed at NEW (a heap
    // pointer, so equal across processes): only the caller's goes.
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 3, 0x1000, HDR).send(&gpu, STRANGER),
        Ok(0),
        "nothing of STRANGER's: a no-op, as in Linux's nvif_object_dtor"
    );
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 3, 0x1000, HDR).send(&gpu, A),
        Ok(0)
    );
    assert_eq!(nv::class_object_remove(0x1000, A), None, "gone");
    assert_eq!(
        nv::class_object_remove(0x1000, B),
        Some(0x6b6b),
        "B's survived A's DEL"
    );
    // CHANNEL_FREE reaps what a process left on THAT channel.
    assert_eq!(nv::class_objects_drain_channel(3, B), alloc::vec![]);
    assert_eq!(
        nv::class_objects_drain_channel(3, A),
        alloc::vec![(0x3000, 0x8d8d)]
    );
    assert_eq!(nv::class_objects_drain_channel(4, B), alloc::vec![]);
    // Process exit reaps everything of that pid, across channels.
    nv::class_object_insert(5, 0x5000, 0x9e9e, A);
    nv::class_object_insert(5, 0x6000, 0xafaf, B);
    assert_eq!(
        nv::class_objects_drain_pid(A),
        alloc::vec![(0x2000, 0x7c7c), (0x5000, 0x9e9e)],
        "in insertion order"
    );
    assert_eq!(nv::class_objects_drain_pid(A), alloc::vec![]);
    assert_eq!(
        nv::class_objects_drain_pid(B),
        alloc::vec![(0x6000, 0xafaf)]
    );
}

// ----- With the fake RM: the arms a GL client reaches once attached -----

use super::rm_host_shims::{
    fake_fbmem_offset, notifier_pa, reset_fake_rm, FastChan, FAKE_DOORBELL, FAKE_RM, FAST_ENTRIES,
    FAST_GPFIFO_OFF, FAST_PB_OFF, FAST_SEM_OFF,
};

/// The compositor's pid: it owns ctx 0, so every other pid is a GL
/// client and gets a context of its own.
const COMP: u64 = 66_000;
/// The compositor respawned: a new pid for the same role.
const COMP2: u64 = 66_004;

fn gpu_rm() -> NvidiaGpu {
    let gpu = gpu();
    reset_fake_rm();
    *gpu.rm_device_instance.lock() = Some(0);
    gpu.ctx0_owner.store(COMP, Ordering::Release);
    gpu
}

fn gem_new_rm(
    gpu: &NvidiaGpu,
    size: u64,
    domain: u32,
    pid: u64,
) -> Result<nv::DrmNouveauGemInfo, i32> {
    let mut r = nv::DrmNouveauGemNew {
        info: nv::DrmNouveauGemInfo {
            handle: 0,
            domain,
            size,
            offset: 0,
            map_handle: 0,
            tile_mode: 0,
            tile_flags: 0,
        },
        channel_hint: 0,
        align: 0,
    };
    call(gpu, wr::<nv::DrmNouveauGemNew>(nv::NR_GEM_NEW), &mut r, pid).map(|_| r.info)
}

fn op(op: u32, flags: u32, handle: u32, addr: u64, range: u64) -> nv::DrmNouveauVmBindOp {
    nv::DrmNouveauVmBindOp {
        op,
        flags,
        handle,
        pad: 0,
        addr,
        bo_offset: 0,
        range,
    }
}

fn map(handle: u32, addr: u64, range: u64) -> nv::DrmNouveauVmBindOp {
    op(
        nv::VM_BIND_OP_MAP,
        nv::PTE_KIND_GENERIC,
        handle,
        addr,
        range,
    )
}

fn map_at(handle: u32, addr: u64, range: u64, bo_offset: u64) -> nv::DrmNouveauVmBindOp {
    nv::DrmNouveauVmBindOp {
        bo_offset,
        ..map(handle, addr, range)
    }
}

fn unmap(addr: u64, range: u64) -> nv::DrmNouveauVmBindOp {
    op(nv::VM_BIND_OP_UNMAP, 0, 0, addr, range)
}

fn vm_bind_ops(
    gpu: &NvidiaGpu,
    pid: u64,
    ops: &mut [nv::DrmNouveauVmBindOp],
) -> Result<usize, i32> {
    vm_bind_sync(gpu, pid, ops, 0, &[], &[])
}

/// `VM_BIND` as NVK's bind context issues it for `vkQueueBindSparse`:
/// `flags` (`RUN_ASYNC`), the submit's waits and its sigs.
fn vm_bind_sync(
    gpu: &NvidiaGpu,
    pid: u64,
    ops: &mut [nv::DrmNouveauVmBindOp],
    flags: u32,
    waits: &[nv::DrmNouveauSync],
    sigs: &[nv::DrmNouveauSync],
) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauVmBind {
        op_count: ops.len() as u32,
        flags,
        wait_count: waits.len() as u32,
        sig_count: sigs.len() as u32,
        wait_ptr: ptr_of(waits),
        sig_ptr: ptr_of(sigs),
        op_ptr: if ops.is_empty() {
            0
        } else {
            ops.as_mut_ptr() as u64
        },
    };
    with_uvmm(gpu, pid);
    call(gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, pid)
}

fn ctx_of(gpu: &NvidiaGpu, pid: u64) -> Option<(u32, bool)> {
    gpu.nouveau_pid_ctx
        .lock()
        .iter()
        .find(|t| t.0 == pid)
        .map(|t| (t.1, t.4))
}

fn rm_backed_channels(gpu: &NvidiaGpu, pid: u64) -> usize {
    gpu.nouveau_channels
        .lock()
        .iter()
        .filter(|c| c.owner_pid == pid && c.rm_backed)
        .count()
}

fn driver_maps(gpu: &NvidiaGpu, pid: u64) -> Vec<(u32, u64, u64, u32)> {
    gpu.nouveau_vm_mappings
        .lock()
        .iter()
        .filter(|m| m.owner_pid == pid)
        .map(|m| (m.gem_handle, m.va, m.size, m.h_virt))
        .collect()
}

#[test]
fn with_the_rm_a_clients_channel_builds_its_own_context_once_and_falls_back_when_the_rm_refuses() {
    let _g = LOCK.lock();
    let gpu = gpu_rm();
    let c = channel_alloc(&gpu, A).unwrap();
    assert_eq!(c.channel, 0);
    assert_eq!(c.notifier_handle, 0x6001, "the notifier of CTX 1");
    assert_eq!(
        ctx_of(&gpu, A),
        Some((1, true)),
        "built, primed, published READY"
    );
    assert_eq!(rm_backed_channels(&gpu, A), 1);
    assert_eq!(FAKE_RM.lock().calls, ["ctx_alloc", "ctx_prime"]);
    // A second channel of the same pid reuses the context: NVK opens
    // several per process (labwc runs two Vulkan instances).
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 1);
    assert_eq!(rm_backed_channels(&gpu, A), 2);
    assert_eq!(FAKE_RM.lock().ctxs, [1], "still one context");
    assert_eq!(channel_alloc(&gpu, B).unwrap().notifier_handle, 0x6002);
    assert_eq!(ctx_of(&gpu, B), Some((2, true)));
    assert!(gpu.nouveau_rm_vas_ready());
    // The compositor's own channel is the step16/17 ladder, which the
    // fake does not carry: the honest answer is ENODEV, not a client
    // context in disguise.
    assert_eq!(
        channel_alloc(&gpu, COMP).map(|c| c.channel),
        Err(nv::ENODEV)
    );
    assert_eq!(ctx_of(&gpu, COMP), None);
    // ctx_alloc refused: a discovery channel, nothing reserved.
    FAKE_RM.lock().fail_ctx_alloc = true;
    let c = channel_alloc(&gpu, STRANGER).unwrap();
    assert_eq!(c.notifier_handle, 0, "no RM notifier");
    assert_eq!(rm_backed_channels(&gpu, STRANGER), 0);
    assert_eq!(ctx_of(&gpu, STRANGER), None, "the reservation was removed");
    assert_eq!(FAKE_RM.lock().ctxs, [1, 2]);
    FAKE_RM.lock().fail_ctx_alloc = false;
    // ctx_alloc returned NV_OK with a failed stage: the C side already
    // freed what it built, so the driver only drops the reservation and
    // must neither prime nor ctx_free a context that does not exist.
    FAKE_RM.lock().incomplete_ctx = true;
    assert_eq!(channel_alloc(&gpu, STRANGER).unwrap().notifier_handle, 0);
    assert_eq!(ctx_of(&gpu, STRANGER), None);
    assert_eq!(
        FAKE_RM.lock().calls.last(),
        Some(&"ctx_alloc"),
        "half-built: not primed, and nothing to free"
    );
    assert_eq!(FAKE_RM.lock().ctx_frees, 0);
    FAKE_RM.lock().incomplete_ctx = false;
    // Prime failed: the context is torn down again (a kept one hangs
    // FECS on the first 3D draw) and the client falls back to software.
    FAKE_RM.lock().fail_prime = true;
    assert_eq!(channel_alloc(&gpu, STRANGER).unwrap().notifier_handle, 0);
    assert_eq!(ctx_of(&gpu, STRANGER), None);
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.ctx_frees, 1, "ctx_free after the failed prime");
        assert_eq!(f.ctxs, [1, 2], "slot 3 is free again");
    }
    FAKE_RM.lock().fail_prime = false;
    assert_eq!(
        ctx_of(&gpu, A),
        Some((1, true)),
        "A's context is untouched by all that"
    );
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(STRANGER);
}

#[test]
fn with_the_rm_gem_new_registers_what_is_cpu_mappable_and_close_gives_the_memory_back() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let gart = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A).unwrap();
    let h_gart = FAKE_RM.lock().gems[0].0;
    assert_eq!(FAKE_RM.lock().gems, [(h_gart, 65536, true)]);
    assert_eq!(gart.domain, nv::NOUVEAU_GEM_DOMAIN_GART);
    assert_eq!(
        gart.map_handle,
        u64::from(gart.handle) << 12,
        "CPU-mappable"
    );
    assert_eq!(
        crate::scheme::gem_mmap::lookup(gart.handle),
        Some((FAKE_RM.lock().pa_of(h_gart).unwrap(), 65536)),
        "registered for mmap and PRIME at the RM's host PA"
    );
    assert!(crate::scheme::gem_mmap::holds(gart.handle, A));
    // GART|VRAM (NVK's DEVICE_LOCAL|HOST_VISIBLE) stays system memory.
    let both = gem_new_rm(
        &gpu,
        4096,
        nv::NOUVEAU_GEM_DOMAIN_GART | nv::NOUVEAU_GEM_DOMAIN_VRAM,
        A,
    )
    .unwrap();
    assert_eq!(both.domain, nv::NOUVEAU_GEM_DOMAIN_GART);
    assert_ne!(both.map_handle, 0);
    // VRAM-only is DEVICE_LOCAL: no host PA is ever published for it.
    let vram = gem_new_rm(&gpu, 3 * MIB, nv::NOUVEAU_GEM_DOMAIN_VRAM, A).unwrap();
    let h_vram = FAKE_RM.lock().gems[2].0;
    assert_eq!(FAKE_RM.lock().gems[2], (h_vram, 3 * MIB, false));
    assert_eq!(
        (vram.domain, vram.map_handle),
        (nv::NOUVEAU_GEM_DOMAIN_VRAM, 0)
    );
    assert!(crate::scheme::gem_mmap::lookup(vram.handle).is_none());
    assert_eq!(
        gpu.nouveau_gem
            .lock()
            .iter()
            .find(|o| o.handle == vram.handle)
            .and_then(|o| o.vram_offset),
        Some(fake_fbmem_offset(h_vram)),
        "its FBMEM offset, for scanout by offset"
    );
    assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(3 * MIB));
    assert_eq!(
        _live.delta(),
        65536 + 4096 + 3 * MIB as i64,
        "every byte the RM holds is counted against the quotas"
    );
    assert_eq!(
        &FAKE_RM.lock().calls[4..],
        ["gem_alloc", "gem_fbmem_offset"],
        "VRAM: never asked for a CPU mapping"
    );
    assert_eq!(
        gem_info(&gpu, gart.handle, A).map(|i| (i.size, i.map_handle)),
        Ok((65536, gart.map_handle))
    );
    // The RM put a "GART" object in an aperture the CPU cannot reach:
    // a live object, but not mmap-able and not registered as such.
    FAKE_RM.lock().map_cpu_elsewhere = true;
    let far = gem_new_rm(&gpu, 8192, nv::NOUVEAU_GEM_DOMAIN_GART, A).unwrap();
    FAKE_RM.lock().map_cpu_elsewhere = false;
    assert_eq!(far.map_handle, 0);
    assert!(crate::scheme::gem_mmap::lookup(far.handle).is_none());
    assert!(has_object(&gpu, far.handle));
    assert!(gpu.nouveau_gem_close(far.handle, A));
    assert_eq!(FAKE_RM.lock().gem_frees, 1);
    // The RM refuses: nothing is left behind, not even the bytes.
    let counted = _live.delta();
    FAKE_RM.lock().fail_gem_alloc = true;
    assert_eq!(
        gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A).map(|i| i.handle),
        Err(nv::ENOMEM)
    );
    FAKE_RM.lock().fail_gem_alloc = false;
    assert_eq!(_live.delta(), counted);
    assert_eq!(gpu.nouveau_gem.lock().len(), 3);
    // GEM_CLOSE hands the RM memory back, once, and the registry entry
    // goes with it.
    assert!(gpu.nouveau_gem_close(gart.handle, A));
    assert!(crate::scheme::gem_mmap::lookup(gart.handle).is_none());
    assert!(gpu.nouveau_gem_close(vram.handle, A));
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.gem_frees, 3);
        assert_eq!(f.gems.len(), 1, "only the GART|VRAM one is still allocated");
        assert_eq!(f.bad, 0);
    }
    assert!(
        !gpu.nouveau_gem_close(gart.handle, A),
        "closed twice: refused, not freed twice"
    );
    assert_eq!(FAKE_RM.lock().bad, 0);
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().gems, [], "process exit freed the last one");
}

#[test]
fn vm_bind_maps_into_the_callers_own_context_and_a_map_over_a_live_range_replaces_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    const VA: u64 = 0x3f_f000_0000;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(1, VA, 4096)]),
        Err(nv::ENODEV),
        "no RM-backed channel on this GPU: no VA space to bind into"
    );
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let h_mem_a = FAKE_RM.lock().gems[0].0;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
    let f_maps = FAKE_RM.lock().maps.clone();
    assert_eq!(f_maps.len(), 1);
    let (h_virt, ctx, h_mem, va, size, bo_offset, kind) = f_maps[0];
    assert_eq!(
        (ctx, h_mem, va, size, bo_offset, kind),
        (1, h_mem_a, VA, 65536, 0, 0x06),
        "A's context, its memory, the kind verbatim"
    );
    assert_eq!(
        driver_maps(&gpu, A),
        [(ha, VA, 65536, h_virt)],
        "the driver's record names the RM's h_virt"
    );
    assert_eq!(gem_info(&gpu, ha, A).unwrap().offset, VA);
    // REPLACE: a MAP over a live range of the same context unmaps the
    // part it covers first (Linux gpuvm semantics; the RM would refuse
    // the fixed VA); the part outside the range stays, as a REMAP.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, VA + 0x8000, 65536)]),
        Ok(0)
    );
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.unmaps, 1, "the old binding was unmapped in the RM");
        assert_eq!(
            f.maps_of_ctx(1),
            [(VA, 0x8000, 0x06), (VA + 0x8000, 65536, 0x06)],
            "its head was mapped again, then the new one"
        );
        assert_eq!(f.bad, 0);
    }
    assert_eq!(driver_maps(&gpu, A).len(), 2);
    // A mapping that starts exactly where the new one ends is a
    // neighbour, not an overlap: it stays. The offset into the object
    // reaches the RM and the record.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map_at(ha, VA + 0x18000, 4096, 0x3000)]),
        Ok(0)
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, VA + 0x8000, 65536)]),
        Ok(0),
        "re-bound over itself"
    );
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.unmaps, 2, "only the overlapping one was replaced");
        assert_eq!(
            f.maps_of_ctx(1),
            [
                (VA, 0x8000, 0x06),
                (VA + 0x18000, 4096, 0x06),
                (VA + 0x8000, 65536, 0x06)
            ]
        );
        assert_eq!(
            f.maps.iter().find(|m| m.3 == VA + 0x18000).map(|m| m.5),
            Some(0x3000),
            "bo_offset handed to the RM"
        );
    }
    assert_eq!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .find(|m| m.va == VA + 0x18000)
            .map(|m| m.bo_offset),
        Some(0x3000)
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x18000, 4096)]),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A).len(), 2);
    assert_eq!(FAKE_RM.lock().unmaps, 3);
    // Another process, the same VA: its own context, so no conflict.
    assert_eq!(channel_alloc(&gpu, B).unwrap().channel, 1);
    let hb = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, B)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, B, &mut [map(hb, VA + 0x8000, 4096)]),
        Ok(0)
    );
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.maps.len(), 3, "A's two pieces and B's");
        assert_eq!(f.maps_of_ctx(2), [(VA + 0x8000, 4096, 0x06)]);
        assert_eq!(f.unmaps, 3, "B replaced nothing of A's");
    }
    assert_eq!(driver_maps(&gpu, A).len(), 2);
    // Binding another process's buffer is a GPU read/write of it: only
    // a holder may. PRIME makes A a holder.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(hb, VA + 0x10_0000, 4096)]),
        Err(nv::ENOENT)
    );
    assert!(crate::scheme::gem_mmap::add_ref(hb, A).is_some());
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(hb, VA + 0x10_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A).len(), 3);
    // UNMAP is by range, scoped to the caller: the page of A's mapping
    // goes (the rest of that mapping stays, offset moved along), B's
    // identical VA stays. An empty range is a success.
    assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x8000, 4096)]), Ok(0));
    assert_eq!(
        driver_maps(&gpu, A)
            .iter()
            .map(|m| (m.0, m.1, m.2))
            .collect::<Vec<_>>(),
        [
            (ha, VA, 0x8000),
            (hb, VA + 0x10_0000, 4096),
            (ha, VA + 0x9000, 65536 - 4096)
        ]
    );
    assert_eq!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .find(|m| m.va == VA + 0x9000)
            .map(|m| m.bo_offset),
        Some(0x1000),
        "the tail starts a page into the object"
    );
    assert_eq!(
        FAKE_RM.lock().maps_of_ctx(2).len(),
        1,
        "B's binding is still live"
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(0x1000, 0x1000)]),
        Ok(0),
        "nothing there: fine"
    );
    assert_eq!(FAKE_RM.lock().unmaps, 4);
    // Both of A's own pieces, whole, in one range.
    assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA, 0x20000)]), Ok(0));
    assert_eq!(
        driver_maps(&gpu, A).iter().map(|m| m.0).collect::<Vec<_>>(),
        [hb]
    );
    assert_eq!(FAKE_RM.lock().unmaps, 6);
    // MAP with handle 0 is Mesa's "unbind, keep the reservation", and
    // it is scoped like UNMAP: B's mapping at the same VA is not A's.
    assert_eq!(
        vm_bind_ops(&gpu, B, &mut [map(hb, VA + 0x10_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(0, VA + 0x10_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A), []);
    assert_eq!(FAKE_RM.lock().maps_of_ctx(1), []);
    assert_eq!(driver_maps(&gpu, B).len(), 2, "B's two bindings untouched");
    assert_eq!(FAKE_RM.lock().maps_of_ctx(2).len(), 2);
    // What the arm refuses before touching the RM.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [op(7, 0, ha, VA, 4096)]),
        Err(nv::EINVAL),
        "unknown op"
    );
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [op(nv::VM_BIND_OP_MAP, nv::VM_BIND_SPARSE, 0, VA, 4096)]
        ),
        Err(nv::EOPNOTSUPP),
        "sparse regions"
    );
    assert_eq!(vm_bind_ops(&gpu, A, &mut []), Err(nv::EINVAL), "no ops");
    let mut many: Vec<_> = (0..4097).map(|_| map(ha, VA, 4096)).collect();
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut many),
        Err(nv::EOPNOTSUPP),
        "4097 ops"
    );
    let mut r = nv::DrmNouveauVmBind {
        op_count: 1,
        flags: 0,
        wait_count: 1,
        sig_count: 0,
        wait_ptr: 0x1000,
        sig_ptr: 0,
        op_ptr: many.as_mut_ptr() as u64,
    };
    assert_eq!(
        call(&gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, A),
        Err(nv::EINVAL),
        "a synchronous VM_BIND (no RUN_ASYNC) carries no syncs"
    );
    assert_eq!(
        FAKE_RM.lock().calls.len(),
        before,
        "none of those reached the RM"
    );
    // A batch with a bad op in it does nothing at all: Linux checks
    // every op before the job runs.
    let mut ops = [
        map(ha, VA, 4096),
        map(ha + 1000, VA + 0x1000, 4096),
        map(ha, VA + 0x2000, 4096),
    ];
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(vm_bind_ops(&gpu, A, &mut ops), Err(nv::ENOENT));
    assert_eq!(driver_maps(&gpu, A), [], "op[0] was not applied either");
    assert_eq!(FAKE_RM.lock().calls.len(), before, "nothing reached the RM");
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 4096)]), Ok(0));
    assert_eq!(FAKE_RM.lock().maps_of_ctx(1), [(VA, 4096, 0x06)]);
    // The RM refused the map (a fixed VA it will not reserve): EIO, and
    // no binding is recorded for a mapping that does not exist.
    FAKE_RM.lock().refuse_map = true;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, VA + 0x4000, 4096)]),
        Err(nv::EIO)
    );
    FAKE_RM.lock().refuse_map = false;
    assert_eq!(driver_maps(&gpu, A).len(), 1);
    assert_eq!(gem_info(&gpu, ha, A).unwrap().offset, VA);
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// Linux's gpuvm answers an UNMAP over part of a mapping, or a MAP over
/// part of one, with REMAP ops: the head and the tail of the old mapping
/// stay, same object and kind, offsets moved along
/// (`drm_gpuvm_sm_unmap_ops_create` / `..._map_ops_create`,
/// `nouveau_uvmm.c` `op_remap`). NVK's bind context merges adjacent
/// `vkQueueBindSparse` binds into one mapping, so unbinding or rebinding
/// one page of a sparse-binding buffer later cuts through a larger one.
/// Before, any overlap took the whole old mapping, and the next draw
/// that touched the rest of it faulted.
#[test]
fn a_partial_unmap_or_map_over_keeps_the_parts_outside_the_range_like_a_remap() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    const VA: u64 = 0x3f_f000_0000;
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let h2 = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    // (handle, va, size, bo_offset, kind) as the driver records it.
    let parts = || -> Vec<(u32, u64, u64, u64, u32)> {
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.owner_pid == A)
            .map(|m| (m.gem_handle, m.va, m.size, m.bo_offset, m.pte_kind))
            .collect()
    };
    // (va, size, bo_offset, kind) as the RM has it.
    let rm_parts = || -> Vec<(u64, u64, u64, u32)> {
        FAKE_RM
            .lock()
            .maps
            .iter()
            .filter(|m| m.1 == 1)
            .map(|m| (m.3, m.4, m.5, m.6))
            .collect()
    };
    // Fifteen pages of `h`, a page into the object, kind 0x06.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map_at(h, VA, 0xF000, 0x1000)]),
        Ok(0)
    );
    assert_eq!(parts(), [(h, VA, 0xF000, 0x1000, 0x06)]);
    // The middle page goes: head and tail stay, the tail's offset moved
    // along by the pages before it.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x4000, 0x1000)]),
        Ok(0)
    );
    assert_eq!(
        parts(),
        [
            (h, VA, 0x4000, 0x1000, 0x06),
            (h, VA + 0x5000, 0xA000, 0x6000, 0x06)
        ]
    );
    assert_eq!(
        rm_parts(),
        [
            (VA, 0x4000, 0x1000, 0x06),
            (VA + 0x5000, 0xA000, 0x6000, 0x06)
        ],
        "the RM was asked for exactly those two, the whole old one gone"
    );
    assert_eq!(FAKE_RM.lock().unmaps, 1);
    // The head of the first piece: only its tail stays.
    assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA, 0x2000)]), Ok(0));
    assert_eq!(
        parts(),
        [
            (h, VA + 0x5000, 0xA000, 0x6000, 0x06),
            (h, VA + 0x2000, 0x2000, 0x3000, 0x06)
        ]
    );
    // The tail of the second piece, with a range running past its end.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0xC000, 0x4000)]),
        Ok(0)
    );
    assert_eq!(
        parts(),
        [
            (h, VA + 0x2000, 0x2000, 0x3000, 0x06),
            (h, VA + 0x5000, 0x7000, 0x6000, 0x06)
        ]
    );
    assert_eq!(FAKE_RM.lock().unmaps, 3);
    // A MAP of another object over the middle of a piece: head, tail,
    // then the new mapping.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(h2, VA + 0x7000, 0x2000)]),
        Ok(0)
    );
    assert_eq!(
        parts(),
        [
            (h, VA + 0x2000, 0x2000, 0x3000, 0x06),
            (h, VA + 0x5000, 0x2000, 0x6000, 0x06),
            (h, VA + 0x9000, 0x3000, 0xA000, 0x06),
            (h2, VA + 0x7000, 0x2000, 0, 0x06)
        ]
    );
    assert_eq!(parts().len(), rm_parts().len());
    assert_eq!(FAKE_RM.lock().unmaps, 4);
    // One range cutting through two pieces at once: each keeps what
    // lies outside it.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x3000, 0x3000)]),
        Ok(0)
    );
    assert_eq!(
        parts(),
        [
            (h, VA + 0x9000, 0x3000, 0xA000, 0x06),
            (h2, VA + 0x7000, 0x2000, 0, 0x06),
            (h, VA + 0x2000, 0x1000, 0x3000, 0x06),
            (h, VA + 0x6000, 0x1000, 0x7000, 0x06)
        ]
    );
    assert_eq!(FAKE_RM.lock().unmaps, 6);
    assert_eq!(rm_parts().len(), 4);
    // The RM refusing to map a kept part again: the op still succeeds
    // (Linux cannot fail a remap either), the part is reported lost and
    // nothing pretends it is mapped.
    FAKE_RM.lock().refuse_map = true;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0xA000, 0x1000)]),
        Ok(0)
    );
    FAKE_RM.lock().refuse_map = false;
    assert_eq!(
        parts(),
        [
            (h2, VA + 0x7000, 0x2000, 0, 0x06),
            (h, VA + 0x2000, 0x1000, 0x3000, 0x06),
            (h, VA + 0x6000, 0x1000, 0x7000, 0x06)
        ]
    );
    assert_eq!(rm_parts().len(), 3);
    assert_eq!(FAKE_RM.lock().unmaps, 7);
    assert_eq!(FAKE_RM.lock().bad, 0);
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().maps_of_ctx(1), []);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn vm_bind_programs_uncompressed_kinds_verbatim_and_hands_the_rest_to_the_rm_default() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    channel_alloc(&gpu, A).unwrap();
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    // (kind asked, kind programmed)
    let cases = [
        (0x00, 0x00),
        (0x01, 0x01),
        (0x03, 0x03),
        (0x06, 0x06),
        (0x0a, 0x0a),
        (0x0f, 0x0f),
        (0x07, 0x00),
        (0x20, 0x00),
        (0xff, 0x00),
    ];
    for (i, (asked, _)) in cases.iter().enumerate() {
        let va = 0x1000_0000 + (i as u64) * 0x1_0000;
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [op(nv::VM_BIND_OP_MAP, *asked, h, va, 4096)]),
            Ok(0),
            "a kind is never refused: kind {:#x}",
            asked
        );
    }
    let programmed: Vec<u32> = FAKE_RM.lock().maps_of_ctx(1).iter().map(|m| m.2).collect();
    let expected: Vec<u32> = cases.iter().map(|c| c.1).collect();
    assert_eq!(programmed, expected);
    // Bits above the kind byte (bit 8 is SPARSE, refused elsewhere) are
    // not a kind: 0x0200 programs the default, not 0x02, and 0x0206 is
    // still GENERIC.
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [op(nv::VM_BIND_OP_MAP, 0x0200, h, 0x2000_0000, 4096)]
        ),
        Ok(0)
    );
    assert_eq!(
        FAKE_RM.lock().maps_of_ctx(1).last().map(|m| m.2),
        Some(0x00)
    );
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [op(nv::VM_BIND_OP_MAP, 0x0206, h, 0x2001_0000, 4096)]
        ),
        Ok(0)
    );
    assert_eq!(
        FAKE_RM.lock().maps_of_ctx(1).last().map(|m| m.2),
        Some(0x06)
    );
    gpu.nouveau_release_process(A);
}

#[test]
fn with_the_rm_a_subchannel_new_builds_the_class_on_the_callers_context_and_channel_free_reaps_it()
{
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    channel_alloc(&gpu, A).unwrap();
    channel_alloc(&gpu, B).unwrap();
    assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
    assert_eq!(subchan_new(0, 0xc5c0, 0x1008).send(&gpu, A), Ok(0));
    assert_eq!(
        subchan_new(1, 0xc597, 0x1000).send(&gpu, B),
        Ok(0),
        "B's own, same cookie"
    );
    {
        let f = FAKE_RM.lock();
        assert_eq!(
            f.classes.iter().map(|c| (c.1, c.2)).collect::<Vec<_>>(),
            [(1, 0xc597), (1, 0xc5c0), (2, 0xc597)],
            "each on its caller's context, never on ctx 0"
        );
    }
    // DEL frees the RM object, the caller's only.
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 0, 0x1000, HDR).send(&gpu, B),
        Ok(0)
    );
    assert_eq!(FAKE_RM.lock().class_frees, 1);
    assert_eq!(
        FAKE_RM
            .lock()
            .classes
            .iter()
            .map(|c| c.1)
            .collect::<Vec<_>>(),
        [1, 1]
    );
    // CHANNEL_FREE reaps what was left without DEL, on that channel
    // only, and leaves the context and its bindings alone (the VAS is
    // shared by every channel of the pid).
    let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(h, 0x5000_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(channel_free(&gpu, 0, A), Ok(0));
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.class_frees, 3);
        assert_eq!(f.classes, []);
        assert_eq!(f.ctxs, [1, 2], "no ctx_free on CHANNEL_FREE");
        assert_eq!(f.maps.len(), 1);
        assert_eq!(f.bad, 0);
    }
    assert!(nv::class_objects_drain_pid(A).is_empty());
    // The RM refuses the class: the NEW fails rather than leaving a
    // class NVK will submit methods of.
    channel_alloc(&gpu, A).unwrap();
    FAKE_RM.lock().refuse_class = true;
    assert_eq!(
        subchan_new(0, 0xc597, 0x2000).send(&gpu, A),
        Err(nv::EINVAL)
    );
    FAKE_RM.lock().refuse_class = false;
    assert!(nv::class_objects_drain_pid(A).is_empty());
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
}

#[test]
fn process_exit_with_the_rm_frees_classes_then_the_context_then_memory_and_never_unmaps_a_dead_vas()
{
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    channel_alloc(&gpu, A).unwrap();
    channel_alloc(&gpu, B).unwrap();
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let hb = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_VRAM, B)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, 0x1000_0000, 65536)]),
        Ok(0)
    );
    assert_eq!(
        vm_bind_ops(&gpu, B, &mut [map(hb, 0x1000_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(A);
    let f = FAKE_RM.lock();
    assert_eq!(
        &f.calls[before..],
        ["class_free", "ctx_free", "gem_free"],
        "child before parent, GPU stopped before its memory goes"
    );
    assert_eq!(
        f.unmaps, 0,
        "ctx_free took the VAS: a second unmap would be a use-after-free"
    );
    assert_eq!(f.ctxs, [2]);
    assert_eq!(f.maps_of_ctx(2).len(), 1, "B's binding is untouched");
    assert_eq!(f.gems.len(), 1);
    assert_eq!(f.classes, []);
    assert_eq!(f.bad, 0);
    drop(f);
    assert_eq!(ctx_of(&gpu, A), None);
    assert_eq!(driver_maps(&gpu, A), []);
    assert!(!has_object(&gpu, ha));
    assert!(has_object(&gpu, hb));
    assert_eq!(rm_backed_channels(&gpu, A), 0);
    assert_eq!(rm_backed_channels(&gpu, B), 1);
    // A comes back: a fresh context in the freed slot, primed again.
    channel_alloc(&gpu, A).unwrap();
    assert_eq!(ctx_of(&gpu, A), Some((1, true)));
    assert_eq!(FAKE_RM.lock().primed, [1, 2, 1]);
    gpu.nouveau_release_process(A);
    // ctx_free refused for B: its VAS is still alive in the RM, so each
    // binding IS unmapped before its memory is freed (freeing the backing
    // under a live h_virt is a use-after-free in the vendor RM).
    FAKE_RM.lock().fail_ctx_free = true;
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(B);
    FAKE_RM.lock().fail_ctx_free = false;
    {
        let f = FAKE_RM.lock();
        assert_eq!(
            &f.calls[before..],
            ["ctx_free", "vm_bind_unmap", "gem_free"]
        );
        assert_eq!(f.unmaps, 1);
        assert_eq!(f.maps, [], "B's binding went through the RM");
        assert_eq!(f.ctxs, [2], "the context the RM would not free stays");
        assert_eq!(f.bad, 0);
        assert_eq!(f.gems, []);
    }
    assert_eq!(ctx_of(&gpu, B), None, "forgotten locally either way");
}

// ----- EXEC through the fake: the RM per-submit path -----
//
// `exec_fast_prepare` stays refused in this binary, so every EXEC goes
// the way it does on a context whose direct-submit setup failed: one
// RM call per push, the last one carrying the fence the syncobjs of
// `sig` wait for. The fake's fence lands before the call returns
// unless told to stall.

use crate::scheme::syncobj;

/// Where the client's pushbuffer object is bound.
const PUSH_VA: u64 = 0x7f_0000_0000;

fn sync(handle: u32) -> nv::DrmNouveauSync {
    nv::DrmNouveauSync {
        flags: 0,
        handle,
        timeline_value: 0,
    }
}

fn sync_tl(handle: u32, point: u64) -> nv::DrmNouveauSync {
    nv::DrmNouveauSync {
        flags: nv::SYNC_TIMELINE_SYNCOBJ,
        handle,
        timeline_value: point,
    }
}

fn push(va: u64, va_len: u32) -> nv::DrmNouveauExecPush {
    nv::DrmNouveauExecPush {
        va,
        va_len,
        flags: 0,
    }
}

/// A push the GPU is still writing when it is submitted.
fn push_no_prefetch(va: u64, va_len: u32) -> nv::DrmNouveauExecPush {
    nv::DrmNouveauExecPush {
        va,
        va_len,
        flags: nv::EXEC_PUSH_NO_PREFETCH,
    }
}

fn ptr_of<T>(items: &[T]) -> u64 {
    if items.is_empty() {
        0
    } else {
        items.as_ptr() as u64
    }
}

fn exec(
    gpu: &NvidiaGpu,
    pid: u64,
    channel: u32,
    pushes: &[nv::DrmNouveauExecPush],
    waits: &[nv::DrmNouveauSync],
    sigs: &[nv::DrmNouveauSync],
) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauExec {
        channel,
        push_count: pushes.len() as u32,
        wait_count: waits.len() as u32,
        sig_count: sigs.len() as u32,
        wait_ptr: ptr_of(waits),
        sig_ptr: ptr_of(sigs),
        push_ptr: ptr_of(pushes),
    };
    exec_raw(gpu, pid, &mut r)
}

fn exec_raw(gpu: &NvidiaGpu, pid: u64, r: &mut nv::DrmNouveauExec) -> Result<usize, i32> {
    with_uvmm(gpu, pid);
    call(gpu, wr::<nv::DrmNouveauExec>(nv::NR_EXEC), r, pid)
}

/// A client with its own context (channel 0 of this GPU when called
/// first) and a 64 KiB GART object bound at `PUSH_VA`.
fn client_with_pushbuf(gpu: &NvidiaGpu, pid: u64) -> u32 {
    let channel = channel_alloc(gpu, pid).unwrap().channel;
    let h = gem_new_rm(gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, pid)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(gpu, pid, &mut [map(h, PUSH_VA, 65536)]), Ok(0));
    channel as u32
}

fn rm_calls_since(before: usize) -> Vec<&'static str> {
    FAKE_RM.lock().calls[before..].to_vec()
}

/// `nouveau_uvmm_ioctl_vm_init` checks the kernel-managed range against
/// the VA space (EINVAL past it), and `nouveau_uvmm_ioctl_vm_bind` and
/// `nouveau_exec_ioctl_exec` answer ENOSYS to a client that never ran
/// it, before looking at anything else. This driver accepted any range,
/// kept no record of it, and sent such a client on to its own gates.
#[test]
fn vm_bind_and_exec_answer_enosys_until_the_client_ran_vm_init() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let init = |pid: u64, addr: u64, size: u64| {
        let mut r = nv::DrmNouveauVmInit {
            kernel_managed_addr: addr,
            kernel_managed_size: size,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauVmInit>(nv::NR_VM_INIT),
            &mut r,
            pid,
        )
    };
    // An empty synchronous request: EINVAL once past the gates, so a
    // gate that lets it through shows as EINVAL rather than as a bind.
    let bind = |pid: u64| {
        let mut r = nv::DrmNouveauVmBind {
            op_count: 0,
            flags: 0,
            wait_count: 0,
            sig_count: 0,
            wait_ptr: 0,
            sig_ptr: 0,
            op_ptr: 0,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND),
            &mut r,
            pid,
        )
    };
    let submit = |pid: u64| {
        let p = [push(PUSH_VA, 16)];
        let mut r = nv::DrmNouveauExec {
            channel: 0,
            push_count: 1,
            wait_count: 0,
            sig_count: 0,
            wait_ptr: 0,
            sig_ptr: 0,
            push_ptr: ptr_of(&p),
        };
        call(&gpu, wr::<nv::DrmNouveauExec>(nv::NR_EXEC), &mut r, pid)
    };
    // Before VM_INIT: ENOSYS, ahead of the "no RM channel" ENODEV.
    assert_eq!(bind(A), Err(nv::ENOSYS));
    assert_eq!(submit(A), Err(nv::ENOSYS));
    // A range past the VA space is EINVAL, and does not count.
    const END: u64 = nv::NOUVEAU_VA_SPACE_END;
    assert_eq!(init(A, u64::MAX - 4095, 8192), Err(nv::EINVAL), "overflow");
    assert_eq!(init(A, END - 4096, 8192), Err(nv::EINVAL), "a page past");
    assert_eq!(init(A, 0, END + 1), Err(nv::EINVAL));
    assert_eq!(bind(A), Err(nv::ENOSYS), "a refused VM_INIT made no uvmm");
    assert_eq!(submit(A), Err(nv::ENOSYS));
    // One ending exactly at the end fits: on to the next gates.
    assert_eq!(init(A, END - 4096, 4096), Ok(0));
    assert_eq!(bind(A), Err(nv::ENODEV));
    assert_eq!(submit(A), Err(nv::ENODEV));
    // Again is fine.
    assert_eq!(init(A, 0, 1 << 32), Ok(0));
    assert_eq!(bind(A), Err(nv::ENODEV));
    // It is this client's, not the GPU's.
    assert_eq!(bind(B), Err(nv::ENOSYS));
    assert_eq!(submit(B), Err(nv::ENOSYS));
    // The client's exit takes its uvmm with it, and nobody else's.
    assert_eq!(init(B, 0, 1 << 32), Ok(0));
    gpu.nouveau_release_process(A);
    assert_eq!(bind(A), Err(nv::ENOSYS));
    assert_eq!(bind(B), Err(nv::ENODEV));
    // The normal order for a fresh client: VM_INIT, channel, object,
    // bind (`client_with_pushbuf` asserts the bind went through).
    with_uvmm(&gpu, STRANGER);
    let _ch = client_with_pushbuf(&gpu, STRANGER);
    // The GPU's VA space being up changes nothing for a client without
    // a uvmm, and a channel is not one: a legacy client (CHANNEL_ALLOC,
    // no VM_INIT) gets ENOSYS from the new-uAPI ioctls, as in Linux.
    assert_eq!(bind(A), Err(nv::ENOSYS));
    assert_eq!(submit(A), Err(nv::ENOSYS));
    gpu.nouveau_release_process(B);
    channel_alloc(&gpu, B).unwrap();
    assert_eq!(bind(B), Err(nv::ENOSYS));
    assert_eq!(submit(B), Err(nv::ENOSYS));
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(STRANGER);
}

#[test]
fn exec_needs_the_callers_own_rm_channel_and_a_well_formed_request() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let p = [push(PUSH_VA, 16)];
    // No RM-backed channel on this GPU at all.
    assert_eq!(exec(&gpu, A, 0, &p, &[], &[]), Err(nv::ENODEV));
    // A discovery channel is not a GPFIFO.
    FAKE_RM.lock().fail_ctx_alloc = true;
    let disc = channel_alloc(&gpu, STRANGER).unwrap().channel as u32;
    FAKE_RM.lock().fail_ctx_alloc = false;
    assert_eq!(exec(&gpu, STRANGER, disc, &p, &[], &[]), Err(nv::ENODEV));
    let ch = client_with_pushbuf(&gpu, A);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, STRANGER, disc, &p, &[], &[]),
        Err(nv::ENODEV),
        "still discovery-only, whoever else has a real one"
    );
    assert_eq!(
        exec(&gpu, B, ch, &p, &[], &[]),
        Err(nv::ENODEV),
        "B owns no RM channel"
    );
    channel_alloc(&gpu, B).unwrap();
    assert_eq!(
        exec(&gpu, B, ch, &p, &[], &[]),
        Err(nv::ENOENT),
        "B has one, but this channel id is A's: not in B's list (Linux ENOENT)"
    );
    assert_eq!(exec(&gpu, A, ch + 7, &p, &[], &[]), Err(nv::ENOENT));
    // A channel opened before the RM attached is discovery-only for
    // good, even once its owner has a real one: the client must free it
    // and CHANNEL_ALLOC again (the driver's own log says so).
    *gpu.rm_device_instance.lock() = None;
    let old = channel_alloc(&gpu, B).unwrap().channel as u32;
    *gpu.rm_device_instance.lock() = Some(0);
    assert_eq!(exec(&gpu, B, old, &p, &[], &[]), Err(nv::ENODEV));
    assert_eq!(channel_free(&gpu, old as i32, B), Ok(0));
    // The shape of the request, before any push is looked at.
    let too_many: Vec<_> = (0..65).map(|_| push(PUSH_VA, 16)).collect();
    assert_eq!(
        exec(&gpu, A, ch, &too_many, &[], &[]),
        Err(nv::EOPNOTSUPP),
        "65 pushes"
    );
    let mut r = nv::DrmNouveauExec {
        channel: ch,
        push_count: 1,
        wait_count: 0,
        sig_count: 0,
        wait_ptr: 0,
        sig_ptr: 0,
        push_ptr: 0,
    };
    assert_eq!(
        exec_raw(&gpu, A, &mut r),
        Err(nv::EFAULT),
        "null pushes: a user range check, like any null array"
    );
    r.push_ptr = p.as_ptr() as u64;
    r.wait_count = 1;
    assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "null waits");
    // One more than NVK's batch (256) of real, satisfied syncs: a cap
    // that let them through would submit, not crash.
    let ready = syncobj::create(true);
    let many_syncs: Vec<_> = (0..257).map(|_| sync(ready)).collect();
    r.wait_count = 257;
    r.wait_ptr = many_syncs.as_ptr() as u64;
    assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "257 waits");
    r.wait_count = 0;
    r.wait_ptr = 0;
    r.sig_count = 1;
    assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "null sigs");
    r.sig_count = 257;
    r.sig_ptr = many_syncs.as_ptr() as u64;
    assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "257 sigs");
    assert!(syncobj::destroy(ready));
    r.sig_count = 0;
    r.push_ptr = 0xffff_ffff_ffff_0000;
    assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EFAULT), "kernel address");
    // Pushes are dword streams.
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 0)], &[], &[]),
        Err(nv::EINVAL)
    );
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16), push(PUSH_VA + 16, 6)],
            &[],
            &[]
        ),
        Err(nv::EINVAL),
        "checked for every push before any is submitted"
    );
    assert_eq!(
        rm_calls_since(before),
        ["ctx_alloc", "ctx_prime"],
        "none of that reached the ring (B's context build did)"
    );
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(STRANGER);
}

#[test]
fn exec_submits_every_push_in_order_and_signals_the_syncobjs_after_the_fence_lands() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let legacy = nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed);
    let before = FAKE_RM.lock().calls.len();
    // No signal: every push is a plain submit, nothing to wait for.
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 32)],
            &[],
            &[]
        ),
        Ok(0)
    );
    assert_eq!(
        rm_calls_since(before),
        ["exec_fast_prepare", "exec_submit", "exec_submit"],
        "the direct-submit setup is tried once and refused here: one RM entry per push"
    );
    assert_eq!(
        FAKE_RM.lock().submits,
        [(1, PUSH_VA, 16, None), (1, PUSH_VA + 0x100, 32, None)],
        "on the caller's own context, in order"
    );
    assert_eq!(nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed), legacy + 1);
    // With signals: the LAST push carries the fence; the syncobjs are
    // signaled only once it landed, a binary one to 1 and a timeline
    // one to the point asked for.
    let bin = syncobj::create(false);
    let tl = syncobj::create(false);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[
                push(PUSH_VA, 16),
                push(PUSH_VA + 0x100, 32),
                push(PUSH_VA + 0x200, 8)
            ],
            &[],
            &[sync(bin), sync_tl(tl, 5)]
        ),
        Ok(0)
    );
    assert_eq!(
        rm_calls_since(before),
        ["exec_submit", "exec_submit", "exec_submit_signaled"]
    );
    {
        let f = FAKE_RM.lock();
        let last = f.submits.last().copied().unwrap();
        assert_eq!((last.0, last.1, last.2), (1, PUSH_VA + 0x200, 8));
        assert!(
            last.3.is_some_and(|p| p & 0x8000_0000 != 0),
            "a fresh fence payload"
        );
        assert!(
            f.submits[2..4].iter().all(|s| s.3.is_none()),
            "the earlier pushes carry none"
        );
    }
    assert_eq!(syncobj::query(bin), Some(1));
    assert_eq!(syncobj::query(tl), Some(5));
    // A signal on a handle nobody has: refused before the RM sees a push.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(0xdead_0000)]),
        Err(nv::ENOENT)
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // Two channels of one process share the context and the ring.
    let ch2 = channel_alloc(&gpu, A).unwrap().channel as u32;
    assert_ne!(ch2, ch);
    assert_eq!(exec(&gpu, A, ch2, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(FAKE_RM.lock().submits.last().unwrap().0, 1);
    assert!(syncobj::destroy(bin));
    assert!(syncobj::destroy(tl));
    gpu.nouveau_release_process(A);
}

#[test]
fn exec_waits_on_the_cpu_before_submitting_and_never_submits_after_a_wait_fails() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let done = syncobj::create(true);
    let tl = syncobj::create(false);
    assert!(syncobj::timeline_signal(tl, 3));
    let out = syncobj::create(false);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16)],
            &[sync(done), sync_tl(tl, 3)],
            &[sync(out)]
        ),
        Ok(0),
        "both waits already satisfied"
    );
    assert_eq!(
        rm_calls_since(before),
        ["exec_fast_prepare", "exec_submit_signaled"]
    );
    assert_eq!(syncobj::query(out), Some(1));
    // An unknown wait handle: ENOENT, and the ring never heard of it.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16)],
            &[sync(0xdead_0001)],
            &[sync(out)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // A wait that never comes: the 10 s deadline (the clock advances
    // 1 ms per read here) ends in EIO, the pushes are NOT submitted and
    // the sig list is NOT signaled -- NVK sees device-lost, not a
    // frame that was never drawn.
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 4)],
            &[sync_tl(out, 7)]
        ),
        Err(nv::EIO),
        "the timeline is at 3: point 4 is a wait, not a pass"
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    let never = syncobj::create(false);
    let waited = nv::EXEC_WAIT_US.load(Ordering::Relaxed);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 4), sync(never)],
            &[sync_tl(out, 7)]
        ),
        Err(nv::EIO)
    );
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(syncobj::query(out), Some(1), "not signaled");
    assert!(
        nv::EXEC_WAIT_US.load(Ordering::Relaxed) - waited >= 10_000_000,
        "the wait is accounted"
    );
    for h in [done, tl, out, never] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
}

#[test]
fn exec_with_no_pushes_is_the_health_probe_that_waits_and_signals_without_the_ring() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let a = syncobj::create(true);
    let b = syncobj::create(false);
    let out = syncobj::create(false);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, A, ch, &[], &[sync(a)], &[sync(out), sync_tl(b, 9)]),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0], "no RM call at all");
    assert_eq!(syncobj::query(out), Some(1));
    assert_eq!(syncobj::query(b), Some(9));
    assert_eq!(
        exec(&gpu, A, ch, &[], &[sync(0xdead_0002)], &[sync(out)]),
        Err(nv::ENOENT)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[], &[], &[sync(0xdead_0003)]),
        Err(nv::ENOENT)
    );
    // Still a health probe: a wedged context answers ENODEV.
    nv::ctx_set_wedged(1);
    assert_eq!(exec(&gpu, A, ch, &[], &[], &[sync(out)]), Err(nv::ENODEV));
    nv::ctx_clear_wedged(1);
    assert_eq!(exec(&gpu, A, ch, &[], &[], &[sync_tl(out, 2)]), Ok(0));
    assert_eq!(syncobj::query(out), Some(2));
    for h in [a, b, out] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
}

/// `vkQueueWaitIdle` / `vkDeviceWaitIdle` is an EXEC with no push and
/// one sig, then a WAIT on it (NVK's `nvkmd_nouveau_exec_ctx_sync`), and
/// a `vkQueueSubmit` with no command buffer and a fence is the same
/// shape. Linux queues the empty job behind the channel's earlier jobs
/// and gives its sigs the job's fence (`nouveau_exec_job_run` emits one
/// even for `push.count == 0`), so they signal once everything queued
/// before has run. Signaled from the CPU, they went off while the ring
/// was still fetching the pushes of the submit before: WaitIdle returned
/// early, and the application re-recorded command buffers the GPU was
/// still reading. The sigs now ride a probe fence appended behind the
/// queued work; an idle ring has nothing to wait for.
/// The probe names a channel like any other EXEC, and Linux looks that
/// channel up before anything else (`nouveau_exec_ioctl_exec`): an id the
/// caller does not hold is ENOENT with nothing signaled. This arm used
/// to answer 0 and signal the sigs for any id at all -- a freed channel,
/// another process's, or one that never existed.
#[test]
fn an_empty_exec_needs_the_callers_own_channel_like_a_real_one() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch + 7, &[], &[], &[sync(out)]),
        Err(nv::ENOENT),
        "a channel that never existed"
    );
    assert_eq!(syncobj::query(out), Some(0), "nothing signaled");
    // B has a channel of its own; A's id is not in B's list.
    let ch_b = channel_alloc(&gpu, B).unwrap().channel as u32;
    assert_ne!(ch_b, ch);
    assert_eq!(
        exec(&gpu, B, ch, &[], &[], &[sync(out)]),
        Err(nv::ENOENT),
        "another process's channel"
    );
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[], &[sync(out)]),
        Ok(0),
        "B's own"
    );
    assert_eq!(syncobj::query(out), Some(1));
    // A channel the caller freed is gone for the probe too (with a
    // second channel still open, so the "no RM channel at all" gate at
    // the top of the arm does not answer first).
    let ch2 = channel_alloc(&gpu, A).unwrap().channel as u32;
    assert_eq!(channel_free(&gpu, ch as i32, A), Ok(0));
    assert_eq!(exec(&gpu, A, ch, &[], &[], &[]), Err(nv::ENOENT), "freed");
    assert_eq!(exec(&gpu, A, ch2, &[], &[], &[]), Ok(0), "the live one");
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(A);
}

#[test]
fn an_empty_exec_signals_behind_the_work_the_ring_still_has_queued() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    let tl = syncobj::create(false);
    // Work queued, not run, and with no fence of its own.
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(
        exec(&gpu, A, ch, &[], &[], &[sync(out), sync_tl(tl, 4)]),
        Ok(0)
    );
    assert_eq!(
        syncobj::query(out),
        Some(0),
        "the ring has not run the push yet: WaitIdle must not return"
    );
    assert_eq!(syncobj::query(tl), Some(0));
    assert_eq!(
        syncobj::query_submitted(tl),
        Some(4),
        "submitted, on a fence of its own"
    );
    let fences = syncobj::pending_hw_fences(out, 1);
    assert_eq!(fences.len(), 1, "one fence, the probe's");
    assert_ne!(
        fences[0].1, 0,
        "a GPU ACQUIRE for a consumer, not a CPU wait"
    );
    assert!(!run_gpu(1).is_empty(), "the push and the probe fence ran");
    syncobj::poll_pending();
    assert_eq!(syncobj::query(out), Some(1));
    assert_eq!(syncobj::query(tl), Some(4));
    // An idle ring has nothing to wait for: the CPU's word stands.
    let idle = syncobj::create(false);
    assert_eq!(exec(&gpu, A, ch, &[], &[], &[sync(idle)]), Ok(0));
    assert_eq!(
        syncobj::query(idle),
        Some(1),
        "nothing queued: signaled at once"
    );
    // Behind a wait settled on the CPU it is still the ring's word.
    let done = syncobj::create(true);
    let after = syncobj::create(false);
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(exec(&gpu, A, ch, &[], &[sync(done)], &[sync(after)]), Ok(0));
    assert_eq!(
        syncobj::query(after),
        Some(0),
        "the wait was done; the ring is not"
    );
    assert!(!run_gpu(1).is_empty());
    syncobj::poll_pending();
    assert_eq!(syncobj::query(after), Some(1));
    // NVK never resets the syncobj it syncs on: the second WaitIdle
    // signals a binary syncobj that is already signaled, and a binary
    // sig REPLACES its fence (`drm_syncobj_replace_fence`), so it goes
    // back to waiting for this ring, not "signaled already".
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(exec(&gpu, A, ch, &[], &[], &[sync(out)]), Ok(0));
    assert_eq!(
        syncobj::query(out),
        Some(0),
        "re-armed on the ring's new fence, as NVK's second WaitIdle needs"
    );
    assert!(!run_gpu(1).is_empty());
    syncobj::poll_pending();
    assert_eq!(syncobj::query(out), Some(1));
    for h in [out, tl, idle, done, after] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
}

#[test]
fn exec_failures_of_the_ring_are_eio_and_only_a_lost_fence_wedges_the_context_until_exit() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    // A push outside the caller's bindings: the RM's lookup refuses it
    // (on hardware it would MMU-fault the channel).
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA + 0x10000, 16)], &[], &[]),
        Err(nv::EIO)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA + 0xfff8, 16)], &[], &[]),
        Err(nv::EIO),
        "crossing the end of the binding"
    );
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[
                push(PUSH_VA, 16),
                push(PUSH_VA + 0x10000, 16),
                push(PUSH_VA, 16)
            ],
            &[],
            &[]
        ),
        Err(nv::EIO)
    );
    assert_eq!(
        FAKE_RM.lock().submits.len(),
        4,
        "stopped at the refused push; the third never went"
    );
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[
                push(PUSH_VA, 16),
                push(PUSH_VA + 0x10000, 16),
                push(PUSH_VA, 16)
            ],
            &[],
            &[sync(out)]
        ),
        Err(nv::EIO),
        "the same with a fence on the last push"
    );
    assert_eq!(FAKE_RM.lock().submits.len(), 6);
    assert_eq!(syncobj::query(out), Some(0));
    // The ring is full: EIO too, and the fence was never asked for.
    FAKE_RM.lock().ring_full = true;
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Err(nv::EIO)
    );
    FAKE_RM.lock().ring_full = false;
    assert_eq!(syncobj::query(out), Some(0));
    assert!(!nv::ctx_is_wedged(1), "a refused submit is not a hang");
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0),
        "and the next one is fine"
    );
    // The push went in but its fence never lands: after the 1 s poll
    // (1 ms per clock read) the submit is EIO and the context is
    // latched WEDGED, so the client fast-fails from then on instead of
    // hanging the compositor's ring behind it.
    FAKE_RM.lock().fence_stalls = true;
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync_tl(out, 2)]),
        Err(nv::EIO)
    );
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    FAKE_RM.lock().fence_stalls = false;
    assert_eq!(syncobj::query(out), Some(1), "not signaled");
    assert!(nv::ctx_is_wedged(1));
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
        Err(nv::EIO),
        "wedged: nothing more reaches the ring"
    );
    assert_eq!(exec(&gpu, A, ch, &[], &[], &[]), Err(nv::ENODEV));
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // B is unaffected: its own context.
    client_with_pushbuf(&gpu, B);
    assert_eq!(exec(&gpu, B, 1, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    // A exits and comes back: the slot is clean again.
    gpu.nouveau_release_process(A);
    assert!(!nv::ctx_is_wedged(1));
    let ch = client_with_pushbuf(&gpu, A);
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// nouveau kills the channel a hang belongs to and only it: the
/// process's next CHANNEL_ALLOC is a new fifo channel that works, which
/// is how a client recovers from VK_ERROR_DEVICE_LOST (destroy the
/// device, its channel with it; create another). Here the verdict was
/// the process's for life, so the device made to recover was born dead.
#[test]
fn a_new_channel_after_the_wedged_one_is_freed_gets_a_fresh_context() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = channel_alloc(&gpu, A).unwrap().channel as u32;
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, PUSH_VA, 65536)]), Ok(0));
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    FAKE_RM.lock().fence_stalls = true;
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync_tl(out, 2)]),
        Err(nv::EIO)
    );
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    FAKE_RM.lock().fence_stalls = false;
    assert!(nv::ctx_is_wedged(1));
    // A second channel while the first is still open shares its VAS:
    // the same context, wedged as it is (NVK's other instance in the
    // same process would be rendering in that VAS).
    let before = FAKE_RM.lock().calls.len();
    let ch2 = channel_alloc(&gpu, A).unwrap().channel as u32;
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(ctx_of(&gpu, A), Some((1, true)));
    assert!(nv::ctx_is_wedged(1));
    assert_eq!(
        exec(&gpu, A, ch2, &[push(PUSH_VA, 16)], &[], &[]),
        Err(nv::EIO)
    );
    assert_eq!(channel_free(&gpu, ch as i32, A), Ok(0));
    assert_eq!(channel_free(&gpu, ch2 as i32, A), Ok(0));
    assert!(nv::ctx_is_wedged(1), "freeing channels judges nothing");
    // No channel left: the next CHANNEL_ALLOC retires the wedged context
    // and the channel it hands out is on a fresh one.
    let before = FAKE_RM.lock().calls.len();
    let bytes = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed);
    let ch3 = channel_alloc(&gpu, A).unwrap().channel as u32;
    let calls = rm_calls_since(before);
    let freed = calls.iter().position(|c| *c == "ctx_free");
    let built = calls.iter().position(|c| *c == "ctx_alloc");
    assert!(
        matches!((freed, built), (Some(f), Some(b)) if f < b),
        "the old context freed, then a new one built: {:?}",
        calls
    );
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(ctx_of(&gpu, A), Some((1, true)));
    assert!(gpu.nouveau_ctx_teardown.lock().is_empty());
    assert!(gpu.nouveau_zombies.lock().is_empty());
    // The VAS went with the context, and the mappings in it; the
    // process's buffers are its own still.
    assert!(
        !gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .any(|m| m.owner_pid == A),
        "no mapping survives its VAS"
    );
    assert!(
        !calls.contains(&"vm_bind_unmap"),
        "gone with the VAS: {:?}",
        calls
    );
    assert!(
        !calls.contains(&"gem_free"),
        "the buffer is the process's: {:?}",
        calls
    );
    assert_eq!(NOUVEAU_GEM_BYTES.load(Ordering::Relaxed), bytes);
    assert!(gem_info(&gpu, h, A).is_ok());
    assert_eq!(
        exec(&gpu, A, ch3, &[push(PUSH_VA, 16)], &[], &[]),
        Err(nv::EIO),
        "nothing is bound at the old VA"
    );
    assert!(!nv::ctx_is_wedged(1), "a refused submit is not a hang");
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, PUSH_VA, 65536)]), Ok(0));
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch3, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0),
        "renders again"
    );
    assert_eq!(syncobj::query(out2), Some(1));
    // A process with a live context that is not wedged is left alone by
    // the same path.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(channel_free(&gpu, ch3 as i32, A), Ok(0));
    let ch4 = channel_alloc(&gpu, A).unwrap().channel as u32;
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(
        exec(&gpu, A, ch4, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0),
        "still bound: the context stayed"
    );
    assert!(syncobj::destroy(out));
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The index of a context being retired is the RM's until `ctx_free`
/// returns: a newcomer arriving inside that window gets another, as
/// on exit -- or the RM would free the newcomer's context.
#[test]
fn the_index_of_a_wedged_context_being_retired_is_not_handed_to_a_newcomer() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu: &'static NvidiaGpu = alloc::boxed::Box::leak(alloc::boxed::Box::new(gpu_rm()));
    let ch = client_with_pushbuf(gpu, A);
    assert_eq!(ctx_of(gpu, A), Some((1, true)));
    let out = syncobj::create(false);
    FAKE_RM.lock().fence_stalls = true;
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    assert_eq!(
        exec(gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Err(nv::EIO)
    );
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    FAKE_RM.lock().fence_stalls = false;
    assert!(nv::ctx_is_wedged(1));
    assert_eq!(channel_free(gpu, ch as i32, A), Ok(0));
    let newcomer: std::sync::Arc<std::sync::Mutex<Option<(u32, Option<(u32, bool)>)>>> =
        Default::default();
    let sink = newcomer.clone();
    *CTX_TEARDOWN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
        if sink.lock().unwrap().is_some() {
            return;
        }
        let ch = client_with_pushbuf(gpu, B);
        *sink.lock().unwrap() = Some((ch, ctx_of(gpu, B)));
    }));
    let ch_a = channel_alloc(gpu, A).unwrap().channel as u32;
    *CTX_TEARDOWN_HOOK.lock() = None;
    let (ch_b, ctx_b) = newcomer
        .lock()
        .unwrap()
        .take()
        .expect("the newcomer came during the retirement");
    assert_eq!(ctx_b, Some((2, true)), "not the index being retired");
    assert_eq!(ctx_of(gpu, A), Some((1, true)), "free once freed");
    assert!(!nv::ctx_is_wedged(1));
    assert!(FAKE_RM.lock().ctxs.contains(&2), "B's context is alive");
    assert_eq!(exec(gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let h = gem_new_rm(gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(gpu, A, &mut [map(h, PUSH_VA, 65536)]), Ok(0));
    assert_eq!(exec(gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

// ---- The direct-submit path -------------------------------------------
//
// With the fake's `fast` switch on, `exec_fast_prepare` hands the driver
// a channel: a 64 KiB buffer (fence slot page, landing zone, GPFIFO
// ring) and a USERD window, host memory the driver reaches through the
// identity `phys_to_virt`. From then on an EXEC never enters the RM: it
// writes GP entries and semaphore streams, bumps GPPut and pokes the
// doorbell in BAR0. `run_gpu` is the PBDMA: it walks GPGet up to GPPut,
// decodes each entry, executes the host semaphore methods (a RELEASE
// writes its payload, an ACQUIRE stalls the channel until the payload
// is there) and hands back what it fetched, in order.

use crate::nvme::nvme_queue::test_clock;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetched {
    Push {
        va: u64,
        len: u32,
    },
    Release {
        sem_va: u64,
        payload: u32,
    },
    Acquire {
        sem_va: u64,
        payload: u32,
    },
    /// A semaphore at a VA the channel has no mapping for: the MMU
    /// fault that kills the channel on hardware.
    Fault {
        sem_va: u64,
    },
}

fn gpu_rm_fast() -> NvidiaGpu {
    let gpu = gpu_rm();
    FAKE_RM.lock().fast = true;
    gpu
}

fn chan(ctx: u32) -> FastChan {
    *FAKE_RM
        .lock()
        .fast_ctxs
        .iter()
        .find(|c| c.ctx == ctx)
        .expect("no direct-submit channel behind this context")
}

fn has_chan(ctx: u32) -> bool {
    FAKE_RM.lock().fast_ctxs.iter().any(|c| c.ctx == ctx)
}

fn peek(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn poke(addr: usize, v: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

/// `(GPGet, GPPut)` of a channel's USERD.
fn userd(c: &FastChan) -> (u32, u32) {
    (peek(c.userd + 0x88), peek(c.userd + 0x8c))
}

fn doorbell(gpu: &NvidiaGpu) -> u32 {
    peek(gpu._bar0 + FAKE_DOORBELL as usize)
}

fn landing_zone(c: &FastChan) -> u32 {
    peek(c.buf + FAST_SEM_OFF as usize)
}

/// The channel's fence semaphore, as the GPU addresses it.
fn sem_va(c: &FastChan) -> u64 {
    c.gpu_va + u64::from(FAST_SEM_OFF)
}

fn cpu_prep(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<usize, i32> {
    let mut r = nv::DrmNouveauGemCpuPrep { handle, flags: 0 };
    call(
        gpu,
        wr::<nv::DrmNouveauGemCpuPrep>(nv::NR_GEM_CPU_PREP),
        &mut r,
        pid,
    )
}

/// The PBDMA of context `ctx`: fetch every entry from GPGet up to GPPut.
/// A clock that advances on every read, restored to a standing one when
/// dropped, unwinding included: test threads are pooled, so an
/// auto-advance a failed test left behind would run the next tests on
/// that thread against a clock that never stands still.
struct AutoAdvance;

impl AutoAdvance {
    fn of(us_per_read: u64) -> Self {
        test_clock::set_auto_advance(us_per_read);
        Self
    }
}

impl Drop for AutoAdvance {
    fn drop(&mut self) {
        test_clock::set_auto_advance(0);
    }
}

fn run_gpu(ctx: u32) -> Vec<Fetched> {
    run_gpu_frames(ctx, u32::MAX)
}

/// [`run_gpu`], stopping after `max_releases` semaphore releases: one
/// frame's worth of work at a time, for a GPU modelled as slower than
/// the CPU feeding it.
fn run_gpu_frames(ctx: u32, max_releases: u32) -> Vec<Fetched> {
    let c = chan(ctx);
    let mut out = Vec::new();
    let mut releases = 0;
    loop {
        let (get, put) = userd(&c);
        if get == put {
            break;
        }
        let gp = c.buf + FAST_GPFIFO_OFF as usize + get as usize * 8;
        let (e0, e1) = (peek(gp), peek(gp + 4));
        let va = (u64::from(e1 & 0xff) << 32) | u64::from(e0);
        let len = ((e1 >> 10) & 0x1f_ffff) * 4;
        let streams = c.gpu_va + u64::from(FAST_PB_OFF)..c.gpu_va + u64::from(FAST_SEM_OFF);
        let item = if streams.contains(&va) {
            assert_eq!(len, 24, "a host semaphore stream is six dwords");
            let words = c.buf + (va - c.gpu_va) as usize;
            let w: [u32; 6] = core::array::from_fn(|i| peek(words + i * 4));
            assert_eq!(
                w[0],
                nv::push_hdr(0, nv::NVC46F_SEM_ADDR_LO, 5),
                "SEM_ADDR_LO..SEM_EXECUTE, five methods, incrementing"
            );
            let sem_va = (u64::from(w[2] & 0xff) << 32) | u64::from(w[1]);
            assert_eq!(w[4], 0, "SEM_PAYLOAD_HI");
            let sem = if (c.gpu_va..c.gpu_va + 0x10000).contains(&sem_va) {
                c.buf + (sem_va - c.gpu_va) as usize
            } else {
                // Another channel's semaphore, through a peer mapping of
                // this channel's -- or a VA nothing maps any more.
                let producer = FAKE_RM
                    .lock()
                    .peer_maps
                    .iter()
                    .find(|m| m.0 == ctx && m.3 == sem_va)
                    .map(|m| m.1);
                match producer.and_then(|p| {
                    FAKE_RM
                        .lock()
                        .fast_ctxs
                        .iter()
                        .find(|c| c.ctx == p)
                        .copied()
                }) {
                    Some(pc) => pc.buf + FAST_SEM_OFF as usize,
                    None => {
                        out.push(Fetched::Fault { sem_va });
                        break;
                    }
                }
            };
            let payload = w[3];
            match w[5] {
                nv::NVC46F_SEM_EXECUTE_RELEASE => {
                    poke(sem, payload);
                    Fetched::Release { sem_va, payload }
                }
                nv::NVC46F_SEM_EXECUTE_ACQUIRE => {
                    if (peek(sem).wrapping_sub(payload) as i32) < 0 {
                        // The channel stalls here: GPGet stays.
                        break;
                    }
                    Fetched::Acquire { sem_va, payload }
                }
                other => panic!(
                    "SEM_EXECUTE {:#x} is neither a release nor an acquire",
                    other
                ),
            }
        } else {
            Fetched::Push { va, len }
        };
        let is_release = matches!(item, Fetched::Release { .. });
        out.push(item);
        poke(c.userd + 0x88, (get + 1) % FAST_ENTRIES);
        if is_release {
            releases += 1;
            if releases >= max_releases {
                break;
            }
        }
    }
    out
}

#[test]
fn a_direct_submit_writes_the_ring_and_rings_the_doorbell_without_the_rm() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let fast = nv::EXEC_FAST_SUBMITS.load(Ordering::Relaxed);
    let legacy = nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 32)],
            &[],
            &[]
        ),
        Ok(0)
    );
    assert_eq!(
        rm_calls_since(before),
        ["exec_fast_prepare"],
        "the channel is prepared once; the submit itself never enters the RM"
    );
    assert!(FAKE_RM.lock().submits.is_empty());
    let c = chan(1);
    assert_eq!(
        userd(&c),
        (0, 2),
        "GPGet is the GPU's; GPPut past the two entries"
    );
    assert_eq!(
        doorbell(&gpu),
        c.token,
        "the channel's work token in the usermode doorbell"
    );
    // The raw entries, as the PBDMA reads them: GET in bits 31:2 of the
    // low word, GET_HI and the length in dwords in the high one.
    let gp = c.buf + FAST_GPFIFO_OFF as usize;
    assert_eq!(peek(gp), PUSH_VA as u32);
    assert_eq!(peek(gp + 4), (((PUSH_VA >> 32) as u32) & 0xff) | (4 << 10));
    assert_eq!(peek(gp + 12), (((PUSH_VA >> 32) as u32) & 0xff) | (8 << 10));
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Push {
                va: PUSH_VA + 0x100,
                len: 32
            }
        ]
    );
    assert_eq!(userd(&c), (2, 2));
    // The next EXEC: no prepare, the next slot.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA + 0x200, 8)], &[], &[]),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(userd(&c), (2, 3));
    assert_eq!(
        run_gpu(1),
        [Fetched::Push {
            va: PUSH_VA + 0x200,
            len: 8
        }]
    );
    assert_eq!(nv::EXEC_FAST_SUBMITS.load(Ordering::Relaxed), fast + 2);
    assert_eq!(nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed), legacy);
    // A second client: a channel of its own, and A's ring untouched.
    let ch_b = client_with_pushbuf(&gpu, B);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(rm_calls_since(before), ["exec_fast_prepare"]);
    let b = chan(2);
    assert_ne!(b.buf, c.buf);
    assert_eq!(userd(&b), (0, 1));
    assert_eq!(userd(&c), (3, 3));
    assert_eq!(doorbell(&gpu), b.token);
    assert_eq!(
        run_gpu(2),
        [Fetched::Push {
            va: PUSH_VA,
            len: 16
        }]
    );
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_syncobj_signals_only_when_the_gpu_reaches_the_fence_behind_the_pushes() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let bin = syncobj::create(false);
    let tl = syncobj::create(false);
    let fenced = nv::EXEC_FAST_FENCED.load(Ordering::Relaxed);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 32)],
            &[],
            &[sync(bin), sync_tl(tl, 5)]
        ),
        Ok(0),
        "returns at once: the fence is the GPU's to write"
    );
    let c = chan(1);
    assert_eq!(
        userd(&c),
        (0, 3),
        "two pushes and the fence entry behind them"
    );
    assert_eq!(landing_zone(&c), 0);
    // Submitted, not signaled: NVK asks for both.
    assert_eq!(syncobj::query_submitted(bin), Some(1));
    assert_eq!(syncobj::query_submitted(tl), Some(5));
    assert_eq!(syncobj::query(bin), Some(0));
    assert_eq!(syncobj::query(tl), Some(0));
    test_clock::set_auto_advance(100);
    let deadline = test_clock::now() + 5_000;
    assert!(
        matches!(
            syncobj::wait(&[bin, tl], Some(&[1, 5]), true, deadline),
            syncobj::WaitOutcome::Timeout
        ),
        "5 ms of waiting: the GPU has not run"
    );
    test_clock::set_auto_advance(0);
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Push {
                va: PUSH_VA + 0x100,
                len: 32
            },
            Fetched::Release {
                sem_va: sem_va(&c),
                payload: 1
            }
        ]
    );
    assert_eq!(landing_zone(&c), 1);
    assert_eq!(syncobj::query(bin), Some(1));
    assert_eq!(syncobj::query(tl), Some(5));
    assert!(!syncobj::has_pending());
    // The payloads are the channel's own sequence: the next fence is 2,
    // written into the same landing zone from the next slot's stream.
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&c),
                payload: 2
            }
        ]
    );
    assert_eq!(syncobj::query(out), Some(1));
    // A signal on a handle nobody has: refused before the ring is
    // touched, so the channel's fence sequence does not move either.
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(0xdead_0000)]),
        Err(nv::ENOENT)
    );
    assert_eq!(userd(&c), (5, 5));
    assert_eq!(run_gpu(1), [] as [Fetched; 0]);
    assert_eq!(nv::EXEC_FAST_FENCED.load(Ordering::Relaxed), fenced + 2);
    for h in [bin, tl, out] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_wait_on_the_same_channel_is_a_gpu_acquire_and_one_on_another_channel_a_cpu_wait() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    let before = FAKE_RM.lock().calls.len();
    // 1 us per clock read: a CPU wait here (the wrong path, nothing
    // runs the GPU yet) ends in EIO after 10 s virtual, a failure
    // rather than a hang.
    test_clock::set_auto_advance(1);
    let t0 = test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA + 0x100, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0),
        "the wait is a fence pending on this very channel: no CPU wait"
    );
    test_clock::set_auto_advance(0);
    assert!(test_clock::now() - t0 < 1_000, "submitted without waiting");
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    let a = chan(1);
    assert_eq!(userd(&a), (0, 5), "push, fence, acquire, push, fence");
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&a),
                payload: 1
            },
            Fetched::Acquire {
                sem_va: sem_va(&a),
                payload: 1
            },
            Fetched::Push {
                va: PUSH_VA + 0x100,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&a),
                payload: 2
            }
        ],
        "the acquire sits in front of the push it guards"
    );
    assert_eq!(syncobj::query(out), Some(1));
    assert_eq!(syncobj::query(out2), Some(1));
    // B waits on A's fence. This RM cannot map A's semaphore into B's
    // VAS, so B's EXEC blocks on the CPU until A's GPU gets there, and
    // B's ring carries no acquire.
    let out3 = syncobj::create(false);
    let out4 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
        Ok(0)
    );
    let now = test_clock::now();
    let gpu_ref = &gpu;
    std::thread::scope(|s| {
        let t = s.spawn(move || {
            // 1 us per clock read: B's wait ends in EIO after 10 s
            // virtual should A's fence never land (a failure, not a
            // hang).
            test_clock::set(now);
            test_clock::set_auto_advance(1);
            exec(
                gpu_ref,
                B,
                ch_b,
                &[push(PUSH_VA, 16)],
                &[sync(out3)],
                &[sync(out4)],
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished(), "B is waiting for A's fence");
        assert!(
            !has_chan(2),
            "and has queued nothing yet: its channel is not even prepared"
        );
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&a),
                    payload: 3
                }
            ]
        );
        assert_eq!(t.join().unwrap(), Ok(0));
    });
    let b = chan(2);
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 1
            }
        ],
        "no acquire on B's ring"
    );
    assert_eq!(syncobj::query(out4), Some(1));
    for h in [out, out2, out3, out4] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_full_ring_waits_for_the_gpu_and_one_that_never_drains_is_eio_and_wedges_the_channel() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    // One slot is always kept free, so 127 entries fill the 128-entry ring.
    for i in 0..127 {
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + i * 16, 16)], &[], &[]),
            Ok(0),
            "entry {}",
            i
        );
    }
    let c = chan(1);
    assert_eq!(userd(&c), (0, 127));
    // The GPU drains the ring while the 128th submit waits for room:
    // the entry goes into the last slot and GPPut wraps to 0.
    let now = test_clock::now();
    // 1 us per clock read: the submit gives up (EIO) after 10 s virtual
    // should the GPU never make room, a failure rather than a hang.
    test_clock::set_auto_advance(1);
    let mut drained = std::thread::scope(|s| {
        let t = s.spawn(move || {
            test_clock::set(now);
            std::thread::sleep(Duration::from_millis(30));
            run_gpu(1)
        });
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + 0x800, 16)], &[], &[]),
            Ok(0),
            "room came while the submit waited"
        );
        t.join().unwrap()
    });
    test_clock::set_auto_advance(0);
    assert_eq!(userd(&c).1, 0, "wrapped");
    // The GPU may or may not have reached the 128th entry before it
    // stopped: either way it is the last thing fetched.
    drained.extend(run_gpu(1));
    assert_eq!(drained.len(), 128);
    assert_eq!(
        drained[127],
        Fetched::Push {
            va: PUSH_VA + 0x800,
            len: 16
        }
    );
    assert_eq!(userd(&c), (0, 0));
    assert!(!nv::ctx_is_wedged(1));
    // Fill it again, and this time nothing drains it: after 10 s
    // (1 ms per clock read) the submit is EIO and the channel is
    // latched WEDGED, so the client fast-fails instead of hanging the
    // compositor's ring behind it.
    for i in 0..127 {
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + i * 16, 16)], &[], &[]),
            Ok(0)
        );
    }
    assert_eq!(userd(&c), (0, 127));
    let out = syncobj::create(false);
    test_clock::set_auto_advance(1000);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Err(nv::EIO)
    );
    test_clock::set_auto_advance(0);
    assert!(nv::ctx_is_wedged(1));
    assert_eq!(userd(&c), (0, 127), "nothing was written over the ring");
    assert_eq!(syncobj::query(out), Some(0), "and nothing was attached");
    assert!(!syncobj::has_pending());
    // The GPU catching up later does not unlatch the context.
    assert_eq!(run_gpu(1).len(), 127);
    assert_eq!(userd(&c), (127, 127));
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
        Err(nv::EIO),
        "wedged until the process goes away"
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(userd(&c), (127, 127));
    // B is unaffected: its own channel.
    let ch_b = client_with_pushbuf(&gpu, B);
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    // A exits and comes back: a fresh channel, ring at 0.
    gpu.nouveau_release_process(A);
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(FAKE_RM.lock().fast_releases, [1]);
    let ch = client_with_pushbuf(&gpu, A);
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let c2 = chan(1);
    assert_ne!(c2.userd, c.userd);
    assert_eq!(userd(&c2), (0, 1));
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// NVK batches up to 256 waits and 256 signals per EXEC and 64 pushes
/// (`GETPARAM_EXEC_PUSH_MAX`), and every same-channel wait is a GPFIFO
/// ACQUIRE entry here, next to the pushes and the fence. The ring has
/// 128 entries with one kept free, so a batch of 64 pushes with 63 or
/// more such waits asked for room the ring never has: `fast_submit`
/// spun 10 s, latched the channel WEDGED and every EXEC after it was
/// EIO (VK_ERROR_DEVICE_LOST) until the process died. Linux has no such
/// ceiling: its waits are scheduler dependencies, not ring entries.
#[test]
fn an_exec_with_more_acquires_than_the_ring_holds_waits_for_the_rest_on_the_cpu_and_is_not_a_hang()
{
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    // One submit signals 100 semaphores with its one fence (65 or more
    // was EOPNOTSUPP before): 100 waits on them are 100 ACQUIREs.
    let sems: Vec<u32> = (0..100).map(|_| syncobj::create(false)).collect();
    let sigs: Vec<_> = sems.iter().map(|&s| sync(s)).collect();
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &sigs),
        Ok(0),
        "100 signals in one EXEC"
    );
    let c = chan(1);
    assert_eq!(userd(&c), (0, 2));
    assert!(sems.iter().all(|&s| syncobj::query(s) == Some(0)));
    // 100 waits + 64 pushes + the fence would be 165 entries.
    let waits: Vec<_> = sems.iter().map(|&s| sync(s)).collect();
    let pushes: Vec<_> = (0..64)
        .map(|i| push(PUSH_VA + 0x100 + i * 16, 16))
        .collect();
    let out = syncobj::create(false);
    let now = test_clock::now();
    let tick = AutoAdvance::of(1);
    let fetched = std::thread::scope(|s| {
        // The GPU runs on its own: it lands the first fence, which
        // satisfies the waits that did not fit as ACQUIREs, and drains
        // the ring for the submission itself.
        let t = s.spawn(move || {
            test_clock::set(now);
            let mut got = Vec::new();
            let start = std::time::Instant::now();
            while got.len() < 2 + 127 && start.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(5));
                got.extend(run_gpu(1));
            }
            got
        });
        assert_eq!(
            exec(&gpu, A, ch, &pushes, &waits, &[sync(out)]),
            Ok(0),
            "62 ACQUIREs in the ring, 38 waits on the CPU"
        );
        t.join().unwrap()
    });
    drop(tick);
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(fetched.len(), 2 + 127);
    let acquires = fetched
        .iter()
        .filter(|f| matches!(f, Fetched::Acquire { payload: 1, .. }))
        .count();
    assert_eq!(
        acquires,
        127 - 64 - 1,
        "as many ACQUIREs as the ring holds next to 64 pushes and the fence"
    );
    assert_eq!(
        fetched
            .iter()
            .filter(|f| matches!(f, Fetched::Push { .. }))
            .count(),
        65
    );
    assert_eq!(
        fetched
            .iter()
            .filter(|f| matches!(f, Fetched::Release { .. }))
            .count(),
        2
    );
    assert!(
        matches!(fetched[2], Fetched::Acquire { .. }),
        "ACQUIREs first"
    );
    assert!(
        matches!(fetched[128], Fetched::Release { .. }),
        "the fence last"
    );
    assert_eq!(syncobj::query(out), Some(1));
    assert!(sems.iter().all(|&s| syncobj::query(s) == Some(1)));
    assert_eq!(userd(&c), (1, 1), "129 entries: wrapped once");
    // Not wedged: the channel goes on.
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    // A submission larger than the ring on its own is refused at once,
    // not after 10 s, and does not wedge the channel either.
    let big: Vec<_> = (0..128).map(|i| push(PUSH_VA + i * 16, 16)).collect();
    // (1 ms per clock read: a submit that still spun would give up
    // after its 10 s instead of hanging the test.)
    let tick = AutoAdvance::of(1000);
    assert!(matches!(
        gpu.fast_submit(1, &big, false, &[]),
        Err(nv::FastSubmitError::TooLarge {
            needed: 128,
            entries: 128
        })
    ));
    assert!(!nv::ctx_is_wedged(1));
    // Through EXEC's direct path the answer is EINVAL, and still no
    // WEDGED latch: the next submission is welcome.
    let r = nv::DrmNouveauExec {
        channel: ch,
        push_count: big.len() as u32,
        wait_count: 0,
        sig_count: 0,
        wait_ptr: 0,
        sig_ptr: 0,
        push_ptr: big.as_ptr() as u64,
    };
    assert_eq!(gpu.exec_fast(1, A, &r, &big, &[]), Err(nv::EINVAL));
    drop(tick);
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(userd(&c), (1, 2), "nothing was written over the ring");
    assert_eq!(run_gpu(1).len(), 1);
    for s in sems {
        assert!(syncobj::destroy(s));
    }
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn exit_releases_the_window_before_freeing_the_context_and_lets_go_of_the_fences_in_flight() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    let tl = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16)],
            &[],
            &[sync(out), sync_tl(tl, 4)]
        ),
        Ok(0)
    );
    assert_eq!(syncobj::query(out), Some(0));
    assert!(syncobj::has_pending());
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(A);
    let calls = rm_calls_since(before);
    let released = calls
        .iter()
        .position(|c| *c == "exec_fast_release")
        .expect("the USERD window is unmapped at exit");
    let freed = calls
        .iter()
        .position(|c| *c == "ctx_free")
        .expect("the channel is freed at exit");
    assert!(
        released < freed,
        "the window goes before the channel it maps: {:?}",
        calls
    );
    assert_eq!(FAKE_RM.lock().fast_releases, [1]);
    assert!(FAKE_RM.lock().fast_ctxs.is_empty());
    // A fence that can never land now is signaled, as a killed channel's
    // would be: a compositor waiting on the dead client's buffer moves on.
    assert!(!syncobj::has_pending());
    assert_eq!(syncobj::query(out), Some(1));
    assert_eq!(syncobj::query(tl), Some(4));
    assert!(!nv::ctx_is_wedged(1), "abandoned, not timed out");
    // Back: a fresh window and ring.
    let ch = client_with_pushbuf(&gpu, A);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(rm_calls_since(before), ["exec_fast_prepare"]);
    assert_eq!(userd(&chan(1)), (0, 1));
    for h in [out, tl] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_setup_that_fails_pins_the_rm_path_and_still_releases_its_window_at_exit() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    // The SDK's DRF value for SEM_EXECUTE disagrees with the kernel's
    // encoder: the RM path, for this context, for good.
    FAKE_RM.lock().fast_bad_encoding = true;
    let ch = client_with_pushbuf(&gpu, A);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(
        rm_calls_since(before),
        ["exec_fast_prepare", "exec_submit"],
        "the self-check failed: this push goes through the RM"
    );
    let before = FAKE_RM.lock().calls.len();
    FAKE_RM.lock().fast_bad_encoding = false;
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(
        rm_calls_since(before),
        ["exec_submit"],
        "the verdict is pinned: no second prepare, even one that would pass"
    );
    assert!(
        has_chan(1),
        "the RM mapped the USERD before the check that failed"
    );
    // A exits. The window it never used still has to go: `ctx_free`
    // does not unmap it, and the RM refuses to prepare the next channel
    // behind an index whose window maps a freed one -- which would take
    // the direct path away from every later owner of this index.
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(A);
    assert!(
        rm_calls_since(before).contains(&"exec_fast_release"),
        "{:?}",
        rm_calls_since(before)
    );
    assert!(!has_chan(1));
    let ch = client_with_pushbuf(&gpu, B);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, B, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(
        rm_calls_since(before),
        ["exec_fast_prepare"],
        "the next owner of the index gets the direct path"
    );
    assert_eq!(userd(&chan(1)), (0, 1));
    gpu.nouveau_release_process(B);
    // A prepare the RM refuses outright pins the RM path the same way.
    FAKE_RM.lock().fast = false;
    let ch = client_with_pushbuf(&gpu, A);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(rm_calls_since(before), ["exec_fast_prepare", "exec_submit"]);
    FAKE_RM.lock().fast = true;
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(rm_calls_since(before), ["exec_submit"]);
    gpu.nouveau_release_process(A);
    // `nvidia.exec_rm`: the direct path switched off never prepares at
    // all, and switching it back on prepares on the next EXEC.
    nv::set_exec_fast_enabled(false);
    let ch = client_with_pushbuf(&gpu, STRANGER);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, STRANGER, ch, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), ["exec_submit"]);
    nv::set_exec_fast_enabled(true);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, STRANGER, ch, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), ["exec_fast_prepare"]);
    gpu.nouveau_release_process(STRANGER);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn cpu_prep_waits_for_everything_queued_on_the_channel_behind_a_probe_fence_of_its_own() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    // Nothing queued: nothing to wait for, and no channel is prepared
    // just to find that out.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
    assert_eq!(cpu_prep(&gpu, h, A), Ok(0));
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert!(!has_chan(1));
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let c = chan(1);
    assert_eq!(userd(&c), (0, 1));
    // NOWAIT with the push still queued: EBUSY at once, and a
    // fence-only entry behind the push is what will prove it finished.
    // The clock moves 1 us per read from here on, so a wait that
    // blocks shows up as virtual time, and a wait for a GPU that never
    // comes ends (EBUSY after 10 s virtual) instead of hanging.
    test_clock::set_auto_advance(1);
    let t0 = test_clock::now();
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::EBUSY));
    assert!(test_clock::now() - t0 < 1_000, "answered without waiting");
    assert_eq!(userd(&c), (0, 2), "a probe fence behind the push");
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::EBUSY));
    assert_eq!(userd(&c), (0, 3));
    // A blocking prep returns once the GPU has run past its probe.
    // The GPU only starts fetching once that probe is on the ring
    // (GPPut at 4): were it to run the two NOWAIT probes first, the
    // prep would find the channel idle and append nothing, and this
    // test would then wait its full five seconds for a third release
    // that never comes.
    let now = test_clock::now();
    let sem = sem_va(&c);
    std::thread::scope(|s| {
        let t = s.spawn(move || {
            test_clock::set(now);
            for _ in 0..5_000 {
                if userd(&c).1 >= 4 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(userd(&c), (0, 4), "the blocking prep queued its probe");
            let mut fetched = Vec::new();
            for _ in 0..5_000 {
                fetched.extend(run_gpu(1));
                if fetched.contains(&Fetched::Release {
                    sem_va: sem,
                    payload: 3,
                }) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            fetched
        });
        assert_eq!(cpu_prep(&gpu, h, A), Ok(0));
        assert_eq!(
            t.join().unwrap(),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 1
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 2
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 3
                }
            ]
        );
    });
    assert_eq!(userd(&c), (4, 4));
    assert_eq!(landing_zone(&c), 3);
    // Idle: the last thing on the ring is a fence that landed. Neither
    // prep appends a probe, and NOWAIT is not busy.
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
    assert_eq!(cpu_prep(&gpu, h, A), Ok(0));
    assert_eq!(userd(&c), (4, 4), "no probe needed: the last one landed");
    // A push behind that fence makes the channel busy again. Nothing
    // runs the GPU: after 10 s (1 ms per clock read) the wait is EBUSY,
    // as Linux answers a reservation wait that timed out.
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::EBUSY));
    assert_eq!(userd(&c), (4, 6), "the push and its probe");
    // A GPU that arrives two real seconds late, an eternity next to
    // the 10 s virtual: only there so a wait that ignored its bound
    // would fail here instead of hanging the test.
    test_clock::set_auto_advance(1000);
    let now = test_clock::now();
    let (late, _) = std::thread::scope(|s| {
        let t = s.spawn(move || {
            test_clock::set(now);
            std::thread::sleep(Duration::from_secs(2));
            run_gpu(1)
        });
        (cpu_prep(&gpu, h, A), t.join().unwrap())
    });
    assert_eq!(late, Err(nv::EBUSY));
    test_clock::set_auto_advance(0);
    assert!(!nv::ctx_is_wedged(1), "a slow GPU is not a hung one");
    // A process without a channel of its own has queued nothing.
    let hs = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, STRANGER)
        .unwrap()
        .handle;
    assert_eq!(cpu_prep(&gpu, hs, STRANGER), Ok(0));
    assert_eq!(
        cpu_prep(&gpu, h, STRANGER),
        Err(nv::ENOENT),
        "not its buffer"
    );
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(STRANGER);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The `NOWAIT` bit is the one `nouveau_drm.h` defines and libdrm sends,
/// bit 0; `WRITE` (bit 2) is not it, and neither is bit 1, which no
/// header defines. This used to read bit 1: a client's NOWAIT waited
/// for the whole of the queued work, and the tests above proved NOWAIT
/// with a bit no client sends.
#[test]
fn cpu_prep_nowait_is_bit_zero_as_libdrm_sends_it_and_write_is_not_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let c = chan(1);
    assert_eq!(userd(&c), (0, 1));
    // Nothing runs the GPU. 1 ms per clock read: a wait that blocks is
    // the 10 s timeout, in virtual time, and answers EBUSY too -- what
    // tells a blocking prep from a NOWAIT one is the clock.
    test_clock::set_auto_advance(1000);
    for (flags, what) in [
        (nv::NOUVEAU_GEM_CPU_PREP_NOWAIT, "NOWAIT"),
        (
            nv::NOUVEAU_GEM_CPU_PREP_NOWAIT | nv::NOUVEAU_GEM_CPU_PREP_WRITE,
            "NOWAIT | WRITE",
        ),
        (nv::NOUVEAU_GEM_CPU_PREP_NOWAIT | 0x2, "NOWAIT plus bit 1"),
    ] {
        let t0 = test_clock::now();
        assert_eq!(cpu_prep_flags(&gpu, h, flags, A), Err(nv::EBUSY), "{what}");
        assert!(
            test_clock::now() - t0 < 100_000,
            "{what}: waited {} us instead of answering at once",
            test_clock::now() - t0
        );
    }
    for (flags, what) in [
        (0, "no flags"),
        (nv::NOUVEAU_GEM_CPU_PREP_WRITE, "WRITE"),
        (0x2, "bit 1, which no header defines"),
    ] {
        let t0 = test_clock::now();
        assert_eq!(cpu_prep_flags(&gpu, h, flags, A), Err(nv::EBUSY), "{what}");
        assert!(
            test_clock::now() - t0 >= 10_000_000,
            "{what}: answered after {} us instead of waiting the 10 s",
            test_clock::now() - t0
        );
    }
    test_clock::set_auto_advance(0);
    assert!(!nv::ctx_is_wedged(1), "a slow GPU is not a hung one");
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn cpu_prep_waits_for_every_channel_whose_vm_maps_the_buffer_not_only_the_callers() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    let ch_c = client_with_pushbuf(&gpu, COMP);
    assert_eq!(ctx0_owner(&gpu), COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    // The client's frame: bound in its own VAS, then imported over PRIME
    // and bound by the compositor, which samples it from its own ring.
    const FRAME_VA: u64 = PUSH_VA + 0x10_0000;
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, FRAME_VA, 65536)]), Ok(0));
    assert!(crate::scheme::gem_mmap::add_ref(h, COMP).is_some());
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h, FRAME_VA, 65536)]),
        Ok(0)
    );
    // Another buffer of the client's, shared the same way but bound in
    // no VM: nothing on any ring can touch it.
    let h_idle = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert!(crate::scheme::gem_mmap::add_ref(h_idle, COMP).is_some());
    assert_eq!(
        cpu_prep_nowait(&gpu, h, COMP),
        Ok(0),
        "nothing queued anywhere"
    );
    // The client renders the frame: queued on ring 1, not fetched yet.
    // The clock moves 1 us per read from here on, so a wait that
    // blocks shows up as virtual time and one for a GPU that never
    // comes ends (EBUSY after 10 s virtual) instead of hanging.
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let c1 = chan(1);
    assert_eq!(userd(&c1), (0, 1));
    assert!(!has_chan(0), "the compositor has submitted nothing yet");
    test_clock::set_auto_advance(1);
    let t0 = test_clock::now();
    // The compositor about to read the frame with the CPU: busy, because
    // the PRODUCER's channel still has it queued. Before this, only the
    // caller's channel was asked, and the compositor's was idle.
    assert_eq!(
        cpu_prep_nowait(&gpu, h, COMP),
        Err(nv::EBUSY),
        "the producer's channel still has the frame queued"
    );
    assert!(test_clock::now() - t0 < 1_000, "answered without waiting");
    assert_eq!(
        userd(&c1),
        (0, 2),
        "the probe went behind the producer's push"
    );
    assert!(
        !has_chan(0),
        "no channel is prepared for the caller just to find that out"
    );
    assert_eq!(
        cpu_prep_nowait(&gpu, h_idle, COMP),
        Ok(0),
        "a shared buffer no VM maps has no fence to wait for"
    );
    assert_eq!(userd(&c1), (0, 2), "and probes nothing");
    // A blocking prep returns once the producer's ring ran past the
    // probe it appended (GPPut at 3).
    let now = test_clock::now();
    let sem = sem_va(&c1);
    std::thread::scope(|s| {
        let t = s.spawn(move || {
            test_clock::set(now);
            for _ in 0..5_000 {
                if userd(&c1).1 >= 3 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(userd(&c1), (0, 3), "the blocking prep queued its probe");
            let mut fetched = Vec::new();
            for _ in 0..5_000 {
                fetched.extend(run_gpu(1));
                if fetched.contains(&Fetched::Release {
                    sem_va: sem,
                    payload: 2,
                }) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            fetched
        });
        assert_eq!(cpu_prep(&gpu, h, COMP), Ok(0));
        assert_eq!(
            t.join().unwrap(),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 1
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 2
                }
            ]
        );
    });
    assert_eq!(userd(&c1), (3, 3));
    assert!(!has_chan(0));
    // The other way round: the compositor sampling the frame is a
    // reader the client must wait for before it writes the buffer
    // again. Its own ring is idle and gets no probe.
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    let c0 = chan(0);
    assert_eq!(userd(&c0), (0, 1));
    assert_eq!(
        cpu_prep_nowait(&gpu, h, A),
        Err(nv::EBUSY),
        "the compositor's channel still has the sample queued"
    );
    assert_eq!(
        userd(&c0),
        (0, 2),
        "the probe went behind the compositor's push"
    );
    assert_eq!(userd(&c1), (3, 3), "the caller's idle ring gets no probe");
    // The compositor asking about a buffer it maps itself: caller and
    // mapper at once, and one probe on its channel.
    assert_eq!(cpu_prep_nowait(&gpu, h, COMP), Err(nv::EBUSY));
    assert_eq!(userd(&c0), (0, 3), "one probe, not one per role");
    assert_eq!(run_gpu(0).len(), 3, "push and both probes");
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
    assert_eq!(cpu_prep_nowait(&gpu, h, COMP), Ok(0));
    test_clock::set_auto_advance(0);
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The async pre-wait's fences and the blocking arm agree, frame by
/// frame.
///
/// `GEM_CPU_PREP` blocks by contract, and `io_control` is synchronous, so
/// the arm that serves it can only busy-wait -- a core pegged for however
/// long the GPU takes, on the ioctl Mesa calls every time it recycles a
/// buffer. `cpu_prep_fences` exists so `sys_ioctl` can sleep on the same
/// fences first and leave that arm nothing to spin for.
///
/// "The same fences" is the whole contract, and it is what this pins:
/// while the list is unlanded the blocking arm answers EBUSY, and the
/// moment the last of it lands the blocking arm answers Ok. A list
/// missing the producer's ring -- the caller's pid alone, which is what
/// the blocking arm itself used to look at -- wakes the sleeper early and
/// hands the spin straight back.
#[test]
fn the_pre_waits_fences_land_exactly_when_the_blocking_cpu_prep_returns() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    let _ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    const FRAME_VA: u64 = PUSH_VA + 0x10_0000;
    // The client's frame, bound in its own VAS and imported and bound by
    // the compositor: the producer is A, the caller below is COMP.
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, FRAME_VA, 65536)]), Ok(0));
    assert!(crate::scheme::gem_mmap::add_ref(h, COMP).is_some());
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h, FRAME_VA, 65536)]),
        Ok(0)
    );
    // Shared the same way but bound in no VM: nothing can be writing it.
    let h_idle = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert!(crate::scheme::gem_mmap::add_ref(h_idle, COMP).is_some());

    // The same call the pre-wait in `drm_scheme.rs` makes.
    let landed = |fences: &[(usize, u32)]| crate::scheme::syncobj::hw_fences_landed(fences);

    // Nothing queued: no fence to sleep on, and the blocking arm does
    // not block either.
    assert!(
        DrmScheme::cpu_prep_fences(&gpu, h, COMP).is_empty(),
        "a ring with nothing on it gave the sleeper a fence"
    );
    assert_eq!(cpu_prep_nowait(&gpu, h, COMP), Ok(0));

    // The client renders the frame: queued on its ring, not fetched.
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let c1 = chan(1);
    let fences = DrmScheme::cpu_prep_fences(&gpu, h, COMP);
    assert!(
        !fences.is_empty(),
        "the producer's queued frame left the sleeper nothing to wait on, \
         so it would return at once and the sync arm would spin the wait"
    );
    assert!(
        !landed(&fences),
        "the sleeper's fences read as landed while the frame is still queued"
    );
    // A buffer no VM maps still has nothing to wait for, whoever asks.
    assert!(DrmScheme::cpu_prep_fences(&gpu, h_idle, COMP).is_empty());

    // The GPU runs the frame and the probe behind it. Now every fence
    // the sleeper was given has landed -- and that is exactly the moment
    // the blocking arm stops saying EBUSY.
    test_clock::set_auto_advance(1);
    assert_eq!(
        cpu_prep_nowait(&gpu, h, COMP),
        Err(nv::EBUSY),
        "the producer's channel still has the frame queued"
    );
    assert!(!landed(&fences), "landed before the GPU ran");
    let fetched = run_gpu(1);
    assert!(
        fetched.iter().any(|f| matches!(f, Fetched::Release { .. })),
        "the probe left no fence on the producer's ring ({:?})",
        fetched
    );
    assert!(
        landed(&fences),
        "the sleeper is still parked on a ring that has drained"
    );
    assert_eq!(
        cpu_prep_nowait(&gpu, h, COMP),
        Ok(0),
        "the blocking arm still waits after the sleeper's fences landed"
    );
    assert_eq!(userd(&c1).0, userd(&c1).1, "the ring drained");

    test_clock::set_auto_advance(0);
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A client that exits mid-frame with the compositor's ACQUIRE queued
/// on its semaphore stays as a zombie context (`ZombieCtx`), and its
/// ring keeps running what it had queued: the frame it was writing is
/// still being written after the exit. Linux keeps the dead process's
/// fences on the BO's reservation until its channel has idled
/// (`nouveau_channel_idle` at close), so the compositor's `CPU_PREP` on
/// that frame waits for them. Here the exit took the pid off the
/// context registry, so the prep found no channel behind the mapping's
/// pid and answered at once: the compositor read the frame under the
/// dead client's ring. A zombie's ring is looked up through the zombie
/// list, by the pid its mappings wear (the tombstone once the pid was
/// recycled).
#[test]
fn cpu_prep_waits_for_a_zombies_ring_that_still_writes_the_buffer() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    const FRAME_VA: u64 = PUSH_VA + 0x10_0000;
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, FRAME_VA, 65536)]), Ok(0));
    assert!(crate::scheme::gem_mmap::add_ref(h, COMP).is_some());
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h, FRAME_VA, 65536)]),
        Ok(0)
    );
    // The client renders the frame and signals; the compositor samples
    // it behind an ACQUIRE on the client's semaphore, with a fence of
    // its own, so its ring reads idle once that has landed.
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(
            &gpu,
            COMP,
            ch_c,
            &[push(PUSH_VA, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0)
    );
    let (c0, c1) = (chan(0), chan(1));
    assert_eq!(userd(&c1), (0, 2), "the frame and its fence, not fetched");
    // The client exits before its ring ran: a zombie, its ring still
    // to run. Its last payload landed at the park, so the compositor
    // passes its ACQUIRE and drains its own ring.
    gpu.nouveau_release_process(A);
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1, "a zombie");
    assert!(has_chan(1));
    assert_eq!(run_gpu(0).len(), 3, "the compositor passes its acquire");
    assert_eq!(userd(&c0), (3, 3));
    assert_eq!(userd(&c1), (0, 2), "the zombie's ring has not moved");
    test_clock::set_auto_advance(1);
    let t0 = test_clock::now();
    // The compositor about to read the frame with the CPU: busy, the
    // dead client's ring still has the write queued. Before this, a
    // mapping whose pid had left the registry was taken as idle.
    assert_eq!(
        cpu_prep_nowait(&gpu, h, COMP),
        Err(nv::EBUSY),
        "the zombie's ring still has the frame queued"
    );
    assert!(test_clock::now() - t0 < 1_000, "answered without waiting");
    assert_eq!(
        userd(&c1),
        (0, 3),
        "the probe went behind the zombie's push"
    );
    assert_eq!(userd(&c0), (3, 3), "nothing on the compositor's idle ring");
    // The pid comes back as a new process: the zombie and its mappings
    // now wear its tombstone, and the prep still finds its ring.
    gpu.rekey_zombie_wearing(A);
    assert_eq!(
        cpu_prep_nowait(&gpu, h, COMP),
        Err(nv::EBUSY),
        "the tombstone leads to the zombie's ring as the pid did"
    );
    assert_eq!(userd(&c1), (0, 4));
    // A blocking prep returns once the zombie's ring ran past the probe
    // it appended (GPPut at 5).
    let now = test_clock::now();
    let sem = sem_va(&c1);
    std::thread::scope(|s| {
        let t = s.spawn(move || {
            test_clock::set(now);
            for _ in 0..5_000 {
                if userd(&c1).1 >= 5 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(userd(&c1), (0, 5), "the blocking prep queued its probe");
            let mut fetched = Vec::new();
            for _ in 0..5_000 {
                fetched.extend(run_gpu(1));
                if fetched.contains(&Fetched::Release {
                    sem_va: sem,
                    payload: 4,
                }) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            fetched
        });
        assert_eq!(cpu_prep(&gpu, h, COMP), Ok(0));
        assert_eq!(
            t.join().unwrap(),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 1
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 2
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 3
                },
                Fetched::Release {
                    sem_va: sem,
                    payload: 4
                }
            ]
        );
    });
    assert_eq!(userd(&c1), (5, 5));
    // That probe is the ring's own word: with it landed the zombie reads
    // idle, and nothing more is appended.
    assert_eq!(cpu_prep_nowait(&gpu, h, COMP), Ok(0));
    assert_eq!(userd(&c1), (5, 5), "no probe behind a landed probe");
    // Its consumer passed: the zombie is due, and once its teardown ran
    // the mapping is gone with it. Nothing left to wait for, and no
    // channel to probe.
    gpu.reap_zombie_contexts();
    assert!(gpu.nouveau_zombies.lock().is_empty());
    assert!(!has_chan(1), "the zombie's channel is freed");
    assert_eq!(cpu_prep_nowait(&gpu, h, COMP), Ok(0));
    assert_eq!(userd(&c0), (3, 3), "no probe on the compositor's idle ring");
    test_clock::set_auto_advance(0);
    assert!(syncobj::destroy(out));
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn exec_with_an_unknown_sig_syncobj_queues_nothing_and_signals_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    // The direct-submit path. The sig list used to be looked at only
    // once the pushes were on the ring: the caller got ENOENT for a
    // batch the GPU was already fetching, and resubmitted it.
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    let c = chan(1);
    assert_eq!(run_gpu(1).len(), 2, "the push and its fence");
    assert_eq!(userd(&c), (2, 2));
    assert_eq!(syncobj::query(out), Some(1));
    const STRANGER_HANDLE: u32 = 0xdead_0004;
    assert_eq!(syncobj::query(STRANGER_HANDLE), None);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA + 0x100, 16)],
            &[],
            &[sync(out2), sync(STRANGER_HANDLE)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(userd(&c), (2, 2), "nothing went onto the ring");
    assert_eq!(run_gpu(1), [] as [Fetched; 0]);
    assert_eq!(
        syncobj::query_submitted(out2),
        Some(0),
        "the handle before the unknown one was not attached to any fence"
    );
    assert!(syncobj::pending_hw_fences(out2, 1).is_empty());
    // The unknown handle first, as a timeline point: the same.
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA + 0x100, 16)],
            &[],
            &[sync_tl(STRANGER_HANDLE, 3), sync(out2)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(userd(&c), (2, 2));
    assert!(syncobj::pending_hw_fences(out2, 1).is_empty());
    // Refused before the waits are honoured, like Linux (both lists are
    // looked up before the job is armed): a wait that never comes does
    // not turn the answer into a 10 s EIO.
    let never = syncobj::create(false);
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    let t0 = crate::nvme::nvme_queue::test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA + 0x100, 16)],
            &[sync(never)],
            &[sync(STRANGER_HANDLE)]
        ),
        Err(nv::ENOENT)
    );
    let waited = crate::nvme::nvme_queue::test_clock::now() - t0;
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    assert!(
        waited < 1_000_000,
        "answered at once, not after the deadline: {}us",
        waited
    );
    assert_eq!(userd(&c), (2, 2));
    // The health probe (no pushes) signals its sig list on the CPU, and
    // used to have signaled every handle before the unknown one.
    let out3 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[], &[], &[sync(out3), sync(STRANGER_HANDLE)]),
        Err(nv::ENOENT)
    );
    assert_eq!(syncobj::query(out3), Some(0), "not signaled");
    // The ring is intact: the resubmit with a good list is one push and
    // one fence, and the good handle is attached to that fence only.
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA + 0x100, 16)],
            &[],
            &[sync(out2)]
        ),
        Ok(0)
    );
    assert_eq!(userd(&c), (2, 4));
    assert_eq!(syncobj::pending_hw_fences(out2, 1).len(), 1);
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(syncobj::query(out2), Some(1));
    for h in [out, out2, out3, never] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn exec_through_the_rm_with_an_unknown_sig_syncobj_submits_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    // The RM per-submit path: the pushes went in, the fence came back,
    // and the handles before the unknown one were signaled -- then
    // ENOENT, for a batch that had run.
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    const STRANGER_HANDLE: u32 = 0xdead_0005;
    let n = FAKE_RM.lock().submits.len();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 16)],
            &[],
            &[sync(out), sync(STRANGER_HANDLE)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(FAKE_RM.lock().submits.len(), n, "no push entered the RM");
    assert_eq!(
        syncobj::query(out),
        Some(0),
        "the good handle stayed unsignaled"
    );
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(FAKE_RM.lock().submits.len(), n + 1);
    assert_eq!(syncobj::query(out), Some(1));
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A fence that never lands is given up on after `FENCE_TIMEOUT_US`:
/// syncobj advances the point so its CPU waiters do not park forever.
/// EXEC's contract on a wait that never arrives is EIO (NVK: device
/// lost), and that deadline is the same 10 s -- so whether the client's
/// push was refused or SUBMITTED behind a release that never happened
/// depended on which of the two clocks ticked first, i.e. on one extra
/// read of the clock, such as a QUERY between the submit and the EXEC.
#[test]
fn an_exec_wait_reached_only_by_a_timed_out_fence_is_eio_whichever_clock_ticks_first() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    // The plain case first: A's fence timed out long ago. Userspace's
    // QUERY reads it as reached (Linux: a signaled fence, error and all).
    let rel = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(rel)]),
        Ok(0)
    );
    test_clock::advance(syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(syncobj::query(rel), Some(1));
    let out = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(rel)],
            &[sync(out)]
        ),
        Err(nv::EIO),
        "the release never happened: not submitting"
    );
    assert!(
        !has_chan(2),
        "nothing on B's ring: it was never even prepared"
    );
    assert_eq!(syncobj::query(out), Some(0));
    // The same through a merge whose other half is genuine.
    let ok = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(ok)]),
        Ok(0)
    );
    assert_eq!(run_gpu(2).len(), 2);
    let b = chan(2);
    assert_eq!(userd(&b), (2, 2));
    assert_eq!(syncobj::query(ok), Some(1));
    let merged = syncobj::merge_fences(&[(rel, 1), (ok, 1)]);
    assert_eq!(syncobj::query(merged), Some(1));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(merged)],
            &[sync(out)]
        ),
        Err(nv::EIO)
    );
    assert_eq!(userd(&b), (2, 2));
    // And the health probe: it signals nothing on a wait that died.
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[sync(rel)], &[sync(out)]),
        Err(nv::EIO)
    );
    assert_eq!(syncobj::query(out), Some(0));
    // The probe with a half the GPU could ACQUIRE (B's own fence in
    // flight): the dead half still decides, before anything is queued.
    let ok2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(ok2)]),
        Ok(0)
    );
    assert_eq!(userd(&b), (2, 4));
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[sync(ok2), sync(rel)], &[sync(out)]),
        Err(nv::EIO)
    );
    assert_eq!(
        userd(&b),
        (2, 4),
        "no acquire and no fence went onto the ring"
    );
    assert_eq!(syncobj::query(out), Some(0));
    assert_eq!(run_gpu(2).len(), 2);
    assert_eq!(syncobj::query(ok2), Some(1));
    // A point reached for real before the one that timed out stays good:
    // 1 landed, 2 never did.
    let tl = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync_tl(tl, 1)]),
        Ok(0)
    );
    assert_eq!(run_gpu(1).len(), 4, "A's two submits land");
    assert_eq!(syncobj::query(tl), Some(1));
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync_tl(tl, 2)]),
        Ok(0)
    );
    test_clock::advance(syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(syncobj::query(tl), Some(2));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 1)],
            &[sync(out)]
        ),
        Ok(0),
        "point 1 was the GPU's word"
    );
    assert_eq!(userd(&b), (4, 6));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 2)],
            &[sync(out)]
        ),
        Err(nv::EIO)
    );
    assert_eq!(userd(&b), (4, 6));
    // Userspace signaling the point itself supersedes the dead fence.
    assert!(syncobj::timeline_signal(tl, 2));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 2)],
            &[sync(out)]
        ),
        Ok(0)
    );
    assert_eq!(userd(&b), (4, 8));
    assert_eq!(run_gpu(2).len(), 4);
    assert_eq!(syncobj::query(out), Some(1));
    // The race itself: B waits while the fence is still in flight, one
    // read of the clock per millisecond. The QUERY in between is the
    // read that used to make the fence timeout win over EXEC's own
    // deadline, and turned the EIO into a submit.
    let rel2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(rel2)]),
        Ok(0)
    );
    let out2 = syncobj::create(false);
    test_clock::set_auto_advance(1_000);
    assert_eq!(syncobj::query(rel2), Some(0));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(rel2)],
            &[sync(out2)]
        ),
        Err(nv::EIO)
    );
    test_clock::set_auto_advance(0);
    assert_eq!(userd(&b), (8, 8), "still nothing new on B's ring");
    assert_eq!(syncobj::query(out2), Some(0));
    assert_eq!(syncobj::query(rel2), Some(1), "given up on, as before");
    for h in [rel, out, ok, ok2, merged, tl, rel2, out2] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// `GEM_CLOSE` by the last holder freed the RM memory and unmapped the
/// VAS at once, with a ring that still had the buffer queued: the
/// compositor's blit of a client's frame, or a pushbuffer the client
/// was done with, read (or scanned out) freed memory. Linux frees a BO
/// only once the fences on its reservation have signaled
/// (`ttm_bo_release` waits, or defers the delete). Here the close
/// appends a probe fence behind every ring whose VM maps the object
/// (and the caller's own), and the free waits for them, reaped at the
/// next submit, close or exit; the handle itself goes at once.
#[test]
fn gem_close_of_a_buffer_a_ring_still_reads_frees_it_once_the_ring_has_passed() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    const FRAME_VA: u64 = PUSH_VA + 0x10_0000;
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, FRAME_VA, 65536)]), Ok(0));
    assert!(crate::scheme::gem_mmap::add_ref(h, COMP).is_some());
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h, FRAME_VA, 65536)]),
        Ok(0)
    );
    // The compositor samples the frame from its own ring (unfenced, not
    // fetched yet) and lets go of its import; the client, whose ring
    // has nothing queued, is done with it and closes last.
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    let c0 = chan(0);
    assert_eq!(userd(&c0), (0, 1));
    let (frees, unmaps) = {
        let f = FAKE_RM.lock();
        (f.gem_frees, f.unmaps)
    };
    assert!(gpu.nouveau_gem_close(h, COMP), "one holder letting go");
    assert!(has_object(&gpu, h));
    assert_eq!(mappings_of(&gpu, h), 2);
    assert_eq!(userd(&c0), (0, 1), "nothing probed for that");
    // The last holder: the handle goes now, the memory and the
    // mappings once the COMPOSITOR's ring has passed the blit.
    assert!(gpu.nouveau_gem_close(h, A));
    assert!(!has_object(&gpu, h), "the handle is gone at once");
    assert_eq!(
        mappings_of(&gpu, h),
        0,
        "and carries no mapping a new object with the same handle could inherit"
    );
    assert_eq!(
        FAKE_RM.lock().gem_frees,
        frees,
        "the memory stays until the ring has passed"
    );
    assert_eq!(
        FAKE_RM.lock().unmaps,
        unmaps,
        "and so does its place in the VAS"
    );
    assert_eq!(userd(&c0), (0, 2), "a probe fence behind the blit");
    assert!(!has_chan(1), "the client's idle ring was not even prepared");
    // A submit is a reap point, but the ring has not moved.
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(FAKE_RM.lock().gem_frees, frees);
    // The ring passes the blit and the probe: the next reap frees.
    assert_eq!(
        run_gpu(0),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&c0),
                payload: 1
            }
        ]
    );
    assert_eq!(FAKE_RM.lock().gem_frees, frees, "nobody has looked yet");
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.gem_frees, frees + 1, "freed");
        assert_eq!(f.unmaps, unmaps + 2, "both VMs unmapped");
    }
    // Nothing queued anywhere: freed on the spot, as before.
    let h_idle = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, COMP)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h_idle, FRAME_VA, 4096)]),
        Ok(0)
    );
    assert!(gpu.nouveau_gem_close(h_idle, COMP));
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.gem_frees, frees + 2);
        assert_eq!(f.unmaps, unmaps + 3);
    }
    assert_eq!(userd(&c0), (2, 2), "no probe on an idle ring");
    // The caller's own ring counts, mapped or not: a buffer of the
    // client's that no VM maps, closed with its two pushes queued.
    let h_own = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let c1 = chan(1);
    assert_eq!(userd(&c1), (0, 2), "A's two submits, unfetched");
    assert!(gpu.nouveau_gem_close(h_own, A));
    assert_eq!(userd(&c1), (0, 3), "a probe behind them");
    assert_eq!(FAKE_RM.lock().gem_frees, frees + 2);
    assert_eq!(run_gpu(1).len(), 3);
    assert!(
        !gpu.nouveau_gem_close(h + 1000, A),
        "no such handle, but a close is a reap point too"
    );
    assert_eq!(FAKE_RM.lock().gem_frees, frees + 3);
    assert_eq!(FAKE_RM.lock().unmaps, unmaps + 3, "nothing was mapped");
    // A ring that never passes holds the memory for the fence timeout,
    // no longer: a ring that has not moved in that long is wedged.
    let h_stuck = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, COMP)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h_stuck, FRAME_VA, 4096)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert!(gpu.nouveau_gem_close(h_stuck, COMP));
    assert_eq!(userd(&c0), (2, 4), "the push and the probe");
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(FAKE_RM.lock().gem_frees, frees + 3, "still held");
    test_clock::advance(syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.gem_frees, frees + 4, "given up on, as a fence would be");
        assert_eq!(f.unmaps, unmaps + 4);
    }
    // The client exits with a close of its own still waiting on its
    // ring: the exit frees the ring, and with it the wait -- and the
    // VAS, so the mapping is dropped rather than unmapped twice.
    let h_exit = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(h_exit, FRAME_VA, 4096)]),
        Ok(0)
    );
    assert_eq!(userd(&c1), (3, 5), "A's two later submits, unfetched");
    assert!(gpu.nouveau_gem_close(h_exit, A));
    assert_eq!(userd(&c1), (3, 6));
    assert_eq!(FAKE_RM.lock().gem_frees, frees + 4);
    gpu.nouveau_release_process(A);
    assert!(!has_chan(1));
    {
        let f = FAKE_RM.lock();
        assert_eq!(f.gem_frees, frees + 5, "freed with the ring");
        assert_eq!(f.unmaps, unmaps + 4, "its VAS went with the context");
        assert_eq!(f.bad, 0);
    }
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A client that exits with a consumer's ACQUIRE queued on its
/// semaphore leaves a zombie whose ring still runs, and the park lands
/// its last payload from the CPU (`write_final_payload`): a close of
/// its buffer that was waiting on that ring must not take the landed
/// payload for the ring's word, only the entries it has consumed.
#[test]
fn a_closed_buffer_of_a_zombie_waits_for_the_zombies_ring_not_for_its_parked_payload() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    const FRAME_VA: u64 = PUSH_VA + 0x10_0000;
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, FRAME_VA, 65536)]), Ok(0));
    assert!(crate::scheme::gem_mmap::add_ref(h, COMP).is_some());
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h, FRAME_VA, 65536)]),
        Ok(0)
    );
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(
            &gpu,
            COMP,
            ch_c,
            &[push(PUSH_VA, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0)
    );
    let (c0, c1) = (chan(0), chan(1));
    assert_eq!(userd(&c1), (0, 2));
    assert_eq!(userd(&c0), (0, 3), "acquire, push, fence");
    let (frees, unmaps) = {
        let f = FAKE_RM.lock();
        (f.gem_frees, f.unmaps)
    };
    assert!(gpu.nouveau_gem_close(h, COMP));
    assert!(
        gpu.nouveau_gem_close(h, A),
        "the last holder, both rings busy"
    );
    assert_eq!(userd(&c1), (0, 3), "a probe on the client's ring");
    assert_eq!(userd(&c0), (0, 4), "and one on the compositor's");
    assert_eq!(FAKE_RM.lock().gem_frees, frees);
    // The client exits before its ring ran: a zombie. Its last payload
    // -- the probe's -- landed at the park; the ring has consumed
    // nothing, and the buffer stays.
    gpu.nouveau_release_process(A);
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1);
    assert!(syncobj::hw_fence_landed(
        sem_va(&c1) as usize - c1.gpu_va as usize + c1.buf,
        2
    ));
    assert_eq!(userd(&c1), (0, 3), "the zombie's ring has not moved");
    assert_eq!(FAKE_RM.lock().gem_frees, frees, "not freed at the exit");
    // The pid comes back before the consumer passed: the close's
    // mappings wear the tombstone with the rest of the zombie's.
    gpu.rekey_zombie_wearing(A);
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert_eq!(
        FAKE_RM.lock().gem_frees,
        frees,
        "nobody has passed anything"
    );
    assert_eq!(run_gpu(0).len(), 5, "the compositor passes its acquire");
    // The consumer passed: the zombie is torn down, ring, VAS and all,
    // whether or not its ring ran (that is what a zombie is for) -- and
    // with the ring gone the close has nothing left to wait for. Its
    // mapping of the freed VAS is dropped, not unmapped a second time.
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert!(gpu.nouveau_zombies.lock().is_empty(), "reaped");
    assert!(!has_chan(1));
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    {
        let f = FAKE_RM.lock();
        assert_eq!(
            f.gem_frees,
            frees + 2,
            "the closed frame, freed with the zombie's ring, and the zombie's own pushbuffer"
        );
        assert_eq!(
            f.unmaps,
            unmaps + 1,
            "the compositor's mapping; the zombie's VAS was gone"
        );
        assert_eq!(f.bad, 0);
    }
    for h in [out, out2] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(COMP);
    gpu.reap_zombie_contexts();
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A wait on a timeline point the producer has already delivered is
/// over. EXEC handed it the producer's NEXT fence to acquire
/// (`pending_hw_fences` looked only at what was in flight), so a
/// consumer that waited for frame N stalled behind frame N+1, on the
/// GPU, in front of pushes nobody asked to hold back; a wait on point
/// 1 fared worse, read as binary it took the HIGHEST fence in flight.
/// Linux drops a dependency on a signaled fence before scheduling.
#[test]
fn a_wait_on_a_point_already_delivered_is_no_acquire_on_the_producers_next_fence() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let tl = syncobj::create(false);
    for point in 1..=2u64 {
        assert_eq!(
            exec(
                &gpu,
                A,
                ch_a,
                &[push(PUSH_VA, 16)],
                &[],
                &[sync_tl(tl, point)]
            ),
            Ok(0)
        );
    }
    assert_eq!(
        run_gpu(1).len(),
        4,
        "two pushes and two fences: points 1 and 2 delivered"
    );
    assert_eq!(syncobj::query(tl), Some(2));
    // Frame 3 still running on A's ring.
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync_tl(tl, 3)]),
        Ok(0)
    );
    let out = syncobj::create(false);
    // 1 ms per clock read: a CPU wait anywhere below ends after 10 s
    // virtual instead of hanging.
    test_clock::set_auto_advance(1_000);
    let t0 = test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 2)],
            &[sync(out)]
        ),
        Ok(0)
    );
    assert!(test_clock::now() - t0 < 1_000_000, "nothing to wait for");
    assert_eq!(
        peer_maps_made(),
        0,
        "no fence to acquire: A's semaphore stays unmapped in B"
    );
    let b = chan(2);
    assert_eq!(userd(&b), (0, 2), "push and fence, no acquire");
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 1
            }
        ],
        "B's ring runs: A's third fence, still in flight, is not its business"
    );
    assert_eq!(syncobj::query(out), Some(1));
    // Point 1 is a timeline point like any other, not a binary wait on
    // the highest fence in flight.
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 1)],
            &[sync(out2)]
        ),
        Ok(0)
    );
    assert_eq!(peer_maps_made(), 0);
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 2
            }
        ]
    );
    assert_eq!(syncobj::query(out2), Some(1));
    // The genuine wait, point 3, is still A's fence in flight: an
    // acquire on B's ring, which stalls until A gets there.
    let out3 = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 3)],
            &[sync(out3)]
        ),
        Ok(0)
    );
    assert_eq!(
        peer_maps_made(),
        1,
        "A's semaphore mapped into B's VAS for it"
    );
    assert_eq!(run_gpu(2), [], "stalls on A's third fence");
    assert_eq!(run_gpu(1).len(), 2);
    let (_, local_va) = peer_map(2, 1).expect("the mapping the RM made");
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Acquire {
                sem_va: local_va,
                payload: 3
            },
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 3
            }
        ]
    );
    assert_eq!(syncobj::query(out3), Some(1));
    test_clock::set_auto_advance(0);
    for h in [tl, out, out2, out3] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The GPFIFO entry's SYNC bit, `SYNC_WAIT` when the host must not
/// fetch the entry before the one ahead of it has completed.
fn gp_sync_wait(ctx: u32, index: u64) -> bool {
    let c = chan(ctx);
    peek(c.buf + FAST_GPFIFO_OFF as usize + index as usize * 8 + 4) >> 31 == 1
}

/// A GPFIFO entry names its push's length in 21 bits of dwords. A
/// longer push was encoded regardless, the field wrapped, and the GPU
/// ran a push of `va_len mod 8 MiB`: the tail of a long command
/// buffer never executed, without a word from anyone; Linux refuses it
/// with EINVAL. And a push flagged NO_PREFETCH (one the GPU itself is
/// still writing: device-generated commands) went into the ring like
/// any other, so the host could fetch its methods before the copy that
/// fills them had landed; the entry now carries `SYNC_WAIT`, as Linux's
/// `nv50_dma_push` writes it.
#[test]
fn a_push_longer_than_an_entry_can_name_is_einval_and_no_prefetch_holds_the_host() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    // One byte over the limit (rounded to a dword), and exactly 8 MiB,
    // which the wrap would have run as a push of nothing.
    for len in [nv::EXEC_PUSH_MAX_LENGTH + 1, 0x80_0000, 0x80_0010] {
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, len)],
                &[],
                &[sync(out)]
            ),
            Err(nv::EINVAL),
            "va_len={:#x}",
            len
        );
    }
    assert!(
        !has_chan(1),
        "refused before the channel was even prepared: nothing reached the ring"
    );
    assert_eq!(syncobj::query(out), Some(0), "and nothing was signaled");
    // The longest push that fits, and a NO_PREFETCH push behind it.
    let longest = nv::EXEC_PUSH_MAX_LENGTH & !3;
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[
                push(PUSH_VA, longest),
                push_no_prefetch(PUSH_VA + 0x100, 32),
                push(PUSH_VA + 0x200, 16)
            ],
            &[],
            &[sync(out)]
        ),
        Ok(0)
    );
    let c = chan(1);
    assert_eq!(userd(&c), (0, 4), "three pushes and the fence");
    assert!(!gp_sync_wait(1, 0), "an ordinary push is prefetched");
    assert!(
        gp_sync_wait(1, 1),
        "the host waits for the push ahead before fetching the one the GPU writes"
    );
    assert!(
        !gp_sync_wait(1, 2),
        "the flag is the push's own, not sticky"
    );
    assert!(!gp_sync_wait(1, 3), "nor the fence's");
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: longest
            },
            Fetched::Push {
                va: PUSH_VA + 0x100,
                len: 32
            },
            Fetched::Push {
                va: PUSH_VA + 0x200,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&c),
                payload: 1
            }
        ],
        "the longest push keeps its whole length in the entry"
    );
    assert_eq!(syncobj::query(out), Some(1));
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}
/// Linux finds every syncobj an EXEC or a VM_BIND names -- to wait on
/// or to signal -- in the caller's own file, so another process's
/// handle is ENOENT and nothing is submitted. Here the handle space is
/// global and only the handle's existence was checked: a client could
/// signal the compositor's release timeline with an EXEC of its own,
/// or wait on its acquire points.
#[test]
fn an_exec_or_a_vm_bind_cannot_name_another_processes_syncobj() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA: u64 = 0x7f_4000_0000;
    let mine = syncobj::create_for(A, false);
    let theirs = syncobj::create_for(B, false);
    let theirs_done = syncobj::create_for(B, true);
    let nobodys = syncobj::create(false);
    // As a sig: ENOENT, nothing on the ring, B's timeline untouched.
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(theirs)]),
        Err(nv::ENOENT)
    );
    // Nothing was submitted: the direct-submit channel behind A's
    // context is not even built until the first EXEC goes through.
    let nothing_on_the_ring = || !has_chan(1) || run_gpu(1).is_empty();
    assert!(nothing_on_the_ring(), "nothing was submitted");
    assert_eq!(syncobj::query_submitted(theirs), Some(0));
    // Mixed in with A's own: still nothing at all (the whole list is
    // looked up first), A's own sig not armed either.
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA, 16)],
            &[],
            &[sync(mine), sync(theirs)]
        ),
        Err(nv::ENOENT)
    );
    assert!(nothing_on_the_ring());
    assert_eq!(syncobj::query_submitted(mine), Some(0));
    // As a wait, even one B already signaled: ENOENT, no wait, no
    // submit.
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA, 16)],
            &[sync(theirs_done)],
            &[sync(mine)]
        ),
        Err(nv::ENOENT)
    );
    assert!(nothing_on_the_ring());
    assert_eq!(syncobj::query_submitted(mine), Some(0));
    // The empty EXEC (NVK's health probe) with syncs: the same.
    assert_eq!(
        exec(&gpu, A, ch_a, &[], &[sync(theirs_done)], &[sync(mine)]),
        Err(nv::ENOENT)
    );
    assert_eq!(syncobj::query_submitted(mine), Some(0));
    // VM_BIND: its sigs and its waits alike, nothing bound.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(ha, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[],
            &[sync(theirs)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(ha, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(theirs_done)],
            &[sync(mine)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(syncobj::query(theirs), Some(0));
    assert_eq!(syncobj::query(mine), Some(0));
    // A's own, and a handle nobody holds (the kernel's): fine.
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA, 16)],
            &[],
            &[sync(mine), sync(nobodys)]
        ),
        Ok(0)
    );
    assert!(!run_gpu(1).is_empty(), "submitted");
    assert_eq!(syncobj::query_submitted(mine), Some(1));
    // Once B's syncobj reaches A through an fd (an opaque import, a
    // reference of A's on it), A may name it.
    assert!(syncobj::add_ref_for(A, theirs));
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(ha, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[],
            &[sync(theirs)]
        ),
        Ok(0)
    );
    assert_eq!(syncobj::query(theirs), Some(1), "signaled by A's bind");
    assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA, 65536)]), Ok(0));
    assert!(syncobj::destroy_for(A, theirs) && syncobj::destroy_for(A, mine));
    assert!(syncobj::destroy_for(B, theirs) && syncobj::destroy_for(B, theirs_done));
    assert!(syncobj::destroy(nobodys));
}

/// `nouveau_uvmm_bind_job_submit` runs `bind_validate_op` over every
/// op of a `VM_BIND` when the job is submitted, before the job waits
/// its in-syncs or touches the VA space, so a batch with a bad op in
/// it does nothing at all. Here the ops were checked as they were
/// applied: op[0]'s UNMAP was already in the RM when op[1] was
/// refused -- the caller told "nothing happened" had lost a live
/// mapping -- and a bad op behind an unsignaled wait was heard of only
/// after the wait had cost its whole deadline.
#[test]
fn a_vm_bind_batch_with_a_bad_op_binds_nothing_and_unbinds_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let _ch_a = client_with_pushbuf(&gpu, A);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA: u64 = 0x7f_3000_0000;
    let bound = driver_maps(&gpu, A).len();
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
    // A's mappings in this test's stretch of VA: (va, size).
    let live = || {
        driver_maps(&gpu, A)
            .iter()
            .filter(|m| (VA..VA + 0x100_0000).contains(&m.1))
            .map(|m| (m.1, m.2))
            .collect::<Vec<_>>()
    };
    assert_eq!(live(), [(VA, 65536)]);
    let bad: [(nv::DrmNouveauVmBindOp, i32, &str); 6] = [
        (
            map(ha + 1000, VA + 0x10_0000, 4096),
            nv::ENOENT,
            "a handle A does not hold",
        ),
        (
            op(
                nv::VM_BIND_OP_MAP,
                nv::VM_BIND_SPARSE,
                0,
                VA + 0x10_0000,
                4096,
            ),
            nv::EOPNOTSUPP,
            "a sparse region",
        ),
        (
            map(ha, VA + 0x10_0001, 4096),
            nv::EINVAL,
            "a VA off the page",
        ),
        (map(ha, VA + 0x10_0000, 0), nv::EINVAL, "an empty range"),
        (
            map_at(ha, VA + 0x10_0000, 4096, 65536),
            nv::EINVAL,
            "a window past the object",
        ),
        (
            op(7, 0, ha, VA + 0x10_0000, 4096),
            nv::EINVAL,
            "an op nobody knows",
        ),
    ];
    for (op_bad, errno, what) in bad {
        // Two good ops first -- an UNMAP of the live mapping and a MAP
        // of a fresh page -- then the bad one; and the bad one first.
        let again = || nv::DrmNouveauVmBindOp { ..op_bad };
        for (place, mut ops) in [
            (
                "last",
                [unmap(VA, 65536), map(ha, VA + 0x20_0000, 4096), again()],
            ),
            (
                "first",
                [again(), unmap(VA, 65536), map(ha, VA + 0x20_0000, 4096)],
            ),
        ] {
            let before = FAKE_RM.lock().calls.len();
            assert_eq!(
                vm_bind_ops(&gpu, A, &mut ops),
                Err(errno),
                "{} ({})",
                what,
                place
            );
            assert_eq!(
                live(),
                [(VA, 65536)],
                "{} ({}): the UNMAP in the same batch did not run, nor the MAP",
                what,
                place
            );
            assert_eq!(
                rm_calls_since(before),
                [] as [&str; 0],
                "{} ({}): nothing reached the RM",
                what,
                place
            );
        }
    }
    // Behind a wait that is not signaled: the bad op is refused at once,
    // not after the deadline the wait would have cost, and the sig
    // stays unsignaled.
    let never = syncobj::create(false);
    let late = syncobj::create(false);
    let _advance = AutoAdvance::of(1_000);
    let t0 = test_clock::now();
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536), map(ha + 1000, VA + 0x10_0000, 4096)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(never)],
            &[sync(late)]
        ),
        Err(nv::ENOENT),
        "the bad op, not EIO for the wait"
    );
    assert!(
        test_clock::now() - t0 < 1_000_000,
        "refused without waiting ({} us)",
        test_clock::now() - t0
    );
    assert_eq!(live(), [(VA, 65536)]);
    assert_eq!(syncobj::query(late), Some(0), "not signaled");
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // The same two good ops on their own: both apply.
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [unmap(VA, 65536), map(ha, VA + 0x20_0000, 4096)]
        ),
        Ok(0)
    );
    assert_eq!(live(), [(VA + 0x20_0000, 4096)]);
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x20_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound);
}

/// `vkQueueBindSparse` is a `VM_BIND` with `RUN_ASYNC`, the submit's
/// wait semaphores as its wait list and its signal semaphores and fence
/// as its sig list (NVK's bind context, `nvkmd_nouveau_bind_ctx_flush`,
/// up to 4096 coalesced ops per call, flushed at every signal -- so a
/// submit with nothing to bind still carries its syncs). Every such
/// call was refused with EOPNOTSUPP, and more than 64 ops too, so any
/// sparse bind with a fence to signal was `DRM_NOUVEAU_VM_BIND failed`
/// and the queue was lost. Linux runs the ops behind the waits and
/// signals the sigs after them; here the ops are synchronous RM calls,
/// so the ioctl waits first, binds, then signals. Without `RUN_ASYNC`
/// a sync list is EINVAL, as in `nouveau_job_init`.
#[test]
fn a_sparse_bind_waits_its_semaphores_and_signals_its_fence_like_vkqueuebindsparse() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA: u64 = 0x7f_2000_0000;
    let bound = driver_maps(&gpu, A).len();
    let ready = syncobj::create(false);
    assert!(syncobj::signal(ready));
    let tl = syncobj::create(false);
    assert!(syncobj::timeline_signal(tl, 3));
    let done = syncobj::create(false);
    let fence_tl = syncobj::create(false);
    // Syncs without RUN_ASYNC: EINVAL, nothing bound, nothing signaled.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(ha, VA, 65536)],
            0,
            &[sync(ready)],
            &[sync(done)]
        ),
        Err(nv::EINVAL),
        "a synchronous bind cannot carry syncs"
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound);
    assert_eq!(syncobj::query(done), Some(0));
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // An op list with no pointer is EINVAL, not a fault.
    let mut r = nv::DrmNouveauVmBind {
        op_count: 1,
        flags: nv::VM_BIND_RUN_ASYNC,
        wait_count: 0,
        sig_count: 0,
        wait_ptr: 0,
        sig_ptr: 0,
        op_ptr: 0,
    };
    assert_eq!(
        call(&gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, A),
        Err(nv::EINVAL),
        "ops with no pointer"
    );
    // The sparse submit: two waits already satisfied, the bind, and a
    // binary and a timeline signal after it.
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(ha, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(ready), sync_tl(tl, 2)],
            &[sync(done), sync_tl(fence_tl, 7)]
        ),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1, "bound");
    assert_eq!(syncobj::query(done), Some(1), "the binary sig");
    assert_eq!(syncobj::query(fence_tl), Some(7), "the timeline sig");
    // A submit with nothing to bind still flushes its syncs.
    let only = syncobj::create(false);
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [],
            nv::VM_BIND_RUN_ASYNC,
            &[],
            &[sync_tl(only, 4)]
        ),
        Ok(0),
        "no ops, one signal"
    );
    assert_eq!(syncobj::query(only), Some(4));
    // A wait that never comes: EIO, the unmap never runs, the sig stays.
    let never = syncobj::create(false);
    let late = syncobj::create(false);
    let before = FAKE_RM.lock().calls.len();
    test_clock::set_auto_advance(1_000);
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(ready), sync(never)],
            &[sync(late)]
        ),
        Err(nv::EIO)
    );
    test_clock::set_auto_advance(0);
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1, "still bound");
    assert_eq!(syncobj::query(late), Some(0), "not signaled");
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // A timeline point not yet reached is a wait too: EIO, not "3 >= 1".
    test_clock::set_auto_advance(1_000);
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync_tl(tl, 5)],
            &[sync(late)]
        ),
        Err(nv::EIO),
        "the timeline is at 3, the bind waits for 5"
    );
    test_clock::set_auto_advance(0);
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1);
    assert_eq!(syncobj::query(late), Some(0));
    // A semaphore A's own EXEC signals, on a ring that never gets to
    // it: the fence times out, syncobj reads the point as reached, and
    // the bind behind it is EIO (a sparse bind behind a hung queue),
    // whichever of the two clocks ticked first.
    let rel = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(rel)]),
        Ok(0)
    );
    test_clock::advance(syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(syncobj::query(rel), Some(1), "reached, by the timeout");
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(rel)],
            &[sync(late)]
        ),
        Err(nv::EIO),
        "the release never happened"
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1);
    assert_eq!(syncobj::query(late), Some(0));
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // An op the bind refuses leaves its sigs unsignaled: the fence of
    // a failed bind never fires (Linux: the job never ran).
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA + 1, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(ready)],
            &[sync(late)]
        ),
        Err(nv::EINVAL),
        "off the page"
    );
    assert_eq!(
        syncobj::query(late),
        Some(0),
        "not signaled: nothing was bound"
    );
    // Unknown handles are ENOENT before anything is bound or signaled,
    // a sig as much as a wait.
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(ready)],
            &[sync(late), sync(0xdead_0001)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(0xdead_0002)],
            &[sync(late)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1);
    assert_eq!(syncobj::query(late), Some(0));
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // NVK's batch: 4096 ops in one call, and 256 syncs.
    let mut many: Vec<_> = (0..4096)
        .map(|i| unmap(VA + 0x1000_0000 + i * 4096, 4096))
        .collect();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut many,
            nv::VM_BIND_RUN_ASYNC,
            &[],
            &[sync(late)]
        ),
        Ok(0),
        "4096 ops"
    );
    assert_eq!(syncobj::query(late), Some(1));
    many.push(unmap(VA + 0x2000_0000, 4096));
    assert_eq!(
        vm_bind_sync(&gpu, A, &mut many, nv::VM_BIND_RUN_ASYNC, &[], &[]),
        Err(nv::EOPNOTSUPP),
        "4097 ops"
    );
    let waits: Vec<_> = (0..256).map(|_| sync(ready)).collect();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [],
            nv::VM_BIND_RUN_ASYNC,
            &waits,
            &[sync_tl(only, 5)]
        ),
        Ok(0),
        "256 waits"
    );
    assert_eq!(syncobj::query(only), Some(5));
    let waits: Vec<_> = (0..257).map(|_| sync(ready)).collect();
    assert_eq!(
        vm_bind_sync(&gpu, A, &mut [], nv::VM_BIND_RUN_ASYNC, &waits, &[]),
        Err(nv::EOPNOTSUPP),
        "257 waits"
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1);
    for h in [ready, tl, done, fence_tl, only, never, late, rel] {
        syncobj::destroy(h);
    }
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// Linux refuses a VM_BIND op that is off the page, empty, wrapping, or
/// (a MAP) a window that does not fit its object, before it touches
/// anything. Here nothing was checked: the "last binder wins" drain had
/// already unmapped whatever the op overlapped when the RM refused it,
/// so a malformed op cost the caller a live mapping and its next EXEC
/// faulted; and a window past the end of the object went to the RM as
/// is, which either refused it or mapped what followed the allocation.
#[test]
fn a_vm_bind_off_a_page_or_past_the_object_is_einval_before_anything_is_unmapped() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    const VA: u64 = 0x3f_f000_0000;
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
    let live = driver_maps(&gpu, A);
    assert_eq!(live.len(), 1);
    let before = FAKE_RM.lock().calls.len();
    let bad = [
        ("a range past the end", map(ha, VA, 65536 + 4096)),
        ("an offset at the end", map_at(ha, VA, 4096, 65536)),
        ("offset and range past the end", map_at(ha, VA, 65536, 4096)),
        ("an offset past the end", map_at(ha, VA, 4096, 65536 + 4096)),
        ("a VA off the page", map(ha, VA + 1, 4096)),
        ("a range off the page", map(ha, VA, 4095)),
        ("an offset off the page", map_at(ha, VA, 4096, 1)),
        ("an empty range", map(ha, VA, 0)),
        ("a range that wraps", map(ha, u64::MAX - 4095, 8192)),
        ("an unmap off the page", unmap(VA + 1, 4096)),
        ("an empty unmap", unmap(VA, 0)),
        ("a map of nothing off the page", map(0, VA, 100)),
    ];
    for (what, op) in bad {
        assert_eq!(vm_bind_ops(&gpu, A, &mut [op]), Err(nv::EINVAL), "{}", what);
        assert_eq!(
            driver_maps(&gpu, A),
            live,
            "{}: the live mapping it overlaps is untouched",
            what
        );
    }
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "refused before the RM was asked anything: no unmap, no map"
    );
    assert_eq!(FAKE_RM.lock().maps_of_ctx(1), [(VA, 65536, 0x06)]);
    // The last page of the object, at its offset: fits.
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [map_at(ha, VA + 0x10_0000, 4096, 65536 - 4096)]
        ),
        Ok(0)
    );
    // An object of 100 bytes is a page to the GPU, as in Linux: the
    // whole page maps, the next one does not.
    let hb = gem_new_rm(&gpu, 100, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(hb, VA + 0x20_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(hb, VA + 0x30_0000, 8192)]),
        Err(nv::EINVAL)
    );
    assert_eq!(driver_maps(&gpu, A).len(), 3);
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A `sync_file` imported into a binary semaphore REPLACES its fence
/// (`drm_syncobj_replace_fence`). The semaphore an EXEC had just
/// signaled kept that fence in flight beside the import, so once A's
/// ring passed it the semaphore read signaled with the imported source
/// still unsignaled, and B's EXEC waiting on it submitted behind a
/// release that had not happened.
#[test]
fn an_import_over_a_semaphore_an_exec_just_signaled_replaces_the_fence_the_ring_is_writing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let sem = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(sem)]),
        Ok(0)
    );
    // A release fence nobody has signaled yet, imported over it.
    let release = syncobj::create(false);
    assert!(syncobj::import_snapshot(sem, release, 1));
    assert_eq!(run_gpu(1).len(), 2, "A's ring passes the superseded fence");
    assert_eq!(
        syncobj::query(sem),
        Some(0),
        "the semaphore now stands for the release, which has not happened"
    );
    let out = syncobj::create(false);
    // 1 ms per clock read: the CPU wait ends after 10 s virtual.
    test_clock::set_auto_advance(1_000);
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(sem)],
            &[sync(out)]
        ),
        Err(nv::EIO),
        "the release never came: nothing is submitted behind it"
    );
    test_clock::set_auto_advance(0);
    assert!(!has_chan(2), "B's ring was never opened");
    assert_eq!(peer_maps_made(), 0, "no fence of A's to acquire");
    assert_eq!(syncobj::query(out), Some(0));
    // The release arrives: the semaphore follows its source and B goes.
    assert!(syncobj::signal(release));
    assert_eq!(syncobj::query(sem), Some(1));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(sem)],
            &[sync(out)]
        ),
        Ok(0)
    );
    assert_eq!(userd(&chan(2)), (0, 2), "push and fence, no acquire");
    assert_eq!(run_gpu(2).len(), 2);
    assert_eq!(syncobj::query(out), Some(1));
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// Mesa's `SYNC_FD` export (`vkGetSemaphoreFdKHR`, `eglDupNativeFenceFDANDROID`
/// through zink: wlroots' GLES renderer hands that fd to the
/// `linux-drm-syncobj-v1` release point) resets the binary semaphore in
/// the same call, while the ring is still on its way to the fence the
/// EXEC attached. The file has to reach on that fence regardless: the
/// consumer's EXEC waits on an import of it, acquires that very fence,
/// and the reset semaphore stays at 0 until its owner signals it again.
#[test]
fn a_sync_file_exported_from_a_semaphore_an_exec_signaled_survives_its_reset() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let sem = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(sem)]),
        Ok(0)
    );
    let fd = syncobj::export_fence(sem).expect("live");
    assert!(syncobj::reset(sem), "Mesa's copy transference");
    assert_eq!(syncobj::query(sem), Some(0));
    // The consumer imports the file and submits behind it: B acquires
    // A's fence on its ring, no CPU wait, no EIO.
    let acquire = syncobj::create(false);
    assert!(syncobj::import_snapshot(acquire, fd, 1));
    let out = syncobj::create(false);
    // (1 ms per clock read: a CPU wait, which there must not be, ends
    // in EIO after 10 s virtual instead of parking the test.)
    test_clock::set_auto_advance(1_000);
    let submitted = exec(
        &gpu,
        B,
        ch_b,
        &[push(PUSH_VA, 16)],
        &[sync(acquire)],
        &[sync(out)],
    );
    test_clock::set_auto_advance(0);
    assert_eq!(submitted, Ok(0), "no CPU wait: the fence is A's, in flight");
    assert_eq!(userd(&chan(2)), (0, 3), "acquire, push and fence");
    assert_eq!(peer_maps_made(), 1, "A's fence, mapped for B's ACQUIRE");
    assert!(run_gpu(2).is_empty(), "B stalls behind A");
    assert_eq!(run_gpu(1).len(), 2, "A lands");
    assert_eq!(syncobj::query(fd), Some(1), "the file reaches on A's fence");
    assert_eq!(syncobj::query(acquire), Some(1));
    assert_eq!(syncobj::query(sem), Some(0), "the reset semaphore does not");
    assert_eq!(run_gpu(2).len(), 3);
    assert_eq!(syncobj::query(out), Some(1));
    for h in [sem, fd, acquire, out] {
        syncobj::destroy(h);
    }
    assert_eq!(FAKE_RM.lock().bad, 0);
}

// ---- The fence-timeout upcall -----------------------------------------
//
// A fence the GPU never writes is `syncobj`'s to give up on: after
// `FENCE_TIMEOUT_US` it advances the point (so the waiter can fail on
// its next probe instead of parking forever) and calls the driver back
// with `(ctx, landing zone, payload, handle, point)`. The driver's side
// of that call, `fast_fence_timeout`, is what these tests drive: the
// values come from `pending_hw_fences`, exactly what `syncobj` would pass,
// so the hook itself (registered once at boot, shared by every test
// binary) stays out of the picture.

/// [`syncobj::pending_hw_fences`] where the test expects at most one.
fn pending_hw_fence(handle: u32, point: u64) -> Option<(usize, u64, u32, u32)> {
    let fences = syncobj::pending_hw_fences(handle, point);
    assert!(
        fences.len() <= 1,
        "{handle:#x}@{point}: {} fences",
        fences.len()
    );
    fences.first().copied()
}

/// The landing zone of context `ctx`, as the CPU (and `syncobj`) sees it.
fn landing_zone_va(c: &FastChan) -> usize {
    c.buf + FAST_SEM_OFF as usize
}

#[test]
fn a_fence_that_never_lands_wedges_the_channel_and_the_clients_next_submit_is_device_lost() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out = syncobj::create(false);
    let out_b = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out_b)]),
        Ok(0)
    );
    let c = chan(1);
    // What syncobj holds for A's fence names A's channel and its zone.
    let (fence_va, fence_gpu_va, payload, ctx) =
        pending_hw_fence(out, 1).expect("A's fence is pending on the ring");
    assert_eq!(ctx, 1);
    assert_eq!(fence_va, landing_zone_va(&c));
    assert_eq!(fence_gpu_va, sem_va(&c));
    assert_eq!(payload, 1);
    // B's GPU runs; A's never does.
    assert_eq!(run_gpu(2).len(), 2);
    assert_eq!(syncobj::poll_pending(), 1);
    assert_eq!(syncobj::query(out_b), Some(1));
    test_clock::advance(crate::scheme::syncobj::FENCE_TIMEOUT_US - 1);
    assert_eq!(
        syncobj::poll_pending(),
        1,
        "one microsecond short of the timeout: still the GPU's"
    );
    assert_eq!(syncobj::query(out), Some(0));
    test_clock::advance(1);
    assert_eq!(syncobj::poll_pending(), 0, "given up on");
    assert_eq!(
        syncobj::query(out),
        Some(1),
        "released, so a waiter fails on its next probe instead of parking"
    );
    assert!(matches!(
        syncobj::wait(&[out], None, true, test_clock::now()),
        syncobj::WaitOutcome::Signaled { .. }
    ));
    assert_eq!(landing_zone(&c), 0, "nothing landed");
    assert!(
        !nv::ctx_is_wedged(1),
        "syncobj released the waiter; latching is the driver's, on the upcall"
    );
    // The upcall, as syncobj makes it.
    gpu.fast_fence_timeout(ctx, fence_va, payload, out, 1);
    assert!(nv::ctx_is_wedged(1));
    assert!(!nv::ctx_is_wedged(2), "B's channel is B's");
    assert_eq!(FAKE_RM.lock().bad, 0);
    // A's next submit: device lost, and nothing more reaches the ring.
    let (get, put) = userd(&c);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA + 0x100, 16)], &[], &[]),
        Err(nv::EIO)
    );
    assert_eq!(userd(&c), (get, put));
    assert_eq!(
        exec(&gpu, A, ch, &[], &[], &[]),
        Err(nv::ENODEV),
        "the health probe says killed"
    );
    // B keeps going.
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(run_gpu(2).len(), 1);
    // The GPU catching up later does not unlatch A.
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(landing_zone(&c), 1);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
        Err(nv::EIO)
    );
    // Exit clears it; A comes back on a fresh channel.
    gpu.nouveau_release_process(A);
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(FAKE_RM.lock().fast_releases, [1]);
    let ch = client_with_pushbuf(&gpu, A);
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_ne!(chan(1).buf, c.buf);
    for h in [out, out_b] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_timeout_for_a_landing_zone_that_is_not_this_channels_does_not_wedge_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    let c = chan(1);
    let zone = landing_zone_va(&c);
    // A zone that is nobody's on this GPU (another GPU's, on a machine
    // with two: the hook fans out to every GPU).
    let elsewhere = 0u32;
    gpu.fast_fence_timeout(1, &elsewhere as *const u32 as usize, 1, out, 1);
    assert!(!nv::ctx_is_wedged(1));
    // A's zone named under another index: ctx 0 (the compositor, never
    // prepared here), one with no slot at all, and B's, not prepared yet.
    for idx in [0, 2, 40] {
        gpu.fast_fence_timeout(idx, zone, 1, out, 1);
        assert!(!nv::ctx_is_wedged(idx), "ctx{}", idx);
    }
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0),
        "A is untouched"
    );
    // A goes away; a late call naming its old zone finds no channel.
    gpu.nouveau_release_process(A);
    gpu.fast_fence_timeout(1, zone, 1, out, 1);
    assert!(!nv::ctx_is_wedged(1));
    // B inherits the index with a zone of its own; A's old one is not it,
    // and a latch anything left on the index is not B's either.
    nv::ctx_set_wedged(1);
    let ch_b = client_with_pushbuf(&gpu, B);
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(ctx_of(&gpu, B), Some((1, true)));
    assert_ne!(landing_zone_va(&chan(1)), zone);
    gpu.fast_fence_timeout(1, zone, 1, out, 1);
    assert!(!nv::ctx_is_wedged(1), "A's stale timeout is not B's");
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    // Its own zone is.
    gpu.fast_fence_timeout(1, landing_zone_va(&chan(1)), 1, out, 1);
    assert!(nv::ctx_is_wedged(1));
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(B);
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_client_that_exits_or_closes_its_syncobj_mid_frame_leaves_nothing_to_time_out() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let kept = syncobj::create(false);
    let closed = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(kept)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(closed)]),
        Ok(0)
    );
    let c = chan(1);
    assert_eq!(syncobj::poll_pending(), 2);
    // Closing the handle with its submit in flight takes the fence with
    // it: there is nothing left to time out ten seconds later, and a
    // client that drops a fence it stopped caring about (a resized
    // swapchain) is not a hung one.
    assert!(syncobj::destroy(closed));
    assert_eq!(syncobj::poll_pending(), 1);
    assert_eq!(
        pending_hw_fence(kept, 1),
        Some((landing_zone_va(&c), sem_va(&c), 1, 1)),
        "the kept fence is still the GPU's"
    );
    // Exit mid-frame: the kept fence is abandoned (signaled, no timeout).
    gpu.nouveau_release_process(A);
    assert!(!syncobj::has_pending());
    assert_eq!(syncobj::query(kept), Some(1));
    assert!(!nv::ctx_is_wedged(1));
    test_clock::advance(crate::scheme::syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(syncobj::poll_pending(), 0);
    assert!(!nv::ctx_is_wedged(1));
    // B on the same index in the meantime: no ghost from A reaches it.
    let ch_b = client_with_pushbuf(&gpu, B);
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(ctx_of(&gpu, B), Some((1, true)));
    assert_eq!(syncobj::poll_pending(), 0);
    assert!(!nv::ctx_is_wedged(1));
    assert!(syncobj::destroy(kept));
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn cpu_prep_on_a_wedged_channel_answers_at_once_and_writes_nothing_to_the_jammed_ring() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    let c = chan(1);
    let (fence_va, _, payload, ctx) = pending_hw_fence(out, 1).unwrap();
    test_clock::advance(crate::scheme::syncobj::FENCE_TIMEOUT_US);
    assert_eq!(syncobj::poll_pending(), 0);
    gpu.fast_fence_timeout(ctx, fence_va, payload, out, 1);
    assert!(nv::ctx_is_wedged(1));
    assert_eq!(userd(&c), (0, 2));
    let bell = doorbell(&gpu);
    // The clock moves 1 ms per read from here on: a prep that waits shows
    // up as virtual time, and one that waits for a GPU that never comes
    // ends (EBUSY after 10 s virtual) instead of hanging.
    test_clock::set_auto_advance(1_000);
    let t0 = test_clock::now();
    assert_eq!(
        cpu_prep(&gpu, h, A),
        Ok(0),
        "a killed channel's fences are done: nouveau says 0, the next submit says why"
    );
    assert!(
        test_clock::now() - t0 < 100_000,
        "answered at once, not after the timeout"
    );
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
    test_clock::set_auto_advance(0);
    assert_eq!(
        userd(&c),
        (0, 2),
        "no probe fence on a ring the GPU stopped reading"
    );
    assert_eq!(doorbell(&gpu), bell);
    assert!(!syncobj::has_pending());
    assert_eq!(
        exec(&gpu, A, ch, &[], &[], &[]),
        Err(nv::ENODEV),
        "and the truth comes from the probe"
    );
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

// ---- Waits across channels ---------------------------------------------
//
// A wait on another client's fence is a GPU ACQUIRE too, once the RM has
// mapped the producer's semaphore into the consumer's VAS
// (`map_peer_fence`: the compositor waiting on a client's frame, a client
// waiting on the compositor's release). With the fake's `peer` switch on,
// the shim hands out one consumer VA per (consumer, producer) pair and
// remembers it until either context is freed, as the C does; `run_gpu`
// resolves an ACQUIRE at such a VA to the producer's landing zone, and an
// ACQUIRE at a VA the channel has no mapping for is the MMU fault it
// would be on hardware.

fn peer_map(consumer: u32, producer: u32) -> Option<(u64, u64)> {
    FAKE_RM
        .lock()
        .peer_maps
        .iter()
        .find(|m| m.0 == consumer && m.1 == producer)
        .map(|m| (m.2, m.3))
}

fn peer_maps_made() -> usize {
    FAKE_RM
        .lock()
        .calls
        .iter()
        .filter(|c| **c == "map_peer_fence")
        .count()
}

#[test]
fn a_wait_on_another_channel_is_a_gpu_acquire_on_the_producers_fence_mapped_into_the_consumer() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    let a = chan(1);
    // 1 ms per clock read from here on: a CPU wait anywhere below (the
    // wrong path, nothing runs the GPU) ends after 10 s virtual instead
    // of hanging.
    test_clock::set_auto_advance(1_000);
    let t0 = test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0),
        "the wait is the GPU's: no CPU wait"
    );
    assert!(
        test_clock::now() - t0 < 1_000_000,
        "submitted without waiting"
    );
    assert_eq!(
        peer_maps_made(),
        1,
        "A's semaphore mapped into B's VAS once"
    );
    let (producer_va, local_va) = peer_map(2, 1).expect("the mapping the RM made");
    assert_eq!(producer_va, sem_va(&a), "of A's fence semaphore");
    let b = chan(2);
    assert_ne!(local_va, sem_va(&b));
    assert_eq!(userd(&b), (0, 3), "acquire, push, fence");
    assert_eq!(
        run_gpu(2),
        [],
        "B's channel stalls on the acquire: A has not run"
    );
    assert_eq!(userd(&b), (0, 3));
    assert_eq!(syncobj::query(out2), Some(0));
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Acquire {
                sem_va: local_va,
                payload: 1
            },
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 1
            }
        ],
        "the acquire in front of the push it guards, on B's own ring"
    );
    assert_eq!(syncobj::query(out2), Some(1));
    // The next wait on A reuses the mapping: nothing more from the RM.
    let out3 = syncobj::create(false);
    let out4 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
        Ok(0)
    );
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(out3)],
            &[sync(out4)]
        ),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(
        run_gpu(2)[0],
        Fetched::Acquire {
            sem_va: local_va,
            payload: 2
        }
    );
    assert_eq!(syncobj::query(out4), Some(1));
    // The other way round is a mapping of its own.
    let out5 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out5)]),
        Ok(0)
    );
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out5)], &[]),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), ["map_peer_fence"]);
    let (producer_va_b, local_va_b) = peer_map(1, 2).unwrap();
    assert_eq!(producer_va_b, sem_va(&b));
    assert_ne!(local_va_b, local_va);
    assert_eq!(run_gpu(2).len(), 2);
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Acquire {
                sem_va: local_va_b,
                payload: 3
            },
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            }
        ]
    );
    test_clock::set_auto_advance(0);
    for h in [out, out2, out3, out4, out5] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    assert!(
        FAKE_RM.lock().peer_maps.is_empty(),
        "both mappings involve A: gone with its context"
    );
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_context_that_goes_away_takes_its_peer_mappings_with_it_and_the_next_tenant_gets_fresh_ones() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    // 1 ms per clock read: every wait below is meant to be the GPU's;
    // one that falls to the CPU ends after 10 s virtual, not never.
    test_clock::set_auto_advance(1_000);
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
        Ok(0)
    );
    let (_, stale) = peer_map(2, 1).unwrap();
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(run_gpu(2).len(), 2);
    // A exits: the RM frees the mapping of A's buffer in B's VAS along
    // with A's context. A comes back on the same index with a new
    // channel; B waiting on it needs a mapping of THAT buffer -- an
    // acquire at the old VA is an MMU fault on B's channel (the
    // compositor's, on the desktop: one client come and gone and the
    // next one's frame kills the compositor).
    gpu.nouveau_release_process(A);
    assert_eq!(peer_map(2, 1), None);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    let a2 = chan(1);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    assert_eq!(
        rm_calls_since(before),
        ["map_peer_fence"],
        "a mapping of the new tenant's buffer"
    );
    let (producer_va, fresh) = peer_map(2, 1).unwrap();
    assert_eq!(producer_va, sem_va(&a2));
    assert_ne!(fresh, stale);
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Acquire {
                sem_va: fresh,
                payload: 1
            },
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            }
        ],
        "the acquire resolves on the new channel's landing zone"
    );
    // The consumer going away is the same: B's VAS is gone, and the
    // next B on that index needs a mapping in ITS VAS.
    gpu.nouveau_release_process(B);
    assert_eq!(peer_map(2, 1), None);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out3 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
        Ok(0)
    );
    let made = peer_maps_made();
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out3)], &[]),
        Ok(0)
    );
    assert_eq!(peer_maps_made(), made + 1);
    let (_, newer) = peer_map(2, 1).unwrap();
    assert_ne!(newer, fresh);
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(
        run_gpu(2)[0],
        Fetched::Acquire {
            sem_va: newer,
            payload: 2
        }
    );
    test_clock::set_auto_advance(0);
    for h in [out, out2, out3] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert!(FAKE_RM.lock().peer_maps.is_empty());
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The X11 acquire is never A's syncobj itself: the client waits on a
/// sync_file IMPORTED from it, or on a MERGE of it with the previous
/// present. Both are links whose fence sits in A's row. Looked up by
/// the importer's handle that fence was invisible, so every such EXEC
/// parked on the CPU inside the ioctl; seen through the link it is the
/// same GPU acquire on A's mapped semaphore as a wait on A's syncobj.
#[test]
fn an_exec_wait_on_an_imported_or_merged_fence_is_a_gpu_acquire_on_the_producers_fence() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    // What Mesa's WSI does: A's fence exported as a sync_file and
    // imported into the client's own semaphore...
    let imported = syncobj::create(false);
    assert!(syncobj::import_snapshot(
        imported,
        out,
        syncobj::export_snapshot(out).unwrap()
    ));
    // ...or merged with the previous present, long done here.
    let previous = syncobj::create(true);
    let merged = syncobj::merge_fences(&[(out, 1), (previous, 1)]);
    assert_eq!(syncobj::query(imported), Some(0));
    assert_eq!(syncobj::query(merged), Some(0));
    let a = chan(1);
    let out2 = syncobj::create(false);
    // 1 ms per clock read: a CPU wait below (the old path, nothing runs
    // the GPU) ends in EIO after 10 s virtual instead of hanging.
    test_clock::set_auto_advance(1_000);
    let t0 = test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(imported), sync(merged)],
            &[sync(out2)]
        ),
        Ok(0),
        "both waits are A's fence: no CPU wait"
    );
    assert!(
        test_clock::now() - t0 < 1_000_000,
        "submitted without waiting"
    );
    assert_eq!(
        peer_maps_made(),
        1,
        "A's semaphore mapped into B's VAS once, for both waits"
    );
    let (producer_va, local_va) = peer_map(2, 1).expect("the mapping the RM made");
    assert_eq!(producer_va, sem_va(&a));
    let b = chan(2);
    assert_eq!(userd(&b), (0, 4), "acquire, acquire, push, fence");
    assert_eq!(
        run_gpu(2),
        [],
        "B's channel stalls on the acquire: A has not run"
    );
    assert_eq!(syncobj::query(out2), Some(0));
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Acquire {
                sem_va: local_va,
                payload: 1
            },
            Fetched::Acquire {
                sem_va: local_va,
                payload: 1
            },
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 1
            }
        ],
        "one acquire per wait, both on A's fence, in front of the push"
    );
    assert_eq!(syncobj::query(imported), Some(1));
    assert_eq!(syncobj::query(merged), Some(1));
    assert_eq!(syncobj::query(out2), Some(1));
    test_clock::set_auto_advance(0);
    for h in [out, imported, previous, merged, out2] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The X11 acquire with BOTH halves still running. Mesa's WSI merges
/// "the compositor released the image" (A's fence, in flight) with "our
/// previous present of it completed" (B's own fence, in flight when the
/// GPU is behind), each transferred into a surrogate it destroys at once.
/// Two fences in flight were "not one ACQUIRE", so the whole wait went
/// to the CPU inside the ioctl until the compositor's frame had run: the
/// client serialised behind the compositor. It is two ACQUIREs -- one on
/// B's own semaphore, one on A's mapped in -- and B submits at once.
#[test]
fn the_x11_acquire_with_both_halves_in_flight_is_two_gpu_acquires_not_a_cpu_wait() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    // B renders the image: its own fence, still running.
    let acq = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(acq)]),
        Ok(0)
    );
    // The compositor composites it (waits for B) and promises the release.
    let rel = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA, 16)],
            &[sync(acq)],
            &[sync(rel)]
        ),
        Ok(0)
    );
    assert_eq!(peer_maps_made(), 1, "B's semaphore mapped into A's VAS");
    // What Mesa's WSI does on the next acquire of that image: a surrogate
    // per half, the two merged, the surrogates destroyed on the spot.
    let s_acq = syncobj::create(false);
    let s_rel = syncobj::create(false);
    assert!(syncobj::transfer(s_acq, 0, acq, 0));
    assert!(syncobj::transfer(s_rel, 0, rel, 0));
    let merged = syncobj::merge_fences(&[(s_acq, 1), (s_rel, 1)]);
    assert!(syncobj::destroy(s_acq));
    assert!(syncobj::destroy(s_rel));
    assert_eq!(syncobj::query(merged), Some(0));
    let out = syncobj::create(false);
    // 1 ms per clock read: a CPU wait (the old path; nothing has run the
    // GPU) ends in EIO after 10 s virtual instead of hanging.
    test_clock::set_auto_advance(1_000);
    let t0 = test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA + 0x100, 16)],
            &[sync(merged)],
            &[sync(out)]
        ),
        Ok(0),
        "both halves are fences on rings: no CPU wait"
    );
    assert!(
        test_clock::now() - t0 < 1_000_000,
        "submitted without waiting"
    );
    test_clock::set_auto_advance(0);
    assert_eq!(peer_maps_made(), 2, "and A's semaphore mapped into B's VAS");
    let (producer_va, local_va) = peer_map(2, 1).expect("the mapping the RM made");
    let a = chan(1);
    let b = chan(2);
    assert_eq!(producer_va, sem_va(&a));
    assert_eq!(
        userd(&b),
        (0, 6),
        "push, fence, acquire (own), acquire (A's), push, fence"
    );
    // B's ring runs its render, passes its own acquire and stalls on A's.
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 1
            },
            Fetched::Acquire {
                sem_va: sem_va(&b),
                payload: 1
            }
        ],
        "its own half is ordered on its own ring; the compositor's holds it"
    );
    assert_eq!(syncobj::query(acq), Some(1));
    assert_eq!(syncobj::query(merged), Some(0));
    assert_eq!(syncobj::query(out), Some(0));
    // The compositor's frame runs: its acquire on B passes, it releases.
    assert_eq!(run_gpu(1).len(), 3);
    assert_eq!(syncobj::query(rel), Some(1));
    assert_eq!(
        run_gpu(2),
        [
            Fetched::Acquire {
                sem_va: local_va,
                payload: 1
            },
            Fetched::Push {
                va: PUSH_VA + 0x100,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&b),
                payload: 2
            }
        ]
    );
    assert_eq!(syncobj::query(merged), Some(1));
    assert_eq!(syncobj::query(out), Some(1));
    for h in [acq, rel, merged, out] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The same merge when the RM refuses to map the compositor's semaphore:
/// the wait is all or nothing. Emitting the ACQUIRE for B's own half and
/// forgetting the other would let B's push run before the compositor had
/// released the image; the whole handle waits on the CPU instead, and
/// with nothing running the GPU that is the EIO after 10 s virtual.
#[test]
fn a_merge_whose_other_half_cannot_be_mapped_waits_on_the_cpu_as_a_whole() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    FAKE_RM.lock().peer = true;
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let acq = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(acq)]),
        Ok(0)
    );
    let rel = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA, 16)],
            &[sync(acq)],
            &[sync(rel)]
        ),
        Ok(0)
    );
    let merged = syncobj::merge_fences(&[(acq, 1), (rel, 1)]);
    let out = syncobj::create(false);
    FAKE_RM.lock().peer = false;
    test_clock::set_auto_advance(1_000);
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA + 0x100, 16)],
            &[sync(merged)],
            &[sync(out)]
        ),
        Err(nv::EIO),
        "no half on the GPU: the CPU waited for the compositor, which never ran"
    );
    test_clock::set_auto_advance(0);
    assert_eq!(peer_maps_made(), 2, "asked for A's semaphore in B's VAS...");
    assert_eq!(
        FAKE_RM.lock().peer_maps.len(),
        1,
        "...and refused: only the compositor's mapping of B exists"
    );
    let b = chan(2);
    assert_eq!(userd(&b), (0, 2), "only the render: nothing was submitted");
    assert_eq!(syncobj::query(out), Some(0));
    assert_eq!(run_gpu(2).len(), 2);
    assert_eq!(run_gpu(1).len(), 3);
    assert_eq!(syncobj::query(merged), Some(1));
    for h in [acq, rel, merged, out] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The same through a link on the client's own channel: no mapping, the
/// acquire on its own semaphore, exactly as a wait on the syncobj itself.
#[test]
fn an_exec_wait_on_an_import_of_its_own_channels_fence_needs_no_mapping() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    let imported = syncobj::create(false);
    assert!(syncobj::import_snapshot(imported, out, 1));
    let before = FAKE_RM.lock().calls.len();
    // 1 us per clock read: a CPU wait here ends in EIO after 10 s
    // virtual, a failure rather than a hang.
    test_clock::set_auto_advance(1);
    let t0 = test_clock::now();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch_a,
            &[push(PUSH_VA + 0x100, 16)],
            &[sync(imported)],
            &[sync(out2)]
        ),
        Ok(0),
        "the import's fence is pending on this very channel: no CPU wait"
    );
    test_clock::set_auto_advance(0);
    assert!(test_clock::now() - t0 < 1_000, "submitted without waiting");
    assert_eq!(rm_calls_since(before), [] as [&str; 0], "no peer mapping");
    let a = chan(1);
    assert_eq!(userd(&a), (0, 5), "push, fence, acquire, push, fence");
    assert_eq!(
        run_gpu(1),
        [
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&a),
                payload: 1
            },
            Fetched::Acquire {
                sem_va: sem_va(&a),
                payload: 1
            },
            Fetched::Push {
                va: PUSH_VA + 0x100,
                len: 16
            },
            Fetched::Release {
                sem_va: sem_va(&a),
                payload: 2
            }
        ],
        "the acquire on its own semaphore, in front of the push it guards"
    );
    assert_eq!(syncobj::query(imported), Some(1));
    assert_eq!(syncobj::query(out2), Some(1));
    for h in [out, imported, out2] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

// ----- The compositor's singleton: claimed through the ladder, reset at exit -----

/// A GPU whose fake RM carries the compositor's ladder too, so ctx 0 is
/// claimed the way the desktop claims it: through `CHANNEL_ALLOC`.
fn gpu_rm_ladder() -> NvidiaGpu {
    let gpu = gpu_rm_fast();
    gpu.ctx0_owner.store(0, Ordering::Release);
    FAKE_RM.lock().ladder = true;
    gpu
}

fn ctx0_owner(gpu: &NvidiaGpu) -> u64 {
    gpu.ctx0_owner.load(Ordering::Acquire)
}

fn ctx0_resets() -> u32 {
    FAKE_RM.lock().ctx0_resets
}

fn step17_builds() -> u32 {
    FAKE_RM.lock().step17_builds
}

#[test]
fn the_compositor_claims_ctx0_through_the_ladder_once_and_keeps_it_across_its_throwaway_channels() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    assert_eq!(ctx0_owner(&gpu), 0);
    // The ladder refusing at either step, or stopping part-way: no
    // channel, no claim, nothing built behind index 0.
    FAKE_RM.lock().fail_step16 = true;
    assert_eq!(
        channel_alloc(&gpu, COMP).map(|c| c.channel),
        Err(nv::ENODEV)
    );
    FAKE_RM.lock().fail_step16 = false;
    FAKE_RM.lock().incomplete_step16 = true;
    assert_eq!(
        channel_alloc(&gpu, COMP).map(|c| c.channel),
        Err(nv::ENODEV),
        "a ladder with no context share"
    );
    FAKE_RM.lock().incomplete_step16 = false;
    FAKE_RM.lock().fail_step17 = true;
    assert_eq!(
        channel_alloc(&gpu, COMP).map(|c| c.channel),
        Err(nv::ENODEV)
    );
    FAKE_RM.lock().fail_step17 = false;
    FAKE_RM.lock().incomplete_step17 = true;
    assert_eq!(
        channel_alloc(&gpu, COMP).map(|c| c.channel),
        Err(nv::ENODEV),
        "a channel that never got scheduled"
    );
    FAKE_RM.lock().incomplete_step17 = false;
    assert_eq!(ctx0_owner(&gpu), 0, "nothing claimed");
    assert_eq!(rm_backed_channels(&gpu, COMP), 0);
    assert_eq!(step17_builds(), 0);
    assert!(!FAKE_RM.lock().ctxs.contains(&0));
    // The first compositor channel: the ladder, the singleton channel,
    // the notifier cached for the failure path, and the sticky claim.
    nv::set_chan_notifier_pa(0);
    assert_eq!(nv::chan_notifier_pa_cached(), None);
    let before = FAKE_RM.lock().calls.len();
    let c = channel_alloc(&gpu, COMP).unwrap();
    assert_eq!(c.channel, 0);
    assert_eq!(
        c.notifier_handle, 0x6000,
        "the singleton channel's notifier"
    );
    assert_eq!(ctx0_owner(&gpu), COMP);
    assert_eq!(
        ctx_of(&gpu, COMP),
        None,
        "no client context: the compositor IS context 0"
    );
    assert_eq!(rm_backed_channels(&gpu, COMP), 1);
    let calls = rm_calls_since(before);
    assert_eq!(&calls[..2], ["step16", "step17"]);
    assert!(calls.contains(&"chan_notifier_pa"));
    assert!(!calls.contains(&"ctx_alloc"), "no client context built");
    assert_eq!(nv::chan_notifier_pa_cached(), Some(notifier_pa()));
    // labwc runs two Vulkan instances: the second channel of the same
    // pid rides the cached ladder.
    assert_eq!(channel_alloc(&gpu, COMP).unwrap().channel, 1);
    assert_eq!(step17_builds(), 1, "the ladder is a singleton");
    assert_eq!(rm_backed_channels(&gpu, COMP), 2);
    // A client meanwhile is a client: a context of its own.
    assert_eq!(channel_alloc(&gpu, A).unwrap().notifier_handle, 0x6001);
    assert_eq!(ctx_of(&gpu, A), Some((1, true)));
    // The compositor freeing every channel it has (NVK's throwaway
    // enumeration context, in both instances) keeps the sticky role:
    // the next client is still a client, and the compositor's next
    // channel is still ctx 0, on the same singleton channel.
    assert_eq!(channel_free(&gpu, 0, COMP), Ok(0));
    assert_eq!(channel_free(&gpu, 1, COMP), Ok(0));
    assert_eq!(rm_backed_channels(&gpu, COMP), 0);
    assert_eq!(ctx0_owner(&gpu), COMP, "sticky");
    assert_eq!(channel_alloc(&gpu, B).unwrap().notifier_handle, 0x6002);
    assert_eq!(ctx_of(&gpu, B), Some((2, true)));
    let again = channel_alloc(&gpu, COMP).unwrap();
    assert_eq!(again.notifier_handle, 0x6000);
    assert_eq!(ctx_of(&gpu, COMP), None);
    assert_eq!(step17_builds(), 1);
    assert_eq!(ctx0_resets(), 0, "a CHANNEL_FREE is not an exit");
    // And its submits go down the singleton's own ring, direct, from a
    // buffer bound in the ladder's VAS.
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, COMP)
        .unwrap()
        .handle;
    assert_eq!(
        vm_bind_ops(&gpu, COMP, &mut [map(h, PUSH_VA, 65536)]),
        Ok(0)
    );
    assert_eq!(
        FAKE_RM
            .lock()
            .maps_of_ctx(0)
            .iter()
            .map(|m| (m.0, m.1))
            .collect::<Vec<_>>(),
        [(PUSH_VA, 65536)],
        "bound in context 0"
    );
    let out = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            COMP,
            again.channel as u32,
            &[push(PUSH_VA, 16)],
            &[],
            &[sync(out)]
        ),
        Ok(0)
    );
    let ring = run_gpu(0);
    assert_eq!(ring.len(), 2, "the push and its fence");
    assert_eq!(
        ring[0],
        Fetched::Push {
            va: PUSH_VA,
            len: 16
        }
    );
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(COMP);
    assert_eq!(ctx0_owner(&gpu), 0);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn the_compositors_exit_gives_ctx0_back_and_the_respawn_gets_a_fresh_channel_and_fresh_peer_mappings(
) {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    // 1 ms per clock read: every wait below is meant to be the GPU's;
    // one that falls to the CPU ends after 10 s virtual, not never.
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    assert_eq!(ctx0_owner(&gpu), COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    // A frame each way: the compositor waits on the client's, and the
    // client on the compositor's (the buffer's release).
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(
            &gpu,
            COMP,
            ch_c,
            &[push(PUSH_VA, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    assert_eq!(peer_maps_made(), 2, "one mapping each way");
    let (_, stale) = peer_map(0, 1).unwrap();
    let (_, stale_a) = peer_map(1, 0).unwrap();
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(run_gpu(0).len(), 3, "acquire, push, fence");
    assert_eq!(run_gpu(1).len(), 2, "acquire, push");
    let old = chan(0);
    // The compositor dies mid-session. Its window is released before
    // the channel behind it, the singleton is torn down, the role is
    // free, and the RM has dropped every mapping context 0 took part
    // in -- both directions.
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(COMP);
    assert_eq!(ctx0_owner(&gpu), 0, "the role is free again");
    let calls = rm_calls_since(before);
    let released = calls
        .iter()
        .position(|c| *c == "exec_fast_release")
        .expect("the window released");
    let reset = calls
        .iter()
        .position(|c| *c == "ctx0_reset")
        .expect("the singleton reset");
    assert!(released < reset, "the window before the channel behind it");
    assert_eq!(ctx0_resets(), 1);
    assert!(!has_chan(0));
    assert_eq!(peer_map(0, 1), None);
    assert_eq!(peer_map(1, 0), None);
    // The RM dropped both mappings with the channel; so must the driver's
    // cache, or a client that outlives the compositor is waited on --
    // and waits -- through the VAs the RM has already freed.
    assert!(
        gpu.nouveau_peer_fence
            .lock()
            .keys()
            .all(|&(consumer, producer)| consumer != 0 && producer != 0),
        "no mapping of context 0 is remembered past its reset"
    );
    // The clients go with it (their socket is gone)...
    gpu.nouveau_release_process(A);
    // ...and it comes back. Its CHANNEL_ALLOC rebuilds the singleton
    // channel: a new ring, a new window, not the dead compositor's. A
    // new client takes the dead one's index, and the two wait on each
    // other through mappings of the NEW channels' fence pages.
    let t0 = test_clock::now();
    let ch_c = client_with_pushbuf(&gpu, COMP2);
    assert!(
        test_clock::now().wrapping_sub(t0) < CTX0_RESET_WAIT_US,
        "the teardown is over: nothing to wait for"
    );
    assert_eq!(ctx0_owner(&gpu), COMP2);
    assert_eq!(step17_builds(), 2, "a new channel behind index 0");
    let ch_b = client_with_pushbuf(&gpu, B);
    assert_eq!(ctx_of(&gpu, B), Some((1, true)), "the dead client's index");
    let out3 = syncobj::create(false);
    let out4 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
        Ok(0)
    );
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(
            &gpu,
            COMP2,
            ch_c,
            &[push(PUSH_VA, 16)],
            &[sync(out3)],
            &[sync(out4)]
        ),
        Ok(0)
    );
    assert_ne!(chan(0).buf, old.buf, "a fresh window");
    assert!(
        rm_calls_since(before).contains(&"map_peer_fence"),
        "the client's fence mapped into the new compositor's VAS"
    );
    let (producer_va, fresh) = peer_map(0, 1).expect("a mapping the RM holds");
    assert_eq!(producer_va, sem_va(&chan(1)));
    assert_ne!(fresh, stale);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out4)], &[]),
        Ok(0)
    );
    assert!(
        rm_calls_since(before).contains(&"map_peer_fence"),
        "and the new compositor's fence into the client's"
    );
    let (producer_va, fresh_a) = peer_map(1, 0).expect("a mapping the RM holds");
    assert_eq!(producer_va, sem_va(&chan(0)), "of the NEW channel's fence");
    assert_ne!(fresh_a, stale_a);
    assert_eq!(run_gpu(1).len(), 2);
    let ring = run_gpu(0);
    assert_eq!(ring.len(), 3, "acquire, push, fence: no fault");
    assert!(
        matches!(ring[0], Fetched::Acquire { sem_va, .. } if sem_va == fresh),
        "the acquire resolves on the client's landing zone: {:?}",
        ring[0]
    );
    let ring = run_gpu(1);
    assert_eq!(ring.len(), 2, "acquire, push: no fault");
    assert!(
        matches!(ring[0], Fetched::Acquire { sem_va, .. } if sem_va == fresh_a),
        "the acquire resolves on the new compositor's landing zone: {:?}",
        ring[0]
    );
    test_clock::set_auto_advance(0);
    for h in [out, out2, out3, out4] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(COMP2);
    assert_eq!(ctx0_owner(&gpu), 0);
    assert_eq!(ctx0_resets(), 2);
    assert!(FAKE_RM.lock().peer_maps.is_empty());
    assert_eq!(FAKE_RM.lock().bad, 0);
}

#[test]
fn a_compositor_that_freed_its_channels_before_exiting_still_gives_ctx0_back() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    let c = channel_alloc(&gpu, COMP).unwrap().channel;
    assert_eq!(ctx0_owner(&gpu), COMP);
    // A client freeing its channel and leaving is its own business.
    let ca = channel_alloc(&gpu, A).unwrap().channel;
    assert_eq!(channel_free(&gpu, ca, A), Ok(0));
    gpu.nouveau_release_process(A);
    assert_eq!(ctx0_owner(&gpu), COMP);
    assert_eq!(ctx0_resets(), 0);
    // A clean compositor exit: NVK destroys its contexts (CHANNEL_FREE)
    // before the device closes, so by the time the process goes there
    // is no channel of its own left in the table to infer the role
    // from. The role is the sticky claim, and that is what exits.
    assert_eq!(channel_free(&gpu, c, COMP), Ok(0));
    assert_eq!(rm_backed_channels(&gpu, COMP), 0);
    gpu.nouveau_release_process(COMP);
    assert_eq!(ctx0_owner(&gpu), 0, "the singleton is given back");
    assert_eq!(ctx0_resets(), 1, "and its channel torn down");
    // The respawn is the compositor again: ctx 0 on a rebuilt channel,
    // not a GL client on a context of its own beside a singleton the
    // dead one still holds.
    let c2 = channel_alloc(&gpu, COMP2).unwrap();
    assert_eq!(c2.notifier_handle, 0x6000);
    assert_eq!(ctx0_owner(&gpu), COMP2);
    assert_eq!(ctx_of(&gpu, COMP2), None);
    assert_eq!(step17_builds(), 2);
    // A stranger's exit, or the same exit twice, resets nothing more.
    gpu.nouveau_release_process(STRANGER);
    assert_eq!(ctx0_resets(), 1);
    assert_eq!(ctx0_owner(&gpu), COMP2);
    gpu.nouveau_release_process(COMP2);
    assert_eq!(ctx0_resets(), 2);
    gpu.nouveau_release_process(COMP2);
    assert_eq!(ctx0_resets(), 2);
    assert_eq!(ctx0_owner(&gpu), 0);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The desktop's every frame: the compositor (context 0) composites a
/// client's buffer behind an ACQUIRE on the client's fence, through a
/// peer mapping of the client's semaphore page. The client closes its
/// window before the compositor's ring got there. `ctx_free` right then
/// frees the page and the mapping, and the compositor's ACQUIRE faults
/// on a VA nothing maps any more -- an MMU fault on the COMPOSITOR's
/// channel. So the client's context outlives its process until the
/// compositor has consumed what it had queued.
#[test]
fn a_client_that_exits_with_the_compositors_acquire_queued_keeps_its_fence_page_until_the_compositor_has_passed(
) {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    assert_eq!(ctx0_owner(&gpu), COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    assert_eq!(ctx_of(&gpu, A), Some((1, true)));
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(
            &gpu,
            COMP,
            ch_c,
            &[push(PUSH_VA, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0)
    );
    let (producer_va, _) = peer_map(0, 1).expect("the client's semaphore in the compositor's VAS");
    assert_eq!(producer_va, sem_va(&chan(1)));
    assert_eq!(
        userd(&chan(0)),
        (0, 3),
        "acquire, push, release: queued, not fetched"
    );
    // The window closes. Nothing has run yet on either ring.
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(A);
    assert_eq!(
        ctx_of(&gpu, A),
        None,
        "off the pid registry: no late submit routes here"
    );
    assert!(
        !rm_calls_since(before).contains(&"ctx_free"),
        "the channel is NOT freed while the compositor's ACQUIRE is queued: {:?}",
        rm_calls_since(before)
    );
    assert!(has_chan(1));
    assert!(peer_map(0, 1).is_some(), "the compositor's mapping stays");
    assert_eq!(
        syncobj::query(out),
        Some(1),
        "the CPU side of the client's fence is released at once, as a killed channel's"
    );
    assert_eq!(
        landing_zone(&chan(1)),
        1,
        "the landing zone carries the last payload the client issued, written by the CPU"
    );
    // A new client does not get the zombie's index: its page is still
    // the one the compositor's ring is about to read.
    let ch_b = client_with_pushbuf(&gpu, B);
    assert_eq!(
        ctx_of(&gpu, B),
        Some((2, true)),
        "the zombie's index is not reused"
    );
    // The compositor queues its next frame before its ring has moved:
    // still nothing freed, the ACQUIRE is still ahead of GPGet.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA + 0x100, 16)], &[], &[]),
        Ok(0)
    );
    assert!(!rm_calls_since(before).contains(&"ctx_free"));
    assert!(has_chan(1));
    // The compositor's ring runs: the ACQUIRE passes on the page the
    // dead client's ring never wrote (its own ring never ran), and no
    // entry faults.
    let fetched = run_gpu(0);
    assert_eq!(
        fetched.len(),
        4,
        "acquire, push, release, push: {:?}",
        fetched
    );
    assert!(
        matches!(fetched[0], Fetched::Acquire { sem_va, payload: 1 } if sem_va == peer_map(0, 1).unwrap().1),
        "the ACQUIRE on the dead client's page passed: {:?}",
        fetched[0]
    );
    assert!(
        !fetched.iter().any(|f| matches!(f, Fetched::Fault { .. })),
        "no MMU fault on the compositor's channel: {:?}",
        fetched
    );
    // The compositor's next submit finds the zombie's waiter passed and
    // finishes the teardown: the channel, the page and the mapping go.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA + 0x200, 16)], &[], &[]),
        Ok(0)
    );
    assert!(
        rm_calls_since(before).contains(&"ctx_free"),
        "the dead client's channel freed once the compositor passed: {:?}",
        rm_calls_since(before)
    );
    assert!(!has_chan(1));
    assert_eq!(peer_map(0, 1), None);
    assert!(gpu.nouveau_zombies.lock().is_empty());
    assert_eq!(
        run_gpu(0).len(),
        1,
        "and the compositor's ring is none the worse"
    );
    gpu.nouveau_release_process(B);
    let _ = ch_b;
    gpu.nouveau_release_process(COMP);
    assert!(!has_chan(0));
    assert!(!has_chan(2));
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// Pids are recycled, and fast: the pool is a FIFO of freed ids that
/// thread churn drains in seconds. A new process wearing the dead
/// client's pid must not share the pid-keyed state (context, GEM
/// objects, mappings, channels, class objects, PRIME references) with
/// the zombie -- and the zombie must not be torn down for it either:
/// its consumer still has an ACQUIRE queued on its page, the fault it
/// was parked against. So the zombie's state is re-keyed to a tombstone
/// of its own, the new process starts with nothing of it, and the
/// zombie's teardown, once the consumer has passed, takes nothing of
/// the new process's.
#[test]
fn a_recycled_pid_owns_nothing_of_the_zombie_and_the_zombie_still_waits_for_its_consumer() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = channel_alloc(&gpu, A).unwrap().channel as u32;
    let h_a = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h_a, PUSH_VA, 65536)]), Ok(0));
    // A class object of A's: reaped with the zombie, not with the new A.
    assert_eq!(
        subchan_new(u64::from(ch_a), 0xc597, 0x1000).send(&gpu, A),
        Ok(0)
    );
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
        Ok(0)
    );
    gpu.nouveau_release_process(A);
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1, "a zombie");
    let old = chan(1);
    // The pid comes back as a new process and allocates a channel: the
    // zombie is not torn down (its consumer has not passed), and the new
    // process owns nothing of it.
    let before = FAKE_RM.lock().calls.len();
    let bytes_before = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed);
    let ch_a2 = client_with_pushbuf(&gpu, A);
    let calls = rm_calls_since(before);
    assert!(
        !calls.contains(&"ctx_free") && !calls.contains(&"class_free"),
        "the zombie waits for its consumer still: {:?}",
        calls
    );
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1);
    assert_eq!(
        ctx_of(&gpu, A),
        Some((2, true)),
        "a context of its own, not the zombie's index"
    );
    assert!(
        peer_map(0, 1).is_some(),
        "the compositor's mapping of the zombie's page stays"
    );
    assert_eq!(
        NOUVEAU_GEM_BYTES.load(Ordering::Relaxed) - bytes_before,
        65536,
        "the new process's pushbuf, on top of the zombie's"
    );
    assert_eq!(
        gem_info(&gpu, h_a, A).map(|_| ()),
        Err(nv::ENOENT),
        "the zombie's buffer is not the new process's to touch"
    );
    assert_eq!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.va == PUSH_VA && m.owner_pid != COMP)
            .count(),
        2,
        "the zombie's mapping in its VAS and the new process's in its own: the \
         new bind at the same VA replaced nothing of the zombie's"
    );
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a2, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(
        run_gpu(2).len(),
        2,
        "push and fence on a channel of its own"
    );
    assert_ne!(chan(2).buf, old.buf);
    // The zombie's ring and its consumer run: the compositor passes its
    // ACQUIRE on a page that is still there.
    let zombie_ran = run_gpu(1);
    assert_eq!(zombie_ran.len(), 2, "the zombie's push and fence");
    assert!(
        !zombie_ran
            .iter()
            .any(|f| matches!(f, Fetched::Fault { .. })),
        "its pushbuf is still mapped in its VAS: {:?}",
        zombie_ran
    );
    assert_eq!(run_gpu(0).len(), 2, "acquire and push, no fault");
    // Now the zombie is due. Its teardown takes its own state -- the
    // channel, the pushbuf, the mapping, the class object -- and nothing
    // of the new process's.
    let before = FAKE_RM.lock().calls.len();
    let bytes_before = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed);
    gpu.reap_zombie_contexts();
    let calls = rm_calls_since(before);
    assert!(
        calls.contains(&"class_free"),
        "the zombie's class object: {:?}",
        calls
    );
    assert!(calls.contains(&"ctx_free"), "and its context: {:?}", calls);
    assert!(gpu.nouveau_zombies.lock().is_empty());
    assert_eq!(
        bytes_before - NOUVEAU_GEM_BYTES.load(Ordering::Relaxed),
        65536,
        "the zombie's pushbuf went, and only it"
    );
    assert_eq!(peer_map(0, 1), None);
    assert!(!has_chan(1));
    assert!(
        !gpu.nouveau_channels.lock().iter().any(|c| c.ctx_idx == 1),
        "the zombie's channel entry went with it"
    );
    assert_eq!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.owner_pid == A)
            .count(),
        1,
        "the new process's mapping; the zombie's is gone"
    );
    assert_eq!(
        ctx_of(&gpu, A),
        Some((2, true)),
        "the new process keeps its context"
    );
    assert!(has_chan(2));
    let out3 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a2, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
        Ok(0),
        "and renders on"
    );
    assert_eq!(run_gpu(2).len(), 2);
    assert!(syncobj::destroy(out));
    assert!(syncobj::destroy(out2));
    assert!(syncobj::destroy(out3));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The rebuild of a wedged context a consumer still has an ACQUIRE
/// queued on: the context waits as a zombie, as on exit, and the
/// process renders on in a new one meanwhile. The zombie's teardown
/// takes the old context's own (the mapping in its VAS) and nothing of
/// the live process's; the process's exit, later, takes nothing of the
/// zombie's.
#[test]
fn a_wedged_context_a_consumer_waits_on_is_a_zombie_while_its_process_renders_on() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = channel_alloc(&gpu, A).unwrap().channel as u32;
    let h_a = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h_a, PUSH_VA, 65536)]), Ok(0));
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
        Ok(0)
    );
    // A buffer of A's that B imports and B's ring still reads when A,
    // the last holder, closes it: its free, and the unmap of its
    // mappings -- A's in the VAS about to go among them -- wait for
    // B's ring.
    let ch_b = client_with_pushbuf(&gpu, B);
    const FRAME_VA: u64 = PUSH_VA + 0x10_0000;
    let h2 = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h2, FRAME_VA, 65536)]), Ok(0));
    assert!(crate::scheme::gem_mmap::add_ref(h2, B).is_some());
    assert_eq!(vm_bind_ops(&gpu, B, &mut [map(h2, FRAME_VA, 65536)]), Ok(0));
    assert_eq!(exec(&gpu, B, ch_b, &[push(FRAME_VA, 16)], &[], &[]), Ok(0));
    assert!(gpu.nouveau_gem_close(h2, B));
    assert!(gpu.nouveau_gem_close(h2, A));
    assert_eq!(gpu.nouveau_deferred_frees.lock().len(), 1);
    // A's fence never lands: the timeout's upcall latches its context.
    let (fence_va, _, payload, ctx) = pending_hw_fence(out, 1).unwrap();
    assert_eq!(ctx, 1);
    gpu.fast_fence_timeout(1, fence_va, payload, out, 1);
    assert!(nv::ctx_is_wedged(1));
    assert_eq!(channel_free(&gpu, ch_a as i32, A), Ok(0));
    // The compositor's ACQUIRE is still queued: the context is kept.
    let before = FAKE_RM.lock().calls.len();
    let bytes = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed);
    let ch_a2 = client_with_pushbuf(&gpu, A);
    let calls = rm_calls_since(before);
    assert!(
        !calls.contains(&"ctx_free"),
        "a zombie until its consumer passes: {:?}",
        calls
    );
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1);
    assert_eq!(ctx_of(&gpu, A), Some((3, true)), "a fresh context");
    assert!(!nv::ctx_is_wedged(3));
    assert!(nv::ctx_is_wedged(1), "the zombie keeps its verdict");
    assert!(
        peer_map(0, 1).is_some(),
        "the compositor's mapping of its page stays"
    );
    assert_eq!(
        NOUVEAU_GEM_BYTES.load(Ordering::Relaxed) - bytes,
        65536,
        "the new pushbuf, and nothing of A's freed"
    );
    assert!(gem_info(&gpu, h_a, A).is_ok(), "A's buffer is A's still");
    assert_eq!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.va == PUSH_VA && m.owner_pid != COMP && m.owner_pid != B)
            .count(),
        2,
        "the zombie's mapping in its VAS and A's in the new one"
    );
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a2, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0),
        "renders on"
    );
    assert_eq!(run_gpu(3).len(), 2);
    assert_eq!(syncobj::query(out2), Some(1));
    // The compositor passes its ACQUIRE on a page that is still there,
    // and the zombie is due.
    assert_eq!(run_gpu(0).len(), 2);
    let before = FAKE_RM.lock().calls.len();
    let bytes = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed);
    gpu.reap_zombie_contexts();
    let calls = rm_calls_since(before);
    assert!(calls.contains(&"ctx_free"), "its context: {:?}", calls);
    assert!(!calls.contains(&"gem_free"), "nothing of A's: {:?}", calls);
    assert!(gpu.nouveau_zombies.lock().is_empty());
    assert!(!nv::ctx_is_wedged(1));
    // The closed buffer still waits for B's ring, with B's mapping
    // only: A's went with the VAS, and unmapping it later would be a
    // stale handle in the RM.
    {
        let deferred = gpu.nouveau_deferred_frees.lock();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].mappings.len(), 1);
        assert_eq!(deferred[0].mappings[0].owner_pid, B);
    }
    assert_eq!(NOUVEAU_GEM_BYTES.load(Ordering::Relaxed), bytes);
    assert_eq!(peer_map(0, 1), None);
    assert!(!has_chan(1));
    assert_eq!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.owner_pid == A)
            .count(),
        1,
        "A's mapping in its new VAS; the zombie's is gone"
    );
    assert_eq!(ctx_of(&gpu, A), Some((3, true)));
    assert_eq!(
        exec(&gpu, A, ch_a2, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0),
        "and on"
    );
    assert_eq!(run_gpu(3).len(), 1);
    // B's ring passes the closed buffer; A's exit takes A's: both
    // pushbufs, and the closed buffer, B's mapping of it unmapped and
    // nothing of the VAS that is gone.
    assert_eq!(run_gpu(2).len(), 2, "B's push and the probe");
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(A);
    let calls = rm_calls_since(before);
    assert_eq!(calls.iter().filter(|c| **c == "gem_free").count(), 3);
    assert_eq!(calls.iter().filter(|c| **c == "vm_bind_unmap").count(), 1);
    assert!(gpu.nouveau_deferred_frees.lock().is_empty());
    assert!(syncobj::destroy(out));
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The same, for a process wearing the recycled pid that exits without
/// having touched nouveau: its exit is pid-keyed too (GEM objects,
/// channels), and it takes nothing of the zombie's, whose teardown is
/// its consumer's to release, not this exit's.
#[test]
fn a_recycled_pid_that_exits_without_a_channel_leaves_the_zombie_to_its_consumer() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
        Ok(0)
    );
    gpu.nouveau_release_process(A);
    assert!(has_chan(1), "a zombie");
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(A);
    assert!(
        !rm_calls_since(before).contains(&"ctx_free"),
        "the zombie is its consumer's to release, not this exit's: {:?}",
        rm_calls_since(before)
    );
    assert!(has_chan(1));
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1);
    assert_eq!(run_gpu(1).len(), 2);
    assert_eq!(run_gpu(0).len(), 2, "the compositor passes, no fault");
    gpu.reap_zombie_contexts();
    assert!(!has_chan(1), "freed once the consumer passed");
    assert!(gpu.nouveau_zombies.lock().is_empty());
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The compositor composites every client frame behind an ACQUIRE on the
/// client's fence, on the client's landing zone through a peer mapping.
/// A client whose ring hangs never writes it. syncobj gives up on the
/// fence after 10 s and releases its CPU-side waiters; the GPU-side one
/// is a host semaphore ACQUIRE with no timeout, so the compositor's ring
/// stopped for good behind the hung client: one hung GL client froze the
/// desktop. The timeout upcall lands the payload from the CPU.
#[test]
fn a_wedged_clients_fence_releases_the_compositors_acquire_too() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(
            &gpu,
            COMP,
            ch_c,
            &[push(PUSH_VA, 16)],
            &[sync(out)],
            &[sync(out2)]
        ),
        Ok(0)
    );
    let (fence_va, _, payload, ctx) =
        pending_hw_fence(out, 1).expect("the client's fence is pending on its ring");
    assert_eq!((ctx, payload), (1, 1));
    // The compositor's ring is behind the client's fence: it fetches
    // nothing while the zone is unwritten.
    assert!(run_gpu(0).is_empty(), "stalled on the ACQUIRE");
    assert_eq!(userd(&chan(0)).0, 0);
    // The client's ring never runs. syncobj gives up on the fence...
    test_clock::advance(syncobj::FENCE_TIMEOUT_US);
    assert_eq!(syncobj::poll_pending(), 0);
    assert_eq!(syncobj::query(out), Some(1), "the CPU side is released");
    assert_eq!(landing_zone(&chan(1)), 0);
    // ...and the upcall lands it on the GPU side too.
    gpu.fast_fence_timeout(ctx, fence_va, payload, out, 1);
    assert!(nv::ctx_is_wedged(1));
    assert_eq!(landing_zone(&chan(1)), 1, "written by the CPU");
    let fetched = run_gpu(0);
    assert_eq!(fetched.len(), 3, "acquire, push, release: {:?}", fetched);
    assert!(matches!(fetched[0], Fetched::Acquire { payload: 1, .. }));
    assert_eq!(syncobj::poll_pending(), 0);
    assert_eq!(
        syncobj::query(out2),
        Some(1),
        "the compositor's frame landed"
    );
    // The compositor keeps going.
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA + 0x100, 16)], &[], &[]),
        Ok(0)
    );
    assert_eq!(run_gpu(0).len(), 1);
    for h in [out, out2] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The same the other way round: a client waits on the compositor's
/// release fence (the buffer it may draw into again) through a mapping
/// of context 0's zone. Context 0 is never latched wedged, but its fence
/// timing out releases the client's CPU-side waiter all the same, so it
/// releases the ACQUIRE too.
#[test]
fn the_compositors_own_fence_timing_out_releases_a_clients_acquire_on_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    let (fence_va, _, payload, ctx) =
        pending_hw_fence(out2, 1).expect("the compositor's fence is pending on ring 0");
    assert_eq!((ctx, payload), (0, 1));
    assert!(run_gpu(1).is_empty(), "the client is behind the compositor");
    test_clock::advance(syncobj::FENCE_TIMEOUT_US);
    assert_eq!(syncobj::poll_pending(), 0);
    gpu.fast_fence_timeout(ctx, fence_va, payload, out2, 1);
    assert!(!nv::ctx_is_wedged(0), "context 0 is never latched");
    assert_eq!(landing_zone(&chan(0)), 1, "but its zone is landed");
    assert_eq!(run_gpu(1).len(), 2, "acquire, push");
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The other side of the zombie: the compositor is context 0, the
/// singleton its respawn rebuilds, so its page cannot be kept past its
/// exit -- and every client with a frame in flight has an ACQUIRE
/// queued on that page (its buffer's release). Its exit lands its last
/// payload and waits for those rings to fetch past it before the reset
/// frees the page, so no client channel faults.
#[test]
fn the_compositors_exit_lets_its_clients_pass_their_acquires_before_its_page_goes() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    assert!(
        peer_map(1, 0).is_some(),
        "the compositor's semaphore in the client's VAS"
    );
    assert!(run_gpu(1).is_empty(), "the client sits on the ACQUIRE");
    assert_eq!(userd(&chan(1)), (0, 2));
    // The compositor dies with its ring never run. While its exit
    // waits, the client's PBDMA keeps fetching (the hook is the GPU).
    let fetched: std::sync::Arc<std::sync::Mutex<Vec<Fetched>>> = Default::default();
    let sink = fetched.clone();
    *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
        sink.lock().unwrap().extend(run_gpu(1));
    }));
    let t0 = test_clock::now();
    gpu.nouveau_release_process(COMP);
    *GPU_SPIN_HOOK.lock() = None;
    let elapsed = test_clock::now().wrapping_sub(t0);
    assert_eq!(ctx0_owner(&gpu), 0);
    assert!(!has_chan(0));
    assert_eq!(peer_map(1, 0), None, "the mapping went with the page");
    let fetched = fetched.lock().unwrap();
    assert_eq!(
        fetched.len(),
        2,
        "the client fetched acquire and push while the exit waited: {:?}",
        *fetched
    );
    assert!(
        matches!(fetched[0], Fetched::Acquire { payload: 1, .. }),
        "{:?}",
        fetched[0]
    );
    assert!(has_chan(1), "the client's channel is intact");
    assert_eq!(userd(&chan(1)).0, 2);
    assert!(
        elapsed < CTX0_EXIT_GRACE_US,
        "the wait ended when the client passed, not on the budget ({} us)",
        elapsed
    );
    // Nothing is left on the client's ring to fault.
    assert!(run_gpu(1).is_empty());
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A client whose ring does not move in the grace period is stuck
/// elsewhere: the compositor's exit does not wait on it past the budget,
/// and the respawn is not held up.
#[test]
fn a_client_that_does_not_move_holds_the_compositors_exit_only_for_the_grace_period() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    let t0 = test_clock::now();
    gpu.nouveau_release_process(COMP);
    let elapsed = test_clock::now().wrapping_sub(t0);
    assert!(
        elapsed >= CTX0_EXIT_GRACE_US,
        "waited the grace period for the stuck client ({} us)",
        elapsed
    );
    assert!(
        elapsed < CTX0_EXIT_GRACE_US + 5_000,
        "and not much more ({} us)",
        elapsed
    );
    assert!(!has_chan(0), "the reset went through");
    assert_eq!(peer_map(1, 0), None);
    assert_eq!(userd(&chan(1)).0, 0, "the client never moved");
    // The client goes (its socket is gone) and the respawn is not held
    // up: a fresh channel behind index 0.
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(A);
    let ch_c2 = client_with_pushbuf(&gpu, COMP2);
    assert_eq!(ctx0_owner(&gpu), COMP2);
    assert_eq!(
        exec(&gpu, COMP2, ch_c2, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert_eq!(run_gpu(0).len(), 1);
    gpu.nouveau_release_process(COMP2);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// With two clients waiting, one that passes does not end the wait for
/// the one that has not: the page goes only when every waiter passed or
/// the grace period ran out.
#[test]
fn the_compositors_exit_waits_for_every_client_not_the_first_to_pass() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    assert!(run_gpu(1).is_empty() && run_gpu(2).is_empty());
    // A's PBDMA runs during the exit; B's is stuck.
    *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(|| {
        let _ = run_gpu(1);
    }));
    let t0 = test_clock::now();
    gpu.nouveau_release_process(COMP);
    *GPU_SPIN_HOOK.lock() = None;
    let elapsed = test_clock::now().wrapping_sub(t0);
    assert_eq!(userd(&chan(1)).0, 2, "A passed");
    assert_eq!(userd(&chan(2)).0, 0, "B did not");
    assert!(
        elapsed >= CTX0_EXIT_GRACE_US,
        "A passing did not end the wait for B ({} us)",
        elapsed
    );
    assert!(!has_chan(0));
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// labwc's respawn after a crash finds its clients still alive for a few
/// milliseconds (Firefox, Xwayland). Its CHANNEL_ALLOC must still claim
/// context 0: a client's RM-backed channel is on a context of its own
/// and holds nothing of the singleton.
#[test]
fn the_compositors_respawn_claims_ctx0_while_a_client_of_the_dead_one_is_still_alive() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let _ch_c = client_with_pushbuf(&gpu, COMP);
    assert_eq!(ctx0_owner(&gpu), COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    assert_eq!(
        ctx_of(&gpu, A),
        Some((1, true)),
        "the client is RM-backed, on its own context"
    );
    gpu.nouveau_release_process(COMP);
    assert_eq!(ctx0_owner(&gpu), 0, "the role is free");
    // The respawn, with A still alive.
    let ch_c2 = client_with_pushbuf(&gpu, COMP2);
    assert_eq!(
        ctx0_owner(&gpu),
        COMP2,
        "the respawn is the compositor, not a client"
    );
    assert_eq!(step17_builds(), 2, "on a fresh singleton channel");
    assert_eq!(ctx_of(&gpu, COMP2), None, "no client context for it");
    assert_eq!(
        exec(&gpu, COMP2, ch_c2, &[push(PUSH_VA, 16)], &[], &[]),
        Ok(0)
    );
    assert_eq!(run_gpu(0).len(), 1);
    // And the client is none the worse.
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    assert_eq!(run_gpu(1).len(), 1);
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP2);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The parent's `wait4` returns on PROCESS_TERMINATED, which the exit
/// signals BEFORE the DRM release hook runs: a supervisor respawns labwc
/// while the dead one's context-0 teardown (the grace wait, the window's
/// release, the RM's reset) is still running on another CPU. A respawn
/// whose CHANNEL_ALLOC lands inside that teardown must wait for it: the
/// singleton it would get otherwise is the dead compositor's channel,
/// which the teardown then frees under it.
#[test]
fn a_respawn_that_arrives_during_the_dead_compositors_teardown_waits_for_it_and_gets_a_fresh_channel(
) {
    use std::sync::atomic::AtomicBool as StdAtomicBool;
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    // `'static`: the respawn runs on its own thread, as it does on its own
    // CPU.
    let gpu: &'static NvidiaGpu = alloc::boxed::Box::leak(alloc::boxed::Box::new(gpu_rm_ladder()));
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(gpu, COMP);
    let ch_a = client_with_pushbuf(gpu, A);
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(
        exec(gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
        Ok(0)
    );
    assert!(run_gpu(1).is_empty(), "the client sits on the ACQUIRE");
    // The compositor dies. Its exit waits for the client to pass; while
    // it waits (the hook is the GPU's turn), the respawn arrives: its
    // CHANNEL_ALLOC starts on another thread, and the hook returns only
    // once that call is either waiting on the teardown or over. Then
    // the client passes and the teardown goes on.
    let respawn: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<u32>>>> =
        Default::default();
    let started = std::sync::Arc::new(StdAtomicBool::new(false));
    let slot = respawn.clone();
    let once = started.clone();
    *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
        if once.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = std::thread::spawn(move || client_with_pushbuf(gpu, COMP2));
        while CTX0_RESET_WAITERS.load(Ordering::Acquire) == 0 && !handle.is_finished() {
            std::thread::yield_now();
        }
        *slot.lock().unwrap() = Some(handle);
        let _ = run_gpu(1);
    }));
    gpu.nouveau_release_process(COMP);
    *GPU_SPIN_HOOK.lock() = None;
    assert!(
        started.load(Ordering::Acquire),
        "the exit waited, and the respawn came"
    );
    let handle = respawn
        .lock()
        .unwrap()
        .take()
        .expect("the respawn was started");
    let ch_c2 = handle.join().expect("the respawn's CHANNEL_ALLOC returned");
    assert_eq!(CTX0_RESET_WAITERS.load(Ordering::Acquire), 0);
    // It is the compositor, on a channel built AFTER the dead one's was
    // torn down -- not the dead one's, freed under it.
    assert_eq!(ctx0_owner(gpu), COMP2);
    assert_eq!(ctx0_resets(), 1);
    assert_eq!(
        step17_builds(),
        2,
        "a fresh singleton channel, not the dead compositor's"
    );
    assert_eq!(ctx_of(gpu, COMP2), None, "no client context");
    assert!(has_chan(1), "the client's channel is intact");
    let out3 = syncobj::create(false);
    assert_eq!(
        exec(gpu, COMP2, ch_c2, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
        Ok(0),
        "the respawn renders on its channel"
    );
    assert_eq!(run_gpu(0).len(), 2, "push and fence on the new ring");
    assert!(syncobj::destroy(out2));
    assert!(syncobj::destroy(out3));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(COMP2);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

// ----- A client index in teardown is nobody's until the RM has freed it -----

/// The exit takes a client's context out of the pid registry first (no
/// late submit routes to it) and frees it in the RM afterwards, tens of
/// milliseconds later. A client whose first touch landed in between
/// found the index free in the registry, so `ctx_alloc` ran on an index
/// the RM still had, and the dying client's `ctx_free` then took the
/// newcomer's context with it.
#[test]
fn an_index_whose_teardown_is_in_flight_is_not_handed_to_the_next_client() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu: &'static NvidiaGpu = alloc::boxed::Box::leak(alloc::boxed::Box::new(gpu_rm_fast()));
    let _ch_a = client_with_pushbuf(gpu, A);
    assert_eq!(ctx_of(gpu, A), Some((1, true)));
    // A goes away; the newcomer's first touch lands between the registry
    // and the RM (the hook runs right before `ctx_free`).
    let newcomer: std::sync::Arc<std::sync::Mutex<Option<(u32, Option<(u32, bool)>)>>> =
        Default::default();
    let sink = newcomer.clone();
    *CTX_TEARDOWN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
        if sink.lock().unwrap().is_some() {
            return;
        }
        let ch = client_with_pushbuf(gpu, B);
        *sink.lock().unwrap() = Some((ch, ctx_of(gpu, B)));
    }));
    gpu.nouveau_release_process(A);
    *CTX_TEARDOWN_HOOK.lock() = None;
    let (ch_b, ctx_b) = newcomer
        .lock()
        .unwrap()
        .take()
        .expect("the newcomer came during the teardown");
    assert_eq!(ctx_b, Some((2, true)), "not the index being torn down");
    assert_eq!(
        FAKE_RM.lock().ctx_frees,
        1,
        "the dead client's context went"
    );
    assert!(
        FAKE_RM.lock().ctxs.contains(&2),
        "the newcomer's context is still the RM's"
    );
    let out = syncobj::create(false);
    assert_eq!(
        exec(gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0),
        "the newcomer renders on its own context"
    );
    assert_eq!(run_gpu(2).len(), 2, "push and fence");
    // Once the teardown is over, the index is free again.
    let _ = client_with_pushbuf(gpu, STRANGER);
    assert_eq!(
        ctx_of(gpu, STRANGER),
        Some((1, true)),
        "the dead client's index, once the RM is done with it"
    );
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(STRANGER);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The same gap on the zombie's way out: the reap takes it off the
/// zombie list before its teardown runs, and a first touch in between
/// found the index free in the registry and the list both.
#[test]
fn a_zombies_index_is_not_handed_to_the_next_client_while_its_teardown_runs() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu: &'static NvidiaGpu = alloc::boxed::Box::leak(alloc::boxed::Box::new(gpu_rm_ladder()));
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(gpu, COMP);
    let ch_a = client_with_pushbuf(gpu, A);
    assert_eq!(ctx_of(gpu, A), Some((1, true)));
    let out = syncobj::create(false);
    assert_eq!(
        exec(gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
        Ok(0)
    );
    // A exits with the compositor's ACQUIRE queued on its fence: a
    // zombie. The compositor passes, so the zombie is due; the newcomer
    // arrives inside the reap's teardown of it.
    gpu.nouveau_release_process(A);
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1);
    assert_eq!(run_gpu(1).len(), 2, "the dead client's push and fence");
    assert_eq!(run_gpu(0).len(), 2, "the compositor passes its acquire");
    let newcomer: std::sync::Arc<std::sync::Mutex<Option<(u32, Option<(u32, bool)>)>>> =
        Default::default();
    let sink = newcomer.clone();
    *CTX_TEARDOWN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
        if sink.lock().unwrap().is_some() {
            return;
        }
        let ch = client_with_pushbuf(gpu, B);
        *sink.lock().unwrap() = Some((ch, ctx_of(gpu, B)));
    }));
    gpu.reap_zombie_contexts();
    *CTX_TEARDOWN_HOOK.lock() = None;
    assert!(
        gpu.nouveau_zombies.lock().is_empty(),
        "the zombie was reaped"
    );
    let (ch_b, ctx_b) = newcomer
        .lock()
        .unwrap()
        .take()
        .expect("the newcomer came during the zombie's teardown");
    assert_eq!(ctx_b, Some((2, true)), "not the zombie's index");
    assert_eq!(FAKE_RM.lock().ctx_frees, 1);
    assert!(FAKE_RM.lock().ctxs.contains(&2));
    let out2 = syncobj::create(false);
    assert_eq!(
        exec(gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
        Ok(0)
    );
    assert_eq!(run_gpu(2).len(), 2);
    let _ = client_with_pushbuf(gpu, STRANGER);
    assert_eq!(ctx_of(gpu, STRANGER), Some((1, true)), "free once freed");
    assert!(syncobj::destroy(out));
    assert!(syncobj::destroy(out2));
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(STRANGER);
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A consumer that never moves is wedged, and its own fence timeout is
/// what latches it: the zombie does not wait on it forever. Past the
/// fence timeout the teardown goes through, whatever the consumer's
/// ring says.
#[test]
fn a_zombie_whose_consumer_never_moves_is_freed_after_the_fence_timeout() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    FAKE_RM.lock().peer = true;
    test_clock::set_auto_advance(1_000);
    let ch_c = client_with_pushbuf(&gpu, COMP);
    let ch_a = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
        Ok(0)
    );
    assert_eq!(
        exec(&gpu, COMP, ch_c, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
        Ok(0)
    );
    gpu.nouveau_release_process(A);
    assert!(has_chan(1), "a zombie: the compositor's ACQUIRE is queued");
    assert_eq!(gpu.nouveau_zombies.lock().len(), 1);
    // Short of the timeout, another process's exit reaps nothing.
    test_clock::advance(syncobj::FENCE_TIMEOUT_US / 2);
    gpu.nouveau_release_process(STRANGER);
    assert!(has_chan(1));
    assert!(peer_map(0, 1).is_some());
    // Past it, the next event that runs the reaper frees it, with the
    // compositor's ring still where it was.
    test_clock::advance(syncobj::FENCE_TIMEOUT_US / 2);
    let before = FAKE_RM.lock().calls.len();
    gpu.nouveau_release_process(STRANGER);
    assert!(
        rm_calls_since(before).contains(&"ctx_free"),
        "freed on the timeout: {:?}",
        rm_calls_since(before)
    );
    assert!(!has_chan(1));
    assert_eq!(peer_map(0, 1), None);
    assert!(gpu.nouveau_zombies.lock().is_empty());
    assert_eq!(userd(&chan(0)).0, 0, "the compositor's ring never moved");
    gpu.nouveau_release_process(COMP);
    assert_eq!(FAKE_RM.lock().bad, 0);
}
/// `glxgears` under `vblank_mode=0`, as this kernel sees it: the fixed
/// per-frame ioctl sequence of a zink/NVK client on X11 (DRI3 with
/// explicit sync) against a labwc that composites at the refresh and
/// flips through the NVC57E ladder. No pixels and no GPU: the fake RM's
/// direct-submit channels, the PBDMA of `run_gpu`, the syncobj table
/// and the fake display front end, all on the test clock.
///
/// One thread, virtual time:
/// - The client draws into a swapchain of `IMAGES` buffers. A frame is
///   EXEC(wait = the image's acquire semaphore, sig = the swapchain
///   timeline at the frame's point); the GPU is a serial engine that
///   lands each frame `render_us` after the one before. The acquire
///   semaphore is built the way Mesa's `wsi_create_sync_for_image_syncobj`
///   builds it: the image's previous present AND the compositor's release
///   of it, merged and imported into the semaphore.
/// - Every present is a commit for the compositor. It serves a commit
///   once the frame's point has landed (its `SYNCOBJ_EVENTFD`) and, when
///   the commit replaces the buffer it held, lets the old one go: through
///   the fence of the pass still reading it, or at once (wlroots
///   `wlr_buffer_unlock`).
/// - At every vblank it renders the buffer it holds (EXEC on the
///   compositor's channel, waiting on that frame's point) and flips.
/// - The compositor is one thread: while it is inside an ioctl it serves
///   nothing, so an ioctl that waits is time the client's buffers stay
///   held. That is the coupling `vblank_mode=0` at 60 fps was made of.
mod glxgears_vblank_mode {
    use super::super::rm_host_shims::{reset_fake_hwflip, FAKE_HWFLIP};
    use super::super::surfaceflip_tests::SERIAL;
    use super::*;
    use crate::nvme::nvme_queue::test_clock;
    use crate::scheme::syncobj;
    use std::collections::VecDeque;

    /// zink on X11 acquires up to `minImageCount + 1` images: three.
    const IMAGES: usize = 3;
    /// The client's GPU work per frame: 500 frames a second unthrottled.
    const RENDER_US: u64 = 2_000;
    /// The compositor's pass over the scene.
    const COMPOSITE_US: u64 = 1_000;
    /// A 60 Hz panel.
    const VBLANK_US: u64 = 16_667;
    /// How long the display front end takes to fetch a flip: most of a
    /// frame, what the NVC57E flip used to wait out inside the ioctl.
    const FETCH_US: u64 = 15_000;
    const FRAMES: u64 = 240;
    const FB: u32 = 7;
    const H_MEMORY: u32 = 0x1234;
    /// A client waiting longer than this for a buffer is a kernel that
    /// lost the release, not a slow panel.
    const STUCK_US: u64 = 1_000_000;

    /// How the kernel's fence waits look for the point they are parked
    /// on.
    ///
    /// A client blocked in `SYNCOBJ_WAIT` does NOT see its buffer come
    /// free the instant the compositor releases it: there is no interrupt
    /// behind a syncobj on this hardware, so the wait is a poll, and what
    /// the client actually sees is its next probe. Every wait on this
    /// path used to probe on a flat 1 ms tick, which is more than a
    /// glxgears frame costs the GPU -- so the poll, not the card, was
    /// setting the frame rate. This is the knob that shows it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Poll {
        /// What the kernel does now: a few yields, then a sleep that
        /// starts at the timer's arming floor and backs off
        /// ([`syncobj::fence_poll_step`]).
        Backoff,
        /// The flat tick it replaced, in microseconds.
        FixedTick(u64),
    }

    /// How many times one probe of a parked wait walks the pending-fence
    /// list.
    ///
    /// `SYNCOBJ_WAIT` and the atomic commit's fence both have an async
    /// pre-wait in `drm_scheme.rs` that loops "look, sleep, look". Every
    /// look used to be TWO walks of the table: a `poll_pending()` and
    /// then the `wait_ready()` right behind it, which takes the same lock
    /// and runs the same `resolve_locked`. A walk is not bookkeeping --
    /// it reads the landing zone of every fence still in flight, and a
    /// landing zone is uncached pinned sysmem, so each read leaves the
    /// CPU. This is the knob that shows what the second walk cost.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Resolve {
        /// One walk per look: what the pre-waits do now.
        Once,
        /// The `poll_pending()` + `wait_ready()` pair they used to do.
        Duplicated,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Swap {
        /// `vblank_mode=0`: present as soon as the frame is drawn.
        Immediate,
        /// `vblank_mode=1`: the next frame starts once the compositor has
        /// taken this one to the panel.
        Vsync,
    }

    /// The compositor's pass over the scene, on the GPU.
    struct Pass {
        fence: u32,
        image: usize,
        lands_at: u64,
    }

    struct Desktop<'a> {
        gpu: &'a NvidiaGpu,
        ch_client: u32,
        ch_comp: u32,
        render_us: u64,
        composite_us: u64,
        swap: Swap,
        poll: Poll,
        resolve: Resolve,
        /// The swapchain timeline: frame `f` signals point `f + 1`.
        present_tl: u32,
        /// Per image, the compositor's release timeline: use `u` of the
        /// image is released at point `u`.
        release_tl: [u32; IMAGES],
        /// Per image, the acquire semaphore Mesa imports the merge into.
        sem: [u32; IMAGES],
        uses: [u64; IMAGES],
        last_point: [Option<u64>; IMAGES],
        /// Frames on the client's GPU: `(point, lands_at)`.
        client_queue: VecDeque<(u64, u64)>,
        client_landed: u64,
        client_gpu_free_at: u64,
        /// Commits the compositor has not served yet: `(image, point)`.
        inbox: VecDeque<(usize, u64)>,
        /// The buffer the compositor holds for its next pass.
        held: Option<(usize, u64)>,
        pass: Option<Pass>,
        comp_busy_until: u64,
        /// The highest point the compositor has taken to the panel.
        comp_consumed: u64,
        next_vblank: u64,
        commits_served: u64,
        frames_composited: u64,
        flips: u64,
        flip_cost_max_us: u64,
        client_stalls: u64,
        releases_through_a_fence: u64,
    }

    /// The desktop's GPU: the fake RM's direct submit, the panel on this
    /// GPU, `nvidia.surfaceflip` opted in and the compositor's output
    /// buffer registered as a VRAM framebuffer.
    fn desktop_gpu() -> NvidiaGpu {
        let gpu = gpu_rm_ladder();
        reset_fake_hwflip();
        FAKE_HWFLIP.lock().fetch_delay_us = FETCH_US;
        SURFACEFLIP_STATE.store(0, Ordering::Release);
        for c in [
            &SURFACEFLIP_FLIPS,
            &SURFACEFLIP_DRAIN_WAITS,
            &SURFACEFLIP_DRAIN_US,
            &SURFACEFLIP_DRAIN_MAX_US,
            &SURFACEFLIP_SUBMIT_MAX_US,
            &SURFACEFLIP_STUCK,
            &SURFACEFLIP_BUSY_REFUSED,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        test_clock::set(1_000_000);
        // Every read of the clock is a microsecond: an ioctl that polls
        // costs what it polls, and one that waits costs what it waits.
        test_clock::set_auto_advance(1);
        nv::set_surfaceflip_enabled(true);
        set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
        assert!(gpu.drives_boot_display());
        gpu.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
            id: FB,
            handle_id: 1,
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
            phys_addr: 0,
            size: 0,
            h_memory: H_MEMORY,
            vram_offset: Some(0),
        });
        gpu
    }

    impl<'a> Desktop<'a> {
        fn new(gpu: &'a NvidiaGpu, swap: Swap) -> Self {
            let ch_comp = client_with_pushbuf(gpu, COMP);
            let ch_client = client_with_pushbuf(gpu, A);
            let now = test_clock::now();
            Desktop {
                gpu,
                ch_client,
                ch_comp,
                render_us: RENDER_US,
                composite_us: COMPOSITE_US,
                swap,
                poll: Poll::Backoff,
                resolve: Resolve::Once,
                present_tl: syncobj::create(false),
                release_tl: core::array::from_fn(|_| syncobj::create(false)),
                sem: core::array::from_fn(|_| syncobj::create(false)),
                uses: [0; IMAGES],
                last_point: [None; IMAGES],
                client_queue: VecDeque::new(),
                client_landed: 0,
                client_gpu_free_at: now,
                inbox: VecDeque::new(),
                held: None,
                pass: None,
                comp_busy_until: now,
                comp_consumed: 0,
                next_vblank: now + VBLANK_US,
                commits_served: 0,
                frames_composited: 0,
                flips: 0,
                flip_cost_max_us: 0,
                client_stalls: 0,
                releases_through_a_fence: 0,
            }
        }

        fn now() -> u64 {
            test_clock::now()
        }

        /// The next moment anything happens: a landing, a vblank, or the
        /// compositor back from an ioctl with commits waiting.
        fn next_event(&self) -> u64 {
            let mut t = self.next_vblank;
            if let Some(&(_, at)) = self.client_queue.front() {
                t = t.min(at);
            }
            if let Some(p) = &self.pass {
                t = t.min(p.lands_at);
            }
            if !self.inbox.is_empty() && self.comp_busy_until > Self::now() {
                t = t.min(self.comp_busy_until);
            }
            t
        }

        /// Move the clock to `target`, handling every event on the way,
        /// in order. The clock never moves backwards.
        fn advance_to(&mut self, target: u64) {
            loop {
                let next = self.next_event();
                if next > target {
                    break;
                }
                if next > Self::now() {
                    test_clock::set(next);
                }
                self.events_due();
            }
            if target > Self::now() {
                test_clock::set(target);
            }
            self.serve_commits();
        }

        /// One turn of a parked fence wait: let time pass to this
        /// wait's next probe, handling every event on the way.
        ///
        /// A yield costs no wall clock, so the waiter sees the next
        /// event as it happens; a sleep means the waiter is blind until
        /// it wakes, however early the thing it waits for arrived.
        /// One look of a parked wait, the way the async pre-wait in
        /// `drm_scheme.rs` takes it: the `wait_ready` that answers, and
        /// under [`Resolve::Duplicated`] the redundant `poll_pending`
        /// that used to run in front of it.
        fn look(&self, handle: u32) -> bool {
            if self.resolve == Resolve::Duplicated {
                syncobj::poll_pending();
            }
            matches!(
                syncobj::wait_ready(&[handle], None, true, Self::now() + 1_000),
                Some(Ok(_))
            )
        }

        fn poll_wait(&mut self, probes: &mut u32) {
            let delay = match self.poll {
                Poll::Backoff => match syncobj::fence_poll_step(*probes) {
                    syncobj::PollStep::Yield => 0,
                    syncobj::PollStep::Sleep { us } => us,
                },
                Poll::FixedTick(us) => us,
            };
            *probes = probes.saturating_add(1);
            if delay == 0 {
                let next = self.next_event();
                self.advance_to(next);
            } else {
                let wake = Self::now() + delay;
                self.advance_to(wake);
            }
        }

        fn events_due(&mut self) {
            let now = Self::now();
            while let Some(&(point, at)) = self.client_queue.front() {
                if at > now {
                    break;
                }
                self.client_queue.pop_front();
                let fetched = run_gpu_frames(1, 1);
                assert!(
                    fetched.iter().any(|f| matches!(f, Fetched::Release { .. })),
                    "frame {}: no fence behind it on the client's ring ({:?})",
                    point,
                    fetched
                );
                syncobj::poll_pending();
                self.client_landed = point;
            }
            if self.pass.as_ref().is_some_and(|p| p.lands_at <= now) {
                let fetched = run_gpu_frames(0, 1);
                assert!(
                    fetched.iter().any(|f| matches!(f, Fetched::Release { .. })),
                    "the compositor's pass left no fence on its ring ({:?})",
                    fetched
                );
                syncobj::poll_pending();
                let p = self.pass.take().unwrap();
                assert!(syncobj::destroy(p.fence));
            }
            if self.next_vblank <= now {
                self.vblank();
            }
            self.serve_commits();
        }

        /// The output's frame event: render what is held, flip.
        fn vblank(&mut self) {
            self.next_vblank += VBLANK_US;
            let Some((image, point)) = self.held else {
                return;
            };
            if self.pass.is_some() {
                // The previous pass is still on the GPU: this frame is
                // skipped, as wlroots skips it.
                return;
            }
            let fence = syncobj::create(false);
            assert_eq!(
                exec(
                    self.gpu,
                    COMP,
                    self.ch_comp,
                    &[push(PUSH_VA, 16)],
                    &[sync_tl(self.present_tl, point)],
                    &[sync(fence)]
                ),
                Ok(0),
                "the compositor's pass over frame {}",
                point
            );
            self.pass = Some(Pass {
                fence,
                image,
                lands_at: Self::now() + self.composite_us,
            });
            let before = Self::now();
            assert!(self.gpu.page_flip(FB), "flip {} refused", self.flips);
            let cost = Self::now().wrapping_sub(before);
            self.flip_cost_max_us = self.flip_cost_max_us.max(cost);
            self.flips += 1;
            self.frames_composited += 1;
            self.comp_consumed = self.comp_consumed.max(point);
            self.comp_busy_until = Self::now();
        }

        /// The compositor's event loop: every commit whose frame has
        /// landed, in order.
        fn serve_commits(&mut self) {
            if Self::now() < self.comp_busy_until {
                return;
            }
            while let Some(&(image, point)) = self.inbox.front() {
                match syncobj::wait_ready(
                    &[self.present_tl],
                    Some(&[point]),
                    true,
                    Self::now() + 1_000,
                ) {
                    Some(Ok(_)) => {}
                    None | Some(Err(syncobj::WaitOutcome::Timeout)) => break,
                    Some(Err(_)) => panic!("frame {}: the swapchain timeline vanished", point),
                }
                assert!(
                    self.client_landed >= point,
                    "frame {}: the compositor's eventfd fired while the GPU was still \
                     drawing it (landed up to {})",
                    point,
                    self.client_landed
                );
                self.inbox.pop_front();
                if let Some((old, _)) = self.held.replace((image, point)) {
                    self.release(old);
                }
                self.commits_served += 1;
            }
        }

        /// wlroots lets go of the buffer a commit replaced: through the
        /// fence of the pass still reading it, or at once.
        fn release(&mut self, image: usize) {
            let point = self.uses[image];
            let reading = self
                .pass
                .as_ref()
                .filter(|p| p.image == image && p.lands_at > Self::now())
                .map(|p| p.fence);
            let ok = match reading {
                Some(fence) => {
                    self.releases_through_a_fence += 1;
                    syncobj::transfer(self.release_tl[image], point, fence, 1)
                }
                None => syncobj::timeline_signal(self.release_tl[image], point),
            };
            assert!(ok, "release of image {} at point {}", image, point);
        }

        /// One glxgears frame: acquire, draw, present.
        fn frame(&mut self, f: u64) {
            let image = (f as usize) % IMAGES;
            let point = f + 1;
            let mut waits = Vec::new();
            if let Some(prev) = self.last_point[image] {
                // Mesa: the image's previous present AND its release,
                // merged, imported into the acquire semaphore, and the
                // surrogate closed on the spot.
                let merged = syncobj::merge_fences(&[
                    (self.present_tl, prev),
                    (self.release_tl[image], self.uses[image]),
                ]);
                assert!(syncobj::import_snapshot(self.sem[image], merged, 1));
                assert!(syncobj::destroy(merged));
                let t0 = Self::now();
                let mut stalled = false;
                let mut probes = 0u32;
                loop {
                    if self.look(self.sem[image]) {
                        break;
                    }
                    assert!(
                        syncobj::query(self.sem[image]).is_some(),
                        "frame {}: the acquire semaphore of image {} vanished",
                        f,
                        image
                    );
                    stalled = true;
                    assert!(
                        Self::now().wrapping_sub(t0) < STUCK_US,
                        "frame {}: image {} (use {}) was never released to the client",
                        f,
                        image,
                        self.uses[image]
                    );
                    self.poll_wait(&mut probes);
                }
                if stalled {
                    self.client_stalls += 1;
                }
                waits.push(sync(self.sem[image]));
            }
            // Drawing into a buffer the compositor holds, or is still
            // reading, is a torn desktop.
            assert_ne!(
                self.held.map(|h| h.0),
                Some(image),
                "frame {}: draws into image {} while the compositor holds it",
                f,
                image
            );
            if let Some(p) = &self.pass {
                assert!(
                    !(p.image == image && p.lands_at > Self::now()),
                    "frame {}: draws into image {} while the compositor's pass is still reading it",
                    f,
                    image
                );
            }
            assert_eq!(
                exec(
                    self.gpu,
                    A,
                    self.ch_client,
                    &[push(PUSH_VA, 16)],
                    &waits,
                    &[sync_tl(self.present_tl, point)]
                ),
                Ok(0),
                "frame {}",
                f
            );
            let start = self.client_gpu_free_at.max(Self::now());
            let lands_at = start + self.render_us;
            self.client_gpu_free_at = lands_at;
            self.client_queue.push_back((point, lands_at));
            self.uses[image] += 1;
            self.last_point[image] = Some(point);
            self.inbox.push_back((image, point));
            self.serve_commits();
            if self.swap == Swap::Vsync {
                let t0 = Self::now();
                let mut probes = 0u32;
                while self.comp_consumed < point {
                    assert!(
                        Self::now().wrapping_sub(t0) < STUCK_US,
                        "frame {} never reached the panel",
                        f
                    );
                    self.poll_wait(&mut probes);
                }
            }
        }

        /// `frames` frames of glxgears; the frame rate they made.
        fn run(&mut self, frames: u64) -> f64 {
            let t0 = Self::now();
            for f in 0..frames {
                self.frame(f);
            }
            while let Some(&(_, at)) = self.client_queue.front() {
                self.advance_to(at);
            }
            let elapsed = Self::now().wrapping_sub(t0);
            frames as f64 * 1_000_000.0 / elapsed as f64
        }

        fn finish(mut self) {
            if let Some(at) = self.pass.as_ref().map(|p| p.lands_at) {
                self.advance_to(at);
            }
            assert!(self.pass.is_none());
            assert!(syncobj::destroy(self.present_tl));
            for h in self.release_tl.iter().chain(self.sem.iter()) {
                assert!(syncobj::destroy(*h));
            }
            self.gpu.nouveau_release_process(A);
            self.gpu.nouveau_release_process(COMP);
            test_clock::set_auto_advance(0);
            nv::set_surfaceflip_enabled(false);
            assert_eq!(FAKE_RM.lock().bad, 0);
        }
    }

    /// What the second walk of the pending-fence list cost a frame.
    ///
    /// A client parked on its acquire semaphore takes several looks per
    /// frame, and every look used to run `poll_pending()` and then
    /// `wait_ready()`: the same lock twice, the same `resolve_locked`
    /// twice, and -- the part that leaves the CPU -- the landing zone of
    /// every fence still in flight read twice. Removing the first of the
    /// two changes no answer the wait can give, because the second one
    /// resolves the table itself before it reads it. This pins that: the
    /// same desktop, the same frames, the same frame rate, and fewer
    /// trips to the fence (about three a frame here, and one lock
    /// acquisition a look, which this bench cannot price).
    #[test]
    fn a_parked_wait_walks_the_pending_fences_once_a_look_and_not_twice() {
        let _g = LOCK.lock();
        let _s = SERIAL.lock();
        let _live = LiveBytes::hold();

        let run = |resolve: Resolve| {
            let gpu = desktop_gpu();
            let mut d = Desktop::new(&gpu, Swap::Immediate);
            d.resolve = resolve;
            // The same Turing-sized frame the poll-tick measurement uses:
            // short enough that the client really does park on its
            // acquire semaphore every frame.
            d.render_us = 100;
            syncobj::FENCE_READS.store(0, Ordering::Relaxed);
            let settled = syncobj::table_len();
            let fps = d.run(FRAMES);
            // Mesa builds and closes a surrogate syncobj every frame. If
            // the closed ones piled up, every linear walk of the table
            // would get longer with every frame the desktop runs -- the
            // kind of slowdown that only shows after an hour of uptime,
            // which is exactly when nobody is looking. The desktop's own
            // handles (a present timeline, and a release timeline and an
            // acquire semaphore per image) are what `settled` counts.
            assert!(
                syncobj::table_len() <= settled + IMAGES,
                "{:?}: the table went from {} to {} objects over {} frames",
                resolve,
                settled,
                syncobj::table_len(),
                FRAMES
            );
            let reads = syncobj::FENCE_READS.load(Ordering::Relaxed);
            let stalls = d.client_stalls;
            assert_eq!(
                d.commits_served, FRAMES,
                "{:?}: a present was lost",
                resolve
            );
            d.finish();
            (fps, reads, stalls)
        };

        let (old_fps, old_reads, old_stalls) = run(Resolve::Duplicated);
        let (new_fps, new_reads, new_stalls) = run(Resolve::Once);

        // Without a stall there is no parked wait and this measures
        // nothing at all.
        assert!(old_stalls > 0 && new_stalls > 0, "the client never waited");

        // The answer does not change: same frames on the panel, same
        // rate. Only the work behind each look does.
        assert!(
            (old_fps - new_fps).abs() / old_fps < 0.02,
            "{:.0} -> {:.0} fps: dropping the second walk changed what the wait answers",
            old_fps,
            new_fps
        );
        // Measured: 6957 -> 6238 over 240 frames, about three reads a
        // frame. Not half, and the reason is worth keeping written down:
        // `poll_pending()` bails out on a lock-free counter when no
        // hardware fence is in flight, and most looks of this desktop
        // fall in that window. What it always paid, in flight or not, was
        // the second acquisition of the table lock -- and that one is not
        // counted here, because a uniprocessor bench cannot measure a
        // lock the other CPU wants. Two saved reads a frame is the floor
        // that a reinstated `poll_pending()` cannot clear -- and the floor
        // checked below is one, because the zone cache absorbed part of
        // what the second walk used to cost: it reads a zone once per
        // resolve whichever resolve asked, so the duplicate's second walk
        // now finds the word already in hand more often than not. Measured
        // over these 240 frames after that change: 19.0 -> 17.0.
        assert!(
            old_reads >= new_reads + FRAMES,
            "{} -> {} reads of a fence landing zone over {} frames \
             ({:.1} -> {:.1} a frame): the second walk is still there",
            old_reads,
            new_reads,
            FRAMES,
            old_reads as f64 / FRAMES as f64,
            new_reads as f64 / FRAMES as f64
        );

        // And the ceiling, which is what holds the other half of this
        // change: `resolve_locked` runs its passes to a fixed point, and
        // it used to re-read every landing zone on every pass although
        // what the later passes re-decide is whether a landed fence is
        // still HELD, not whether it landed. And what the cache holds is
        // the WORD, not a verdict, so two fences of one channel -- one
        // semaphore word, two payloads -- cost one read and not two.
        // Measured over these 240 frames: 26.0 reads a frame with no
        // cache at all, 24.0 with a verdict cached per (address,
        // payload), 17.0 with the word cached per address. The ceiling
        // sits between the last two, so a cache that goes back to keying
        // on the payload fails here.
        assert!(
            new_reads <= FRAMES * 18,
            "{:.1} reads of a fence landing zone a frame: a resolve is \
             reading the same zone more than once again",
            new_reads as f64 / FRAMES as f64
        );
    }

    /// The measurement this whole knob exists for: the same desktop,
    /// the same GPU, the same compositor, and the only difference is how
    /// soon a client parked in `SYNCOBJ_WAIT` looks at its fence again.
    ///
    /// With the flat 1 ms tick every one of the client's acquire stalls
    /// was rounded up to a millisecond, so a 2 ms frame became ~3 ms and
    /// the frame rate landed near 300 -- the number Moebius measures on
    /// his own card, from a kernel whose GPU work is far faster than 2 ms
    /// and whose frame rate was therefore set almost entirely by this
    /// tick. With the backoff the first probes cost no timer at all, so
    /// the client sees the release when it happens and the rate goes back
    /// to the GPU's own.
    #[test]
    fn the_fence_poll_tick_and_not_the_gpu_was_setting_the_frame_rate() {
        let _g = LOCK.lock();
        let _s = SERIAL.lock();
        let _live = LiveBytes::hold();

        let rate = |poll: Poll| {
            let gpu = desktop_gpu();
            let mut d = Desktop::new(&gpu, Swap::Immediate);
            d.poll = poll;
            // A glxgears frame on a Turing card, not the 2 ms the other
            // tests use: 100 us is well under the old poll tick, which is
            // the whole point -- a client slower than the tick hides it,
            // a client faster than it is paced by it.
            d.render_us = 100;
            let fps = d.run(FRAMES);
            let stalls = d.client_stalls;
            assert_eq!(d.commits_served, FRAMES, "{:?}: a present was lost", poll);
            d.finish();
            (fps, stalls)
        };

        let (old_fps, old_stalls) = rate(Poll::FixedTick(1_000));
        let (new_fps, new_stalls) = rate(Poll::Backoff);

        // The client really does park on its fence in both runs: without
        // stalls this measures nothing at all.
        // The client really does park on its fence in both runs: without
        // stalls this measures nothing at all.
        assert!(old_stalls > 0 && new_stalls > 0, "the client never waited");

        // What each run spends per frame, against the 100 us the GPU
        // needs. Everything above that is the client sitting on a fence
        // that had already landed, waiting to be allowed to look.
        let old_us = 1_000_000.0 / old_fps;
        let new_us = 1_000_000.0 / new_fps;
        assert!(
            old_us > 400.0,
            "{:.0} us a frame on a flat 1 ms tick: the tick is no longer what it was",
            old_us
        );
        assert!(
            new_us < 120.0,
            "{:.0} us a frame with the backoff, for 100 us of GPU work: \
             the client is still being paced by its poll",
            new_us
        );
        assert!(
            new_fps > old_fps * 3.0,
            "{:.0} -> {:.0} fps ({:.0} -> {:.0} us a frame), which is not worth the code",
            old_fps,
            new_fps,
            old_us,
            new_us
        );
    }

    /// The frame rate is the GPU's, not the panel's: a 2 ms frame gives
    /// ~500 fps against a 60 Hz compositor whose flips take the front end
    /// 15 ms to fetch. Nothing the client or the compositor asks of the
    /// kernel waits for a vblank.
    #[test]
    fn glxgears_with_vblank_mode_0_runs_at_the_speed_of_the_gpu_not_of_the_panel() {
        let _g = LOCK.lock();
        let _s = SERIAL.lock();
        let _live = LiveBytes::hold();
        let gpu = desktop_gpu();
        let mut d = Desktop::new(&gpu, Swap::Immediate);
        let fps = d.run(FRAMES);
        assert!(
            fps > 400.0,
            "{:.0} fps: the client is paced by something other than its own frames",
            fps
        );
        assert_eq!(
            d.commits_served, FRAMES,
            "every present reached the compositor"
        );
        // ~29 vblanks in 480 ms: one flip each, none of them waiting
        // for the panel, and no drain before the next one either (the
        // front end had a whole frame to fetch).
        assert!(
            (25..=32).contains(&d.flips),
            "{} flips in {} frames",
            d.flips,
            FRAMES
        );
        assert!(
            d.flip_cost_max_us < 200,
            "a flip cost {} us of the compositor's time",
            d.flip_cost_max_us
        );
        assert_eq!(SURFACEFLIP_FLIPS.load(Ordering::Relaxed), d.flips);
        assert_eq!(SURFACEFLIP_DRAIN_WAITS.load(Ordering::Relaxed), 0);
        assert_eq!(SURFACEFLIP_STUCK.load(Ordering::Relaxed), 0);
        assert_eq!(SURFACEFLIP_BUSY_REFUSED.load(Ordering::Relaxed), 0);
        d.finish();
    }

    /// The same client with `vblank_mode=1`: one frame per vblank,
    /// exactly the refresh, and every frame reaches the panel.
    #[test]
    fn glxgears_with_vblank_mode_1_runs_at_exactly_the_refresh() {
        let _g = LOCK.lock();
        let _s = SERIAL.lock();
        let _live = LiveBytes::hold();
        let gpu = desktop_gpu();
        let mut d = Desktop::new(&gpu, Swap::Vsync);
        let fps = d.run(FRAMES);
        assert!((fps - 60.0).abs() < 0.5, "{:.2} fps with vsync", fps);
        assert_eq!(d.commits_served, FRAMES);
        assert_eq!(d.flips, FRAMES, "one flip per frame");
        assert_eq!(d.client_stalls, 0, "with vsync the images are always free");
        d.finish();
    }

    /// A compositor pass that reads a buffer for most of a frame: the
    /// release rides the pass's fence, and the client, three images
    /// deep, waits for exactly that image and no other -- never drawing
    /// into one the GPU is still sampling.
    #[test]
    fn the_client_never_draws_into_a_buffer_the_compositor_is_still_reading() {
        let _g = LOCK.lock();
        let _s = SERIAL.lock();
        let _live = LiveBytes::hold();
        let gpu = desktop_gpu();
        let mut d = Desktop::new(&gpu, Swap::Immediate);
        d.composite_us = 12_000;
        let fps = d.run(FRAMES);
        assert!(
            d.releases_through_a_fence > 0,
            "no release ever waited for a pass"
        );
        assert!(
            d.client_stalls > 0,
            "the client never had to wait for a release"
        );
        assert!(fps > 200.0, "{:.0} fps", fps);
        assert_eq!(d.commits_served, FRAMES);
        d.finish();
    }

    /// A scene heavier than a frame (30 ms on the GPU): the compositor
    /// takes a frame only once it has landed, the panel repeats the last
    /// one meanwhile, and the rate is the GPU's 33 fps -- below the
    /// refresh, but not snapped to a divisor of it.
    #[test]
    fn a_frame_reaches_the_compositor_only_once_the_gpu_has_drawn_it() {
        let _g = LOCK.lock();
        let _s = SERIAL.lock();
        let _live = LiveBytes::hold();
        let gpu = desktop_gpu();
        let mut d = Desktop::new(&gpu, Swap::Immediate);
        d.render_us = 30_000;
        let fps = d.run(60);
        assert!((fps - 33.3).abs() < 1.0, "{:.2} fps", fps);
        assert_eq!(d.commits_served, 60);
        assert!(d.flips > 60, "the panel kept refreshing between frames");
        d.finish();
    }
}
