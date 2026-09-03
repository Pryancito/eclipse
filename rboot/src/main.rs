//! Simple ELF OS Loader on UEFI
//!
//! 1. Load config from "\EFI\Boot\rboot.conf"
//! 2. Load kernel ELF file
//! 3. Map ELF segments to virtual memory
//! 4. Map kernel stack and all physical memory
//! 5. Exit boot and jump to ELF entry

#![no_std]
#![no_main]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]

extern crate alloc;

#[macro_use]
extern crate log;

use config::Resolution;
use log::LevelFilter;
use rboot::GraphicInfo;
use uefi::boot::{
    self, AllocateType, MemoryType, OpenProtocolAttributes, OpenProtocolParams, SearchType,
};
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, ModeInfo, PixelFormat};
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::*;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::unsafe_protocol;
use uefi::table::cfg::ConfigTableEntry;
use uefi::{CStr16, Handle};
use xmas_elf::ElfFile;

mod arch;
mod config;
mod fb;
#[cfg(target_arch = "x86_64")]
mod idt;
mod libc_shim;
mod logo;
mod progress;

const CONFIG_PATH: &str = "\\EFI\\Boot\\rboot.conf";

/// Kernel locations tried after `kernel_path` (in this order) when the
/// configured one is missing, so a bare ESP with the image at a conventional
/// path still boots.
const KERNEL_FALLBACK_PATHS: &[&str] = &["\\os", "\\EFI\\Boot\\os", "\\EFI\\zCore\\zcore.elf"];

fn parse_log_level_from_cmdline(cmdline: &str) -> Option<LevelFilter> {
    // cmdline format example:
    //   "LOG=debug:ROOTPROC=/bin/busybox?sh:TERM=xterm-256color"
    // We keep this parser intentionally tiny (no alloc) and tolerant.
    for part in cmdline.split(':') {
        let mut it = part.splitn(2, '=');
        let k = it.next()?.trim();
        let v = it.next().unwrap_or("").trim();
        if k.eq_ignore_ascii_case("LOG") {
            return Some(v.parse().unwrap_or(LevelFilter::Info));
        }
    }
    None
}

fn has_cmdline_flag(cmdline: &str, key: &str) -> bool {
    for part in cmdline.split(':') {
        let mut it = part.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        if k.eq_ignore_ascii_case(key) {
            return v.is_empty()
                || v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on");
        }
    }
    false
}

/// Paint the boot progress bar if a framebuffer is available.
fn bar(graphic: Option<&GraphicInfo>, progress: u32) {
    if let Some(g) = graphic {
        progress::bar(g.mode, g.fb_addr, progress);
    }
}

#[entry]
fn efi_main() -> Status {
    uefi::helpers::init().expect("failed to initialize uefi helpers");
    // helpers::init() enables every log level. Stay quiet until rboot.conf
    // cmdline is parsed (`make qemu` defaults to LOG=error).
    log::set_max_level(LevelFilter::Warn);

    let config = {
        if let Some(mut file) = try_open_file(CONFIG_PATH) {
            let buf = load_file(&mut file);
            config::Config::parse(buf)
        } else {
            warn!("{} not found, using default config", CONFIG_PATH);
            config::Config::parse(b"")
        }
    };

    if let Some(level) = parse_log_level_from_cmdline(config.cmdline) {
        log::set_max_level(level);
    }
    info!("rboot: start (log={:?})", log::max_level());

    // Snapshot cmdline flags before ExitBootServices: `config` lives on the stack
    // and must not be read after firmware reclaims that memory.

    let acpi2_addr = find_config_table(ConfigTableEntry::ACPI2_GUID);
    debug!("acpi2 rsdp: {:?}", acpi2_addr);

    let (graphic_info, edid, edid_size) = init_graphic(config.resolution);
    let graphic = graphic_info.as_ref();
    // Boot progress is continuous across rboot (0..50) and kernel (50..100).
    if has_cmdline_flag(config.cmdline, "FB_ROT180") {
        fb::set_rot180(true);
    }
    if has_cmdline_flag(config.cmdline, "FB_MIRROR_X") {
        fb::set_mirror_x(true);
    }
    // Draw splash logo immediately after GOP init (this also clears screen to white).
    if let Some(g) = graphic {
        logo::draw_centered(g.mode, g.fb_addr);
    }
    bar(graphic, 0);
    debug!("rboot config: {:#x?}", config);
    bar(graphic, 5);

    let smbios_addr = find_config_table(ConfigTableEntry::SMBIOS_GUID);
    debug!("smbios: {:?}", smbios_addr);

    let elf = {
        let mut file = open_kernel_file(config.kernel_path);
        let buf = load_file_progress(&mut file, graphic.map(|g| (g.mode, g.fb_addr, 5, 15)));
        ElfFile::new(buf).expect("failed to parse ELF")
    };
    bar(graphic, 15);
    debug!(
        "kernel elf loaded: entry={:#x}",
        elf.header.pt2.entry_point()
    );

    // The initramfs is by far the largest file rboot touches (the whole
    // Eclipse rootfs, hundreds of MB): give it the 15..44 stretch of the bar
    // so a slow medium (VirtualBox's EFI FAT driver reads at ~1-2 MB/s) shows
    // steady movement instead of sitting "frozen at 15%" for minutes.
    let (initramfs_addr, initramfs_size) = match config.initramfs {
        Some(path) => match try_open_file(path) {
            Some(mut file) => {
                let buf =
                    load_file_progress(&mut file, graphic.map(|g| (g.mode, g.fb_addr, 15, 44)));
                debug!(
                    "initramfs loaded: addr={:#x} size={:#x}",
                    buf.as_ptr() as u64,
                    buf.len()
                );
                (buf.as_ptr() as u64, buf.len() as u64)
            }
            None => {
                warn!("initramfs {} not found; booting without it", path);
                (0, 0)
            }
        },
        None => (0, 0),
    };
    bar(graphic, 45);

    #[cfg(target_arch = "x86_64")]
    {
        use alloc::boxed::Box;
        use alloc::vec;
        use alloc::vec::Vec;
        use rboot::{BootInfo, MemoryDescriptor};
        use uefi::mem::memory_map::MemoryMap;
        use x86_64::registers::control::*;

        let entry = elf.header.pt2.entry_point() as usize;
        let graphic_info = graphic_info.expect("failed to find GraphicsOutput");
        let graphic = Some(&graphic_info);

        let max_phys_addr = {
            let mmap = boot::memory_map(MemoryType::LOADER_DATA).expect("failed to get memory map");
            mmap.entries()
                .map(|m| m.phys_start + m.page_count * 0x1000)
                .max()
                .unwrap()
                .max(0x1_0000_0000) // include IOAPIC MMIO area
                // Ensure the GOP framebuffer is always within the mapped range.
                // On most systems the framebuffer is listed in the UEFI memory map,
                // but on some firmware/GPU combinations the framebuffer BAR can sit
                // above the highest RAM entry.  Without this the kernel's
                // phys_to_virt(fb_addr) would translate to an unmapped virtual
                // address and triple-fault in early_fb_console::try_init().
                .max(graphic_info.fb_addr + graphic_info.fb_size)
                // Ensure initramfs is always within the mapped range too. Some firmware
                // allocates LOADER_DATA at high physical addresses that are above the
                // highest conventional RAM entry in the memory map we iterated.
                .max(initramfs_addr + initramfs_size)
        };
        bar(graphic, 46);

        let mut page_table = arch::current_page_table();
        unsafe {
            Cr0::update(|f| f.remove(Cr0Flags::WRITE_PROTECT));
            // On real hardware UEFI has already set EFER.NXE before we run, so
            // NO_EXECUTE bits we write into page-table entries are enforced.
            // Keep NXE enabled while running under firmware page tables. Some
            // UEFI mappings may already use NX bits and clearing NXE can fault
            // immediately on real hardware.
            Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE));
        }
        debug!("mapping elf segments...");
        arch::map_elf(&elf, &mut page_table, &mut arch::UEFIFrameAllocator)
            .expect("failed to map ELF");
        debug!("mapping kernel stack...");
        arch::map_stack(
            config.kernel_stack_address,
            config.kernel_stack_size,
            &mut page_table,
            &mut arch::UEFIFrameAllocator,
        )
        .expect("failed to map stack");
        debug!("mapping physical memory...");
        arch::map_physical_memory(
            config.physical_memory_offset,
            max_phys_addr,
            &mut page_table,
            &mut arch::UEFIFrameAllocator,
        );
        bar(graphic, 47);
        debug!("sanity checks before ExitBootServices...");

        // Sanity checks while Boot Services are still alive.
        // If these fault on real hardware, the firmware is much more likely to show a dump.
        let stacktop = config.kernel_stack_address + config.kernel_stack_size * 0x1000;
        unsafe {
            // 1) Confirm the entry virtual address is mapped & readable.
            let entry_va = entry as *const u8;
            let _first_byte = core::ptr::read_volatile(entry_va);
            // 2) Confirm the stack top page is mapped & writable.
            let sp_probe = (stacktop - 8) as *mut u64;
            core::ptr::write_volatile(sp_probe, 0);
        }
        bar(graphic, 48);
        unsafe {
            Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT));
        }

        // Pre-allocate BootInfo (and the memory-map vector's storage) on the
        // heap while boot services are still available: nothing may allocate
        // after ExitBootServices, so the map is copied into this fixed
        // capacity and never grown.
        const MEMORY_MAP_CAPACITY: usize = 512;
        let mut bootinfo_box = Box::new(BootInfo {
            memory_map: Vec::with_capacity(MEMORY_MAP_CAPACITY),
            physical_memory_offset: config.physical_memory_offset,
            graphic_info,
            acpi2_rsdp_addr: acpi2_addr as u64,
            smbios_addr: smbios_addr as u64,
            initramfs_addr,
            initramfs_size,
            cmdline: config.cmdline,
            edid,
            edid_size,
        });
        // Firmware buffer for the final memory map: twice the current size,
        // the map grows with the allocations ExitBootServices itself makes.
        let mmap_meta = boot::memory_map(MemoryType::LOADER_DATA)
            .expect("failed to get memory map size")
            .meta();
        let mmap_storage = Box::leak(vec![0u8; mmap_meta.map_size * 2].into_boxed_slice());

        // On some real machines, ExitBootServices can be the point where things go wrong.
        // Update the bar just before attempting it so we can pinpoint the hang visually.
        bar(graphic, 48);
        debug!("calling ExitBootServices (raw)...");
        // The console logger talks to firmware text output: silence it for
        // good before boot services go away (the progress bar is direct
        // framebuffer writes and keeps working).
        log::set_max_level(LevelFilter::Off);
        let (map_size, desc_size) = exit_boot_services_raw(mmap_storage);
        bar(graphic, 49);

        // Reinterpret the raw memory map buffer as `MemoryDescriptor` entries.
        // SAFETY: `mmap_storage` is leaked, and UEFI guarantees the memory map layout.
        let entry_size = desc_size;
        let len = map_size / entry_size;
        for i in 0..len {
            if bootinfo_box.memory_map.len() == bootinfo_box.memory_map.capacity() {
                break; // never reallocate after ExitBootServices
            }
            let p = unsafe { mmap_storage.as_ptr().add(i * entry_size) } as *const MemoryDescriptor;
            // Some firmware leaves unused tail bytes; stop on obviously empty descriptors.
            let d = unsafe { p.read_unaligned() };
            if d.page_count == 0 {
                break;
            }
            bootinfo_box.memory_map.push(d);
        }
        bar(graphic, 49);

        let bootinfo: &'static BootInfo = Box::leak(bootinfo_box);
        // Hand-off point to the kernel.
        bar(graphic, 50);

        unsafe {
            // If we see 51% but not the kernel marker (52%), the hang is inside the
            // handoff asm / very first instruction fetch. Always install a tiny IDT
            // so a #PF/#GP paints 99% instead of freezing the last rboot frame.
            bar(graphic, 51);
            idt::init(graphic_info.mode, graphic_info.fb_addr);
            arch::jump_to_entry(entry, bootinfo, stacktop);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        let _ = (
            initramfs_addr,
            initramfs_size,
            edid,
            edid_size,
            acpi2_addr,
            smbios_addr,
        );
        let entry = arch::load_elf(&elf, config.physical_memory_offset);
        let memory_map = boot::memory_map(MemoryType::LOADER_DATA)
            .expect("failed to get memory map for page-table setup");
        let pt0_paddr = arch::setup_page_tables(&memory_map);

        let bootinfo = rboot::Aarch64BootInfo {
            cmdline: config.cmdline,
            firmware_type: config.firmware_type,
            uart_base: config.uart_base,
            gic_base: config.gic_base,
            offset: config.physical_memory_offset as usize,
        };

        let bootinfo_box = alloc::boxed::Box::new(bootinfo);
        let bootinfo_ptr = alloc::boxed::Box::into_raw(bootinfo_box) as usize;

        info!("kernel entry point: 0x{:x}", entry);
        info!("exit boot services");
        bar(graphic, 50);
        let _ = unsafe { boot::exit_boot_services(None) };

        unsafe {
            arch::jump_to_kernel(entry, bootinfo_ptr, pt0_paddr);
        }
    }
}

/// Physical address of a UEFI configuration table, or null when the firmware
/// does not provide it (VMs without SMBIOS, for instance): the kernel treats
/// 0 as "absent" rather than rboot refusing to boot.
fn find_config_table(guid: uefi::Guid) -> *const core::ffi::c_void {
    uefi::system::with_config_table(|entries| {
        entries
            .iter()
            .find(|entry| entry.guid == guid)
            .map(|entry| entry.address)
    })
    .unwrap_or(core::ptr::null())
}

/// Call `GetMemoryMap` + `ExitBootServices` through the raw tables.
///
/// `uefi::boot::exit_boot_services` resets the machine on failure without
/// printing the underlying `Status`; this retries a couple of times and
/// panics with the status instead. Returns `(map_size, descriptor_size)` of
/// the final map written into `mmap_storage`.
#[cfg(target_arch = "x86_64")]
fn exit_boot_services_raw(mmap_storage: &mut [u8]) -> (usize, usize) {
    let st_raw = uefi::table::system_table_raw().expect("system table not available");
    // SAFETY: boot services are still active (this is the call that ends them).
    let bs_raw = unsafe { &*(*st_raw.as_ptr()).boot_services };
    let image = boot::image_handle();

    let mut last = Status::ABORTED;
    for _ in 0..2 {
        let mut map_size = mmap_storage.len();
        let mut map_key: usize = 0;
        let mut desc_size: usize = 0;
        let mut desc_ver: u32 = 0;

        let status = unsafe {
            (bs_raw.get_memory_map)(
                &mut map_size,
                mmap_storage.as_mut_ptr().cast(),
                &mut map_key,
                &mut desc_size,
                &mut desc_ver,
            )
        };
        if status != Status::SUCCESS {
            last = status;
            continue;
        }

        let status = unsafe { (bs_raw.exit_boot_services)(image.as_ptr(), map_key) };
        if status == Status::SUCCESS {
            return (map_size, desc_size);
        }
        last = status;
    }

    panic!("ExitBootServices failed: {:?}", last);
}

/// Open `SimpleFileSystem` without disconnecting the firmware FAT/ATAPI driver.
///
/// `open_protocol_exclusive` calls `DisconnectController` on whoever has the
/// protocol `ByDriver`. On QEMU q35 the first SFS handle is often the empty
/// DVD-ROM (`UEFI QEMU DVD-ROM QM00005`); stopping that ATAPI driver takes
/// down AHCI and the guest triple-faults right after "trying to open file".
fn open_sfs(handle: Handle) -> Option<boot::ScopedProtocol<SimpleFileSystem>> {
    // SAFETY: GetProtocol does not take exclusive ownership or disconnect
    // drivers. The handle is a live boot-services object and the returned
    // ScopedProtocol is dropped before ExitBootServices.
    unsafe {
        boot::open_protocol::<SimpleFileSystem>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
        .ok()
    }
}

/// SimpleFileSystem for the volume that loaded this bootloader, not an
/// arbitrary first handle (DVD-ROM, PXE, second disk, …).
fn boot_filesystem() -> Option<boot::ScopedProtocol<SimpleFileSystem>> {
    // SAFETY: same GetProtocol contract as `open_sfs`.
    let loaded_image = unsafe {
        boot::open_protocol::<LoadedImage>(
            OpenProtocolParams {
                handle: boot::image_handle(),
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
        .ok()?
    };
    let device_handle = loaded_image.device()?;
    if let Some(fs) = open_sfs(device_handle) {
        return Some(fs);
    }

    // SAFETY: DevicePath on the loaded-image device is firmware-owned for
    // the life of boot services; we only read it to locate the SFS handle.
    let device_path = unsafe {
        boot::open_protocol::<DevicePath>(
            OpenProtocolParams {
                handle: device_handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
        .ok()?
    };
    let sfs_handle = boot::locate_device_path::<SimpleFileSystem>(&mut &*device_path).ok()?;
    open_sfs(sfs_handle)
}

fn open_regular_file(fs: &mut SimpleFileSystem, path: &str) -> Option<RegularFile> {
    let mut buf = [0u16; 256];
    let ucs_path = CStr16::from_str_with_buf(path, &mut buf).ok()?;
    let mut root = fs.open_volume().ok()?;
    let handle = root
        .open(ucs_path, FileMode::Read, FileAttribute::empty())
        .ok()?;

    match handle.into_type().ok()? {
        FileType::Regular(regular) => Some(regular),
        _ => None,
    }
}

/// Try to open the regular file at `path` on the boot volume.
fn try_open_file(path: &str) -> Option<RegularFile> {
    debug!("trying to open file: {}", path);
    let mut fs = boot_filesystem()?;
    open_regular_file(&mut fs, path)
}

/// Open the kernel image: the configured path first, then the conventional
/// fallback locations.
fn open_kernel_file(kernel_path: &str) -> RegularFile {
    if let Some(file) = try_open_file(kernel_path) {
        return file;
    }
    for path in KERNEL_FALLBACK_PATHS {
        if let Some(file) = try_open_file(path) {
            warn!("kernel {} not found; using {}", kernel_path, path);
            return file;
        }
    }
    panic!("failed to open kernel ELF file: {}", kernel_path);
}

fn load_file(file: &mut RegularFile) -> &'static mut [u8] {
    load_file_progress(file, None)
}

/// Load a file in bounded chunks, optionally advancing the boot progress bar
/// across `(mode, fb_addr, from_pct, to_pct)` proportionally to bytes read.
///
/// The single giant `EFI_FILE_PROTOCOL.Read()` this replaces was fine on
/// OVMF/real firmware but is a trap on VirtualBox's EFI: its FAT driver
/// serves large reads glacially (minutes for a few hundred MB), and huge
/// single reads are where its known large-file bugs live -- the boot sat
/// "frozen" at the 15% mark for the whole initramfs load with no feedback.
/// Chunking keeps every firmware inside its comfort zone and lets the bar
/// move, so a slow medium looks slow instead of hung, and a genuine failure
/// panics with the failing offset instead of silently wedging.
fn load_file_progress(
    file: &mut RegularFile,
    progress: Option<(ModeInfo, u64, u32, u32)>,
) -> &'static mut [u8] {
    /// 8 MiB: large enough to stream at full speed on real firmware, small
    /// enough that even VirtualBox's EFI returns each call promptly.
    const CHUNK: usize = 8 * 1024 * 1024;
    let mut info_buf = [0u8; 0x100];
    let info = file
        .get_info::<FileInfo>(&mut info_buf)
        .expect("failed to get file info");
    let file_size = info.file_size() as usize;
    let pages = file_size / 0x1000 + 1;
    let mem_start = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
        .expect("failed to allocate pages");
    let buf = unsafe { core::slice::from_raw_parts_mut(mem_start.as_ptr(), pages * 0x1000) };
    let mut done = 0usize;
    while done < file_size {
        let end = (done + CHUNK).min(file_size);
        let n = match file.read(&mut buf[done..end]) {
            Ok(n) => n,
            Err(e) => panic!("failed to read file at offset {:#x}: {:?}", done, e),
        };
        if n == 0 {
            // Firmware reported EOF earlier than FileInfo promised: keep what
            // we got rather than spinning forever on zero-byte reads.
            break;
        }
        done += n;
        if let Some((mode, fb_addr, from, to)) = progress {
            let pct = from + ((to - from) as usize * done / file_size) as u32;
            progress::bar(mode, fb_addr, pct.min(to));
        }
    }
    &mut buf[..done]
}

/// Return the handle of the best GOP to use.
///
/// On systems with multiple GPUs (e.g. dual NVIDIA RTX) UEFI exposes one GOP
/// handle per GPU.  `get_handle_for_protocol` returns an arbitrary one which
/// may be the inactive/secondary card.  We enumerate all handles and prefer
/// the one with the largest accessible (non-BltOnly) framebuffer, which is
/// consistently the active/connected display on tested systems.
fn find_active_gop_handle() -> Option<Handle> {
    let handles = boot::locate_handle_buffer(SearchType::from_proto::<GraphicsOutput>()).ok()?;

    let mut best: Option<Handle> = None;
    let mut best_size: usize = 0;

    for &h in handles.iter() {
        if let Ok(mut gop) = boot::open_protocol_exclusive::<GraphicsOutput>(h) {
            // BltOnly means there is no direct framebuffer.
            if gop.current_mode_info().pixel_format() == PixelFormat::BltOnly {
                continue;
            }
            let sz = gop.frame_buffer().size();
            if sz > best_size {
                best_size = sz;
                best = Some(h);
            }
        }
    }

    // If every handle was BltOnly (no direct framebuffer), return None so the
    // caller can fall back to `get_handle_for_protocol`.
    best
}

/// Parse the display's EDID-preferred resolution from the first detailed
/// timing descriptor (EDID 1.x, bytes 54..71): a non-zero pixel clock marks a
/// timing descriptor, whose active pixels are 12-bit fields split across
/// low-byte + high-nibble. Returns `None` for a missing/invalid EDID or an
/// implausible timing.
fn edid_preferred_resolution(edid: &[u8; 128], edid_size: u32) -> Option<(usize, usize)> {
    if edid_size < 72 {
        return None;
    }
    let d = &edid[54..72];
    let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
    if pixel_clock == 0 {
        return None; // not a timing descriptor
    }
    let h = d[2] as usize | ((d[4] as usize & 0xF0) << 4);
    let v = d[5] as usize | ((d[7] as usize & 0xF0) << 4);
    if !(256..=7680).contains(&h) || !(144..=4320).contains(&v) {
        return None;
    }
    Some((h, v))
}

/// Auto (and oversized EDID) refuse GOP modes larger than this many pixels.
///
/// VirtualBox EFI GOP lists VRAM-filling "modes" (8K = 7680×4320) that are
/// not a real panel. The kernel shadows each VT at `width×height×4` bytes;
/// seven 8K consoles (~882 MiB) OOM a 512 MiB heap. 4K (3840×2160) is the
/// largest desktop panel we still fit. `resolution=WxH` is uncapped.
const AUTO_MAX_PIXELS: usize = 3840 * 2160;

fn mode_fits_auto_cap(w: usize, h: usize) -> bool {
    w > 0 && h > 0 && w.saturating_mul(h) <= AUTO_MAX_PIXELS
}

/// Pick and set the graphic mode per `resolution`; return the final mode and
/// the display's EDID. `None` when the firmware exposes no GOP at all (a
/// headless aarch64 board, say): x86_64 boot requires one, aarch64 does not.
fn init_graphic(resolution: Resolution) -> (Option<GraphicInfo>, [u8; 128], u32) {
    let gop_handle = match find_active_gop_handle()
        .or_else(|| boot::get_handle_for_protocol::<GraphicsOutput>().ok())
    {
        Some(h) => h,
        None => {
            warn!("no GraphicsOutput protocol: booting without a framebuffer");
            return (None, [0u8; 128], 0);
        }
    };
    let mut gop = match boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
        Ok(gop) => gop,
        Err(e) => {
            warn!("failed to open GraphicsOutput protocol: {:?}", e);
            return (None, [0u8; 128], 0);
        }
    };

    // EDID first: `Resolution::Auto` picks its target from it.
    let (edid, edid_size) = read_active_edid(gop_handle);

    // What resolution do we want, and how hard should we try?
    // - Exact(w,h): that mode or keep the current one (the old behaviour
    //   panicked with "graphic mode not found", bricking boot over a config
    //   value the firmware happens not to offer).
    // - Auto: the EDID-preferred timing if the firmware offers it *and* it
    //   fits [`AUTO_MAX_PIXELS`]; otherwise the largest offered mode within
    //   that cap (GOP modes are firmware-validated against the display, and
    //   a TV upscales its own standard timings far better than it stretches
    //   a small 4:3 mode across a 16:9 panel). Uncapped max-by-area is how
    //   VirtualBox landed on 8K and the kernel OOM'd.
    let target = match resolution {
        Resolution::Keep => None,
        Resolution::Exact(x, y) => Some((x, y)),
        Resolution::Auto => {
            edid_preferred_resolution(&edid, edid_size).filter(|&(w, h)| mode_fits_auto_cap(w, h))
        }
    };
    let exact = target.and_then(|want| gop.modes().find(|mode| mode.info().resolution() == want));
    let chosen = exact.or_else(|| {
        if resolution != Resolution::Auto {
            return None;
        }
        // Largest firmware-offered mode that still fits the cap.
        gop.modes()
            .filter(|mode| {
                let (w, h) = mode.info().resolution();
                mode_fits_auto_cap(w, h)
            })
            .max_by_key(|mode| {
                let (w, h) = mode.info().resolution();
                w.saturating_mul(h)
            })
            .or_else(|| {
                // Some VMs only list huge modes: pick the smallest so we
                // still boot rather than remaining at 8K.
                gop.modes().min_by_key(|mode| {
                    let (w, h) = mode.info().resolution();
                    w.saturating_mul(h)
                })
            })
    });
    if let Some(mode) = chosen {
        if let Err(e) = gop.set_mode(&mode) {
            warn!(
                "failed to set graphic mode {:?}: {:?}; keeping current {:?}",
                mode.info().resolution(),
                e,
                gop.current_mode_info().resolution()
            );
        }
    } else if target.is_some() {
        warn!(
            "requested graphic mode {:?} not offered by firmware; keeping current {:?}",
            target,
            gop.current_mode_info().resolution()
        );
    }
    let info = GraphicInfo {
        mode: gop.current_mode_info(),
        fb_addr: gop.frame_buffer().as_mut_ptr() as u64,
        fb_size: gop.frame_buffer().size() as u64,
    };
    (Some(info), edid, edid_size)
}

/// `EFI_EDID_ACTIVE_PROTOCOL` — the raw EDID of the currently-active display,
/// as read by the firmware at power-on to set the GOP mode (UEFI 2.x §12.9).
/// The console monitor hangs off the GPU that drives the GOP, so this is the
/// user's real panel EDID — obtained without any GPU display bring-up.
#[repr(C)]
#[unsafe_protocol("bd8c1056-9f36-44ec-92a8-a6337f817986")]
struct EdidActiveProtocol {
    size_of_edid: u32,
    edid: *const u8,
}

/// `EFI_EDID_DISCOVERED_PROTOCOL` — EDID as read over DDC, exposed by some
/// firmwares even when the active protocol is absent. Same layout.
#[repr(C)]
#[unsafe_protocol("1c0c34f6-d380-41fa-a049-8ad06c1a66aa")]
struct EdidDiscoveredProtocol {
    size_of_edid: u32,
    edid: *const u8,
}

fn edid_header_ok(b: &[u8]) -> bool {
    b.len() >= 8 && b[0] == 0x00 && b[7] == 0x00 && b[1..7].iter().all(|&x| x == 0xFF)
}

/// Read the active display's EDID (first 128-byte block). Tries the GOP
/// handle first (where the active-display EDID normally lives), then a global
/// lookup, then the discovered-EDID protocol. Returns `([0; 128], 0)` when no
/// EDID is available.
fn read_active_edid(gop_handle: Handle) -> ([u8; 128], u32) {
    // Copy one source's bytes into a fresh buffer; returns (buf, len).
    let read_one = |size: u32, ptr: *const u8| -> ([u8; 128], u32) {
        let mut buf = [0u8; 128];
        let n = (size as usize).min(128);
        if ptr.is_null() || n == 0 {
            return (buf, 0);
        }
        unsafe { core::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), n) };
        (buf, n as u32)
    };

    // All candidate sources, in order of preference. Some firmwares populate
    // the DISCOVERED protocol (raw DDC read) but leave ACTIVE empty, or expose
    // the protocol on a child handle rather than the GOP handle -- so gather
    // every candidate and pick the first with a valid EDID header.
    let mut first_nonempty: Option<([u8; 128], u32)> = None;
    let mut consider = |buf: [u8; 128], n: u32| -> Option<([u8; 128], u32)> {
        if n == 0 {
            return None;
        }
        if edid_header_ok(&buf[..n.min(128) as usize]) {
            return Some((buf, n));
        }
        if first_nonempty.is_none() {
            first_nonempty = Some((buf, n));
        }
        None
    };

    if let Ok(p) = boot::open_protocol_exclusive::<EdidActiveProtocol>(gop_handle) {
        let (b, n) = read_one(p.size_of_edid, p.edid);
        if let Some(v) = consider(b, n) {
            return v;
        }
    }
    if let Ok(h) = boot::get_handle_for_protocol::<EdidActiveProtocol>()
        && let Ok(p) = boot::open_protocol_exclusive::<EdidActiveProtocol>(h)
    {
        let (b, n) = read_one(p.size_of_edid, p.edid);
        if let Some(v) = consider(b, n) {
            return v;
        }
    }
    if let Ok(p) = boot::open_protocol_exclusive::<EdidDiscoveredProtocol>(gop_handle) {
        let (b, n) = read_one(p.size_of_edid, p.edid);
        if let Some(v) = consider(b, n) {
            return v;
        }
    }
    if let Ok(h) = boot::get_handle_for_protocol::<EdidDiscoveredProtocol>()
        && let Ok(p) = boot::open_protocol_exclusive::<EdidDiscoveredProtocol>(h)
    {
        let (b, n) = read_one(p.size_of_edid, p.edid);
        if let Some(v) = consider(b, n) {
            return v;
        }
    }
    // No source had a valid header; hand back the first non-empty capture (if
    // any) so /proc can dump it for diagnosis.
    first_nonempty.unwrap_or(([0u8; 128], 0))
}
