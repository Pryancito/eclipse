//! Run Zircon user program (userboot) and manage trap/interrupt/syscall.
//!
//! Reference: <https://fuchsia.googlesource.com/fuchsia/+/3c234f79f71/zircon/kernel/lib/userabi/userboot.cc>

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::convert::TryInto;
use core::{future::Future, pin::Pin};

use xmas_elf::ElfFile;

use kernel_hal::context::{TrapReason, UserContext, UserContextField};
use kernel_hal::{MMUFlags, PAGE_SIZE};
use zircon_object::debuglog::DebugLog;
use zircon_object::dev::{Resource, ResourceFlags, ResourceKind, SystemResource};
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::kcounter;
use zircon_object::object::{Handle, KernelObject, Rights};
use zircon_object::task::{CurrentThread, ExceptionType, Process, Thread, ThreadState};
use zircon_object::util::elf_loader::{ElfExt, VmarExt};
use zircon_object::vm::{pages, VmObject, VmarFlags};

macro_rules! include_bytes_aligned {
    ($path: expr) => {{
        #[repr(C, align(16))]
        struct Aligned<T>(T);

        static DATA: Aligned<[u8; include_bytes!($path).len()]> = Aligned(*include_bytes!($path));
        &DATA.0
    }};
}

macro_rules! boot_library {
    ($name: expr) => {{
        cfg_if::cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                boot_library!($name, "../../prebuilt/zircon/x64")
            } else if #[cfg(target_arch = "aarch64")] {
                boot_library!($name, "../../prebuilt/zircon/arm64")
            } else if #[cfg(target_arch = "riscv64")] {
                boot_library!($name, "../../prebuilt/zircon/riscv64")
            } else {
                compile_error!("Unsupported architecture for zircon mode!")
            }
        }
    }};
    ($name: expr, $base_dir: expr) => {{
        include_bytes_aligned!(concat!($base_dir, "/", $name, ".so"))
    }};
}

fn kcounter_vmos() -> (Arc<VmObject>, Arc<VmObject>) {
    let (desc_vmo, arena_vmo) = if cfg!(feature = "libos") {
        // dummy VMOs
        use zircon_object::util::kcounter::DescriptorVmoHeader;
        const HEADER_SIZE: usize = core::mem::size_of::<DescriptorVmoHeader>();
        let desc_vmo = VmObject::new_paged(1);
        let arena_vmo = VmObject::new_paged(1);

        let header = DescriptorVmoHeader::default();
        let header_buf: [u8; HEADER_SIZE] = unsafe { core::mem::transmute(header) };
        desc_vmo.write(0, &header_buf).unwrap();
        (desc_vmo, arena_vmo)
    } else {
        use kernel_hal::vm::{GenericPageTable, PageTable};
        use zircon_object::util::kcounter::AllCounters;
        let pgtable = PageTable::from_current();

        // kcounters names table.
        let desc_vmo_data = AllCounters::raw_desc_vmo_data();
        let paddr = pgtable.query(desc_vmo_data.as_ptr() as usize).unwrap().0;
        let desc_vmo = VmObject::new_physical(paddr, pages(desc_vmo_data.len()));

        // kcounters live data.
        let arena_vmo_data = AllCounters::raw_arena_vmo_data();
        let paddr = pgtable.query(arena_vmo_data.as_ptr() as usize).unwrap().0;
        let arena_vmo = VmObject::new_physical(paddr, pages(arena_vmo_data.len()));
        (desc_vmo, arena_vmo)
    };
    desc_vmo.set_name("counters/desc");
    arena_vmo.set_name("counters/arena");
    (desc_vmo, arena_vmo)
}

/// The stack size userboot asks for in its `PT_GNU_STACK` program header,
/// rounded up to a page. Falls back to Zircon's 256 KiB default.
fn elf_stack_size(elf: &ElfFile) -> usize {
    use xmas_elf::program::Type;
    const DEFAULT_STACK_SIZE: usize = 256 * 1024;
    const PT_GNU_STACK: u32 = 0x6474_e551;
    let size = elf
        .program_iter()
        .find(|ph| ph.get_type() == Ok(Type::OsSpecific(PT_GNU_STACK)))
        .map(|ph| ph.mem_size() as usize)
        .filter(|size| *size != 0)
        .unwrap_or(DEFAULT_STACK_SIZE);
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// ZBI container/item header magic and the item types this loader cares about.
/// See `zircon/system/public/zircon/boot/image.h`.
const ZBI_TYPE_CMDLINE: u32 = 0x4c44_4d43; // 'CMDL'
const ZBI_HEADER_SIZE: usize = 32;

/// Iterate the `CMDLINE` items of a ZBI container and call `f` on each.
fn for_each_zbi_cmdline(zbi: &[u8], mut f: impl FnMut(&str)) {
    if zbi.len() < ZBI_HEADER_SIZE {
        return;
    }
    let word = |off: usize| {
        let b = &zbi[off..off + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
    };
    // The container header's length covers the items that follow it.
    let end = match word(4).checked_add(ZBI_HEADER_SIZE) {
        Some(end) if end <= zbi.len() => end,
        _ => return,
    };
    let mut off = ZBI_HEADER_SIZE;
    while off + ZBI_HEADER_SIZE <= end {
        let item_type = word(off) as u32;
        let len = word(off + 4);
        let payload = off + ZBI_HEADER_SIZE;
        if payload + len > end {
            return;
        }
        if item_type == ZBI_TYPE_CMDLINE {
            if let Ok(s) = core::str::from_utf8(&zbi[payload..payload + len]) {
                f(s.trim_end_matches('\0'));
            }
        }
        off = payload + ((len + 7) & !7);
    }
}

/// Item header fields an appended item needs, and the two flag bits.
const ZBI_ITEM_MAGIC: u32 = 0xb578_1729;
const ZBI_FLAGS_VERSION: u32 = 0x0001_0000;
const ZBI_FLAGS_CRC32: u32 = 0x0002_0000;

/// CRC-32 (reflected, polynomial `0xEDB88320`), which is the one a ZBI item
/// carries. Spelled out here rather than pulled in as a dependency: it runs
/// once per boot over a few hundred bytes.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= 0xedb8_8320;
            }
        }
    }
    !crc
}

/// A copy of `zbi` with one more `CMDLINE` item, carrying `extra`.
///
/// This is how the kernel command line reaches the guest, and nothing else
/// does. Measured, with five different values of `core-tests=` and with
/// `userboot=`/`userboot.shutdown`: **not one option this kernel was given on
/// its command line had any effect**, so every run of
/// `scripts/zircon_core_test.py` ran the whole suite, listing nothing and
/// filtering nothing, and the job could not count. The VMO named
/// `boot-options.txt` that this loader also builds reaches the test process --
/// the process even reads its name while enumerating its handles -- and is
/// then never read; what the guest does read, at the exact offset of the
/// prebuilt's own `CMDLINE` item, is the ZBI. The prebuilt's item is what tells
/// it `kernel.select.userboot=userboot-test-rust` and
/// `userboot.test.next=test/core-tests-standalone`, which is why the test ran
/// at all and why it ran with its defaults.
///
/// Appending rather than rewriting in place: the prebuilt's item is followed
/// immediately by the bootfs, with nothing but its 8-byte padding to spare.
/// A ZBI may carry several `CMDLINE` items and their contents are taken
/// together, which is what [`for_each_zbi_cmdline`] already assumes when it
/// calls `f` once per item.
///
/// Returns `None` when `zbi` is not a container this function can extend, in
/// which case the caller hands the guest the ZBI untouched -- the behaviour
/// before this existed.
fn zbi_with_extra_cmdline(zbi: &[u8], extra: &str) -> Option<Vec<u8>> {
    if extra.is_empty() || zbi.len() < ZBI_HEADER_SIZE {
        return None;
    }
    let word = |off: usize| {
        let b: [u8; 4] = zbi[off..off + 4].try_into().unwrap();
        u32::from_le_bytes(b)
    };
    // The items end where the container header says they do, which need not be
    // the end of the file.
    let payload_len = word(4) as usize;
    let end = payload_len.checked_add(ZBI_HEADER_SIZE)?;
    if end > zbi.len() || end % 8 != 0 {
        return None;
    }
    // NUL-terminated, like the prebuilt's own item.
    let mut item_payload = Vec::with_capacity(extra.len() + 1);
    item_payload.extend_from_slice(extra.as_bytes());
    item_payload.push(0);
    fn put(header: &mut [u8; ZBI_HEADER_SIZE], off: usize, value: u32) {
        header[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }
    let mut header = [0u8; ZBI_HEADER_SIZE];
    put(&mut header, 0, ZBI_TYPE_CMDLINE);
    put(&mut header, 4, item_payload.len() as u32);
    put(&mut header, 12, ZBI_FLAGS_VERSION | ZBI_FLAGS_CRC32);
    put(&mut header, 24, ZBI_ITEM_MAGIC);
    // The checksum covers the header with its own `crc32` field zeroed,
    // followed by the unpadded payload. Verified against the prebuilt's item:
    // over those exact bytes its stored `0x030983f8` comes back out.
    let mut hasher_input = Vec::with_capacity(ZBI_HEADER_SIZE + item_payload.len());
    hasher_input.extend_from_slice(&header);
    hasher_input.extend_from_slice(&item_payload);
    put(&mut header, 28, crc32(&hasher_input));

    let mut out = Vec::with_capacity(end + ZBI_HEADER_SIZE + item_payload.len() + 8);
    out.extend_from_slice(&zbi[..end]);
    out.extend_from_slice(&header);
    out.extend_from_slice(&item_payload);
    while out.len() % 8 != 0 {
        out.push(0);
    }
    let grown = (out.len() - ZBI_HEADER_SIZE) as u32;
    out[4..8].copy_from_slice(&grown.to_le_bytes());
    Some(out)
}

/// Does this ZBI ask for the `userboot-test` build via `kernel.select.userboot=`?
fn zbi_selects_test_userboot(zbi: &[u8]) -> bool {
    let mut selected = false;
    for_each_zbi_cmdline(zbi, |cmdline| {
        for word in cmdline.split_ascii_whitespace() {
            if let Some(value) = word.strip_prefix("kernel.select.userboot=") {
                selected = value.starts_with("userboot-test");
            }
        }
    });
    selected
}

/// Run Zircon `userboot` process from the prebuilt path, and load the ZBI file as the bootfs.
pub fn run_userboot(zbi: impl AsRef<[u8]>, cmdline: &str) -> Arc<Process> {
    let zbi = zbi.as_ref();
    // `kernel.select.userboot=` in the ZBI's own command line picks the
    // userboot variant. `core-tests.zbi` asks for `userboot-test-rust`, which
    // is the build that understands `userboot.test.next=` and prints the
    // `*** Exit status N ***` line `scripts/zircon_core_test.py` greps for.
    let userboot: &'static [u8] = if zbi_selects_test_userboot(zbi) {
        boot_library!("userboot-test")
    } else {
        boot_library!("userboot")
    };
    // Only the vDSO has a libos build. scripts/gen-prebuilt.sh generates
    // `libzircon-libos.so` alone and says so outright -- "Userboot and ZBI
    // artifacts must remain the unmodified upstream builds" -- because the
    // libos patch reworks the syscall entry the vDSO makes, nothing else.
    #[cfg(feature = "libos")]
    let vdso = boot_library!("libzircon-libos");
    #[cfg(not(feature = "libos"))]
    let vdso = boot_library!("libzircon");

    let job = zircon_object::task::ROOT_JOB.clone();
    // The bootstrap protocol identifies handles by object type and name: the
    // job userboot adopts as its own is the JOB handle named "root".
    job.set_name("root");
    let proc = Process::create_with_vmar(
        &job,
        "userboot",
        zircon_object::vm::VmAddressRegion::new_root_zircon(),
        (),
    )
    .unwrap();
    let thread = Thread::create(&proc, "userboot").unwrap();
    let vmar = proc.vmar();

    // userboot
    //
    // The image goes in a child VMAR of its own. userboot's first bootstrap
    // message carries two VMAR handles and tells them apart by size: the
    // bigger one (the root VMAR) must wholly contain the smaller one (the
    // VMAR its own ELF image was loaded into).
    let (entry, userboot_vmar, userboot_size, stack_size) = {
        let elf = ElfFile::new(userboot).unwrap();
        let size = elf.load_segment_size();
        let child = vmar
            .allocate(None, size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
            .unwrap();
        child.load_from_elf(&elf).unwrap();
        (
            child.addr() + elf.header.pt2.entry_point() as usize,
            child,
            size,
            elf_stack_size(&elf),
        )
    };

    // vdso
    let (vdso_vmo, vdso_base, vdso_constants_offset) = {
        let elf = ElfFile::new(vdso).unwrap();
        let vdso_vmo = VmObject::new_paged(vdso.len() / PAGE_SIZE + 1);
        vdso_vmo.write(0, vdso).unwrap();
        let size = elf.load_segment_size();
        let vmar = vmar
            .allocate_at(
                userboot_size,
                size,
                VmarFlags::CAN_MAP_RXW | VmarFlags::SPECIFIC,
                PAGE_SIZE,
            )
            .unwrap();
        // userboot needs to be told where this landed: see `proc.start` below.
        let vdso_base = vmar.addr();
        // `DATA_CONSTANTS` is a local hidden symbol, so it is only in .symtab,
        // but every vDSO build in prebuilt/zircon carries it. Its vaddr is
        // also its file offset (this ELF maps p_offset == p_vaddr), which is
        // what the VMO below is indexed by.
        let vdso_constants_offset =
            elf.get_symbol_address("DATA_CONSTANTS")
                .expect("vDSO has no DATA_CONSTANTS symbol") as usize;
        vmar.map_from_elf(&elf, vdso_vmo.clone()).unwrap();
        #[cfg(feature = "libos")]
        {
            let offset = elf
                .get_symbol_address("zcore_syscall_entry")
                .expect("failed to locate syscall entry") as usize;
            let syscall_entry =
                &(kernel_hal::context::syscall_entry as *const () as usize).to_ne_bytes();
            // fill syscall entry x3
            vdso_vmo.write(offset, syscall_entry).unwrap();
            vdso_vmo.write(offset + 8, syscall_entry).unwrap();
            vdso_vmo.write(offset + 16, syscall_entry).unwrap();
        }
        (vdso_vmo, vdso_base, vdso_constants_offset)
    };

    // zbi
    //
    // Handed over with this kernel's own command line appended as one more
    // `CMDLINE` item: see `zbi_with_extra_cmdline` for why that is the only
    // route that reaches the guest. The kernel's separator is `:` and the
    // ZBI's is a space.
    let guest_cmdline = cmdline.replace(':', " ");
    let extended_zbi = zbi_with_extra_cmdline(zbi, guest_cmdline.trim());
    let zbi_bytes: &[u8] = extended_zbi.as_deref().unwrap_or(zbi);
    let zbi_vmo = {
        let vmo = VmObject::new_paged(zbi_bytes.len() / PAGE_SIZE + 1);
        vmo.write(0, zbi_bytes).unwrap();
        vmo.set_name("zbi");
        vmo
    };

    const VDSO_DATA_CONSTANTS_SIZE: usize = 0x78;
    let constants: [u8; VDSO_DATA_CONSTANTS_SIZE] =
        unsafe { core::mem::transmute(kernel_hal::vdso::vdso_constants()) };
    // Ask the vDSO where its constants live. This used to be hardcoded at
    // 0x4a50, which was right for an older Fuchsia build; in the current one
    // `.rodata` starts at 0x6000 and 0x4a50 lands inside `.dynstr`, so the
    // write silently shredded 120 bytes of the symbol-name table. The one
    // name that fell in the hole was `zx_port_create`, and userboot died
    // linking itself against the vDSO because the entry the hash table found
    // no longer compared equal to the name it was looking for.
    vdso_vmo.write(vdso_constants_offset, &constants).unwrap();
    // `DATA_CONSTANTS` is not the only page the kernel owes the vDSO:
    // `DATA_TIME_VALUES` holds the ticks-to-monotonic transformation that
    // `zx_clock_get_monotonic` applies **in userspace**, without a syscall.
    // The prebuilt blob ships it filled with `0xDEADBEEFDEADBEEF` -- Zircon's
    // "the kernel must write this" marker -- and nothing here ever wrote it,
    // so every program computed its own time from that marker: the clock read
    // `0xDEADBEEFDEADBEEF` plus the elapsed ticks, while the kernel's own
    // `timer_now()` answered correctly. Userspace and kernel were on two
    // different clocks, which is why a futex wait with a deadline could return
    // before its deadline had passed.
    //
    // Every field of the page the blob reads, found by disassembling
    // `prebuilt/zircon/x64/libzircon.so` and grepping the result for each
    // reference to the symbol, so that none is left on the marker:
    //
    // - `+0x08` (u64): the counter's frequency. `_zx_ticks_per_second` is a
    //   single `mov` out of this word and a `ret`, so leaving it unwritten
    //   hands every caller `0xDEADBEEFDEADBEEF` ticks per second.
    // - `+0x10` (u64): added to the raw counter on the **boot** timeline, by
    //   `_zx_ticks_get_boot` and `_zx_clock_get_boot`. Eclipse never
    //   suspends, so the boot and monotonic timelines are the same clock and
    //   this takes the same offset as `+0x18`.
    // - `+0x18` (u64): added to the raw `rdtscp` on the monotonic timeline.
    //   It is also read twice around the `rdtscp`, before and after, and the
    //   sequence retried if it changed, so it doubles as the seqlock guarding
    //   the rest.
    // - `+0x20` (u32): numerator of the ticks-to-nanoseconds scale.
    // - `+0x24` (u32): denominator. The vDSO tests this one and executes
    //   `ud2` when it is zero, which is how a page of zeros here kills
    //   `userboot` with an invalid opcode instead of a wrong time.
    //
    // `vdso.time_values=off` leaves the page as the blob shipped it, which is
    // the behaviour every build before this one had.
    if !kernel_hal::cmdline::is_off(cmdline, "vdso.time_values") {
        write_vdso_time_values(vdso, &vdso_vmo);
    }
    // Any VMO whose name starts with "vdso/" is a vDSO variant; the first one
    // is the one userboot maps into the programs it launches.
    vdso_vmo.set_name("vdso/stable");
    let vdso_test1 = vdso_vmo.create_child(false, 0, vdso_vmo.len()).unwrap();
    vdso_test1.set_name("vdso/test1");
    let vdso_test2 = vdso_vmo.create_child(false, 0, vdso_vmo.len()).unwrap();
    vdso_test2.set_name("vdso/test2");

    // TODO: use correct CrashLogVmo handle
    let crash_log_vmo = VmObject::new_paged(1);
    crash_log_vmo.set_name("crashlog");

    // The boot options, as the file Fuchsia's own standalone test runtime
    // reads them from.
    //
    // The kernel command line does not travel down the bootstrap channel --
    // `_zx_startup_get_arguments` returns nothing -- and userboot hands the
    // program it starts no environment at all, so neither route carries an
    // option to the core tests. `src/zircon/testing/standalone-test/
    // standalone-options.cc` takes a third one: it looks for a kernel file
    // VMO named `boot-options.txt` and parses that. Without it the
    // core-tests binary ran every case it contains whatever was asked for,
    // so `scripts/zircon_core_test.py` could neither list the tests nor
    // select a batch of them.
    //
    // The line format is the boot options' own, one `key=value` per line;
    // this kernel's command line is the same pairs separated by colons.
    let boot_options_text = cmdline.replace(':', "\n");
    let boot_options_vmo = VmObject::new_paged(pages(boot_options_text.len().max(1)));
    if !boot_options_text.is_empty() {
        boot_options_vmo
            .write(0, boot_options_text.as_bytes())
            .unwrap();
    }
    boot_options_vmo
        .set_content_size(boot_options_text.len())
        .unwrap();
    boot_options_vmo.set_name("boot-options.txt");

    // kcounter
    let (desc_vmo, arena_vmo) = kcounter_vmos();

    // Resources. Only the name matters to userboot: it looks each one up by
    // name ("system", "vmex", "mmio", "irq", "io_port"/"smc", "power"), takes
    // the one called "vmex" as its own VMEX capability (used to make bootfs
    // VMOs executable) and hands the rest on to the programs it starts.
    //
    // The root of the SYSTEM kind. Without it `zx_take_startup_handle`
    // returns nothing for `PA_SYSTEM_RESOURCE` in the program userboot
    // starts, and Fuchsia's standalone test runtime asserts in
    // `standalone-init.cc` ("standalone test didn't receive system
    // resource") the moment it sets itself up. That assertion never gets
    // printed, either: it fires from inside the initializer of the very
    // singleton that `__assert_fail`'s printf path needs, so
    // `__cxa_guard_acquire` blocks on the guard its own caller holds and the
    // process wedges in `futex.wait ... deadline=Forever` with nobody left to
    // wake it. That was `Zircon Core Test Baremetal (x86_64)` timing out
    // before it could list a single test.
    //
    // The range covers every subrange this kernel knows about (see
    // `SystemResource`), which is what makes it the parent capability the
    // named ones are carved out of.
    let system_resource = Resource::create(
        "system",
        ResourceKind::SYSTEM,
        0,
        SystemResource::Debuglog as usize + 1,
        ResourceFlags::empty(),
    );
    let vmex_resource = Resource::create(
        "vmex",
        ResourceKind::SYSTEM,
        SystemResource::Vmex as usize,
        1,
        ResourceFlags::empty(),
    );
    let mmio_resource = Resource::create(
        "mmio",
        ResourceKind::MMIO,
        0,
        0x1_0000_0000,
        ResourceFlags::empty(),
    );
    let irq_resource = Resource::create(
        "irq",
        ResourceKind::IRQ,
        0,
        0x1_0000_0000,
        ResourceFlags::empty(),
    );
    #[cfg(target_arch = "x86_64")]
    let arch_resource = Resource::create(
        "io_port",
        ResourceKind::IOPORT,
        0,
        0x1_0000_0000,
        ResourceFlags::empty(),
    );
    #[cfg(not(target_arch = "x86_64"))]
    let arch_resource = Resource::create(
        "smc",
        ResourceKind::SMC,
        0,
        0x1_0000_0000,
        ResourceFlags::empty(),
    );
    // No "power" resource: `zx_system_powerctl` is not implemented here, and
    // userboot only reaches for it when the kernel offers one.

    // stack
    //
    // The size comes from userboot's own `PT_GNU_STACK`: the Rust userboot
    // asks for 2 MiB, and the 32 KiB this used to hand out is nowhere near
    // enough for it.
    let stack_vmo = VmObject::new_paged(stack_size / PAGE_SIZE);
    stack_vmo.set_name("userboot-initial-stack");
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let stack_bottom = vmar
        .map(None, stack_vmo.clone(), 0, stack_vmo.len(), flags)
        .unwrap();
    let sp = if cfg!(target_arch = "x86_64") {
        // WARN: align stack to 16B, then emulate a 'call' (push rip)
        stack_bottom + stack_vmo.len() - 8
    } else {
        stack_bottom + stack_vmo.len()
    };

    // The kernel log userboot writes its own diagnostics to.
    let debuglog = DebugLog::create(0);

    // channel
    let (user_channel, kernel_channel) = Channel::create();
    let handle = Handle::new(user_channel, Rights::DEFAULT_CHANNEL);

    // Handles describing the userboot process itself. The first bootstrap
    // message carries exactly these, and no data bytes at all; anything else
    // makes `_zx_startup_get_handles` panic.
    let process_capabilities = || {
        alloc::vec![
            Handle::new(debuglog.clone(), Rights::DEFAULT_DEBUGLOG),
            Handle::new(proc.clone(), Rights::DEFAULT_PROCESS),
            Handle::new(vmar.clone(), Rights::DEFAULT_VMAR | Rights::IO),
            Handle::new(thread.clone(), Rights::DEFAULT_THREAD),
            Handle::new(userboot_vmar.clone(), Rights::DEFAULT_VMAR | Rights::IO),
        ]
    };

    // Message 1: the process capability message.
    kernel_channel
        .write(MessagePacket {
            data: Vec::new(),
            handles: process_capabilities(),
        })
        .unwrap();

    // Message 2: the system capability message. Also handles only. userboot
    // sorts these out by object type and name, so the order is immaterial --
    // except that the first VMO named "vdso/..." is the one it adopts.
    let mut handles = alloc::vec![
        Handle::new(zbi_vmo, Rights::DEFAULT_VMO),
        Handle::new(vdso_vmo, Rights::DEFAULT_VMO | Rights::EXECUTE),
        Handle::new(vdso_test1, Rights::DEFAULT_VMO | Rights::EXECUTE),
        Handle::new(vdso_test2, Rights::DEFAULT_VMO | Rights::EXECUTE),
        Handle::new(crash_log_vmo, Rights::DEFAULT_VMO),
        Handle::new(boot_options_vmo, Rights::DEFAULT_VMO),
        Handle::new(desc_vmo, Rights::DEFAULT_VMO),
        Handle::new(arena_vmo, Rights::DEFAULT_VMO),
    ];
    handles.extend(process_capabilities());
    handles.extend(alloc::vec![
        Handle::new(job, Rights::DEFAULT_JOB),
        Handle::new(system_resource, Rights::DEFAULT_RESOURCE),
        Handle::new(vmex_resource, Rights::DEFAULT_RESOURCE),
        Handle::new(mmio_resource, Rights::DEFAULT_RESOURCE),
        Handle::new(irq_resource, Rights::DEFAULT_RESOURCE),
        Handle::new(arch_resource, Rights::DEFAULT_RESOURCE),
    ]);
    kernel_channel
        .write(MessagePacket {
            data: Vec::new(),
            handles,
        })
        .unwrap();

    proc.start(&thread, entry, sp, Some(handle), vdso_base, thread_fn)
        .expect("failed to start main thread");
    proc
}

kcounter!(EXCEPTIONS_USER, "exceptions.user");
kcounter!(EXCEPTIONS_IRQ, "exceptions.irq");
kcounter!(EXCEPTIONS_PGFAULT, "exceptions.pgfault");

fn thread_fn(thread: CurrentThread) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
    Box::pin(run_user(thread))
}

async fn run_user(thread: CurrentThread) {
    kernel_hal::thread::set_current_thread(Some(thread.inner()));
    if thread.is_first_thread() {
        thread
            .handle_exception(ExceptionType::ProcessStarting)
            .await;
    };
    thread.handle_exception(ExceptionType::ThreadStarting).await;

    loop {
        // wait
        let mut ctx = thread.wait_for_run().await;
        if thread.state() == ThreadState::Dying {
            break;
        }

        // run
        trace!("go to user: {:#x?}", ctx);
        debug!("switch to {}|{}", thread.proc().name(), thread.name());
        let tmp_time = kernel_hal::timer::timer_now().as_nanos();

        // * Attention
        // The code will enter a magic zone from here.
        // `enter_uspace` will be executed into a wrapped library where context switching takes place.
        // The details are available in the `trapframe` crate on crates.io.
        ctx.enter_uspace();

        // Back from the userspace
        let time = kernel_hal::timer::timer_now().as_nanos() - tmp_time;
        thread.time_add(time);
        trace!("back from user: {:#x?}", ctx);
        EXCEPTIONS_USER.add(1);

        // handle trap/interrupt/syscall
        if let Err(e) = handler_user_trap(&thread, ctx).await {
            if let ExceptionType::ThreadExiting = e {
                break;
            }
            thread.handle_exception(e).await;
        }
    }
    thread.handle_exception(ExceptionType::ThreadExiting).await;
}

async fn handler_user_trap(
    thread: &CurrentThread,
    mut ctx: Box<UserContext>,
) -> Result<(), ExceptionType> {
    let reason = ctx.trap_reason();

    if let TrapReason::Syscall = reason {
        let num = syscall_num(&ctx);
        let args = syscall_args(&ctx);
        ctx.advance_pc(reason);
        thread.put_context(ctx);
        let mut syscall = zircon_syscall::Syscall { thread, thread_fn };
        let ret = syscall.syscall(num as u32, args).await as usize;
        thread
            .with_context(|ctx| ctx.set_field(UserContextField::ReturnValue, ret))
            .map_err(|_| ExceptionType::ThreadExiting)?;
        return Ok(());
    }

    thread.put_context(ctx);
    match reason {
        TrapReason::Interrupt(vector) => {
            EXCEPTIONS_IRQ.add(1); // FIXME
            kernel_hal::interrupt::handle_irq(vector);
            // Involuntary, like the Linux personality's trap path: the thread
            // asked for nothing, an interrupt landed on it. `yield_now` would
            // file it in the voluntary-yield lane, which only drains once no
            // urgent notify is left on the CPU's queue.
            kernel_hal::thread::preempt_now().await;
            Ok(())
        }
        TrapReason::PageFault(vaddr, flags) => {
            EXCEPTIONS_PGFAULT.add(1);
            info!("page fault from user mode @ {:#x}({:?})", vaddr, flags);
            let vmar = thread.proc().vmar();
            vmar.handle_page_fault(vaddr, flags).map_err(|err| {
                error!(
                    "failed to handle page fault from user mode @ {:#x}({:?}): {:?}\n{:#x?}",
                    vaddr,
                    flags,
                    err,
                    thread.context_cloned()
                );
                ExceptionType::FatalPageFault
            })
        }
        TrapReason::UndefinedInstruction => Err(ExceptionType::UndefinedInstruction),
        TrapReason::SoftwareBreakpoint => Err(ExceptionType::SoftwareBreakpoint),
        TrapReason::HardwareBreakpoint => Err(ExceptionType::HardwareBreakpoint),
        TrapReason::UnalignedAccess => Err(ExceptionType::UnalignedAccess),
        TrapReason::GernelFault(_) => Err(ExceptionType::General),
        _ => unreachable!(),
    }
}

fn syscall_num(ctx: &UserContext) -> usize {
    let regs = ctx.general();
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            regs.rax
        } else if #[cfg(target_arch = "aarch64")] {
            regs.x16
        } else if #[cfg(target_arch = "riscv64")] {
            regs.a7
        } else {
            unimplemented!()
        }
    }
}

fn syscall_args(ctx: &UserContext) -> [usize; 8] {
    let regs = ctx.general();
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            if cfg!(feature = "libos") {
                let arg7 = unsafe{ (regs.rsp as *const usize).read() };
                let arg8 = unsafe{ (regs.rsp as *const usize).add(1).read() };
                [regs.rdi, regs.rsi, regs.rdx, regs.rcx, regs.r8, regs.r9, arg7, arg8]
            } else {
                [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9, regs.r12, regs.r13]
            }
        } else if #[cfg(target_arch = "aarch64")] {
            [regs.x0, regs.x1, regs.x2, regs.x3, regs.x4, regs.x5, regs.x6, regs.x7]
        } else if #[cfg(target_arch = "riscv64")] {
            [regs.a0, regs.a1, regs.a2, regs.a3, regs.a4, regs.a5, regs.a6, regs.a7]
        } else {
            unimplemented!()
        }
    }
}

/// Fill the vDSO's `DATA_TIME_VALUES` page so userspace computes the same
/// monotonic clock the kernel does. See the call site for the layout and for
/// what shipping it unwritten did.
///
/// Writing nothing is always the safe outcome: the page then keeps the blob's
/// own contents, so a missing symbol or an unusable TSC ratio costs the old
/// behaviour rather than a wrong clock.
#[allow(unused_variables)]
fn write_vdso_time_values(vdso: &[u8], vdso_vmo: &Arc<VmObject>) {
    #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
    {
        /// Offsets within `DATA_TIME_VALUES`. See the call site for what each
        /// one is and which vDSO entry point reads it.
        const OFF_TICKS_PER_SECOND: usize = 0x08;
        const OFF_BOOT_OFFSET: usize = 0x10;
        const OFF_TICKS_OFFSET: usize = 0x18;
        const OFF_NUMERATOR: usize = 0x20;
        const OFF_DENOMINATOR: usize = 0x24;

        let Some((numerator, denominator, ticks_per_second)) = kernel_hal::timer::vdso_tsc_ratio()
        else {
            warn!("vdso: no usable TSC ratio; leaving DATA_TIME_VALUES as shipped");
            return;
        };
        // The kernel's own monotonic time is `(tsc - base) * num / den`; the
        // vDSO adds instead of subtracting, so what goes on the page is the
        // negation. Wrapping is the point, not an accident: the vDSO adds the
        // two as machine words and the sum is the small positive tick count.
        let offset = 0u64.wrapping_sub(kernel_hal::timer::vdso_tsc_base());
        let elf = ElfFile::new(vdso).unwrap();
        let Some(base) = elf.get_symbol_address("DATA_TIME_VALUES") else {
            warn!("vdso: no DATA_TIME_VALUES symbol; leaving the clock as shipped");
            return;
        };
        let base = base as usize;
        // The monotonic offset goes down last of all: it is the seqlock, so
        // publishing it is what tells the vDSO the rest is already in place.
        vdso_vmo
            .write(base + OFF_TICKS_PER_SECOND, &ticks_per_second.to_ne_bytes())
            .unwrap();
        vdso_vmo
            .write(base + OFF_DENOMINATOR, &denominator.to_ne_bytes())
            .unwrap();
        vdso_vmo
            .write(base + OFF_NUMERATOR, &numerator.to_ne_bytes())
            .unwrap();
        vdso_vmo
            .write(base + OFF_BOOT_OFFSET, &offset.to_ne_bytes())
            .unwrap();
        vdso_vmo
            .write(base + OFF_TICKS_OFFSET, &offset.to_ne_bytes())
            .unwrap();
        info!(
            "vdso: time values set (offset={:#x}, {}/{} ns per tick, {} ticks/s)",
            offset, numerator, denominator, ticks_per_second
        );
    }
}

#[cfg(test)]
mod zbi_cmdline_tests {
    use super::*;
    use alloc::string::String;

    /// The standard check value for CRC-32: `crc32("123456789")`.
    #[test]
    fn the_checksum_is_the_crc32_everyone_elses_is() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// A one-item container, built the way the prebuilt's own header is.
    fn container(items: &[(u32, &[u8])]) -> Vec<u8> {
        let mut out = alloc::vec![0u8; ZBI_HEADER_SIZE];
        let put = |buf: &mut [u8], off: usize, value: u32| {
            buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut out, 0, 0x544f_4f42); // 'BOOT'
        put(&mut out, 8, 0x868c_f7e6);
        put(&mut out, 12, ZBI_FLAGS_VERSION);
        put(&mut out, 24, ZBI_ITEM_MAGIC);
        put(&mut out, 28, 0x4a87_e8d6);
        for &(item_type, payload) in items {
            let mut header = alloc::vec![0u8; ZBI_HEADER_SIZE];
            put(&mut header, 0, item_type);
            put(&mut header, 4, payload.len() as u32);
            put(&mut header, 12, ZBI_FLAGS_VERSION);
            put(&mut header, 24, ZBI_ITEM_MAGIC);
            put(&mut header, 28, 0x4a87_e8d6);
            out.extend_from_slice(&header);
            out.extend_from_slice(payload);
            while out.len() % 8 != 0 {
                out.push(0);
            }
        }
        let len = (out.len() - ZBI_HEADER_SIZE) as u32;
        out[4..8].copy_from_slice(&len.to_le_bytes());
        out
    }

    fn cmdlines(zbi: &[u8]) -> Vec<String> {
        let mut seen = Vec::new();
        for_each_zbi_cmdline(zbi, |line| seen.push(String::from(line)));
        seen
    }

    /// This is the bug: what the kernel was given on its command line has to
    /// come back out of the ZBI the guest reads, next to what the prebuilt
    /// already carried -- not instead of it.
    #[test]
    fn the_appended_line_joins_the_one_already_there() {
        let base = container(&[(ZBI_TYPE_CMDLINE, b"userboot.test.next=test/x\0")]);
        assert_eq!(cmdlines(&base), ["userboot.test.next=test/x"]);
        let grown = zbi_with_extra_cmdline(&base, "core-tests=FutexTest.WaitTimeout").unwrap();
        assert_eq!(
            cmdlines(&grown),
            [
                "userboot.test.next=test/x",
                "core-tests=FutexTest.WaitTimeout"
            ],
            "the guest must see both items"
        );
    }

    /// The appended item has to survive a reader that checks the checksum, and
    /// the span it covers is the header with its own `crc32` field zeroed
    /// followed by the unpadded payload.
    #[test]
    fn the_appended_item_carries_a_checksum_that_checks_out() {
        let base = container(&[]);
        let grown = zbi_with_extra_cmdline(&base, "k=v").unwrap();
        let at = ZBI_HEADER_SIZE;
        let word = |off: usize| u32::from_le_bytes(grown[off..off + 4].try_into().unwrap());
        assert_eq!(word(at + 12) & ZBI_FLAGS_CRC32, ZBI_FLAGS_CRC32);
        let len = word(at + 4) as usize;
        assert_eq!(len, 4, "payload is `k=v` and a NUL");
        let mut span = grown[at..at + ZBI_HEADER_SIZE].to_vec();
        span[28..32].copy_from_slice(&[0; 4]);
        span.extend_from_slice(&grown[at + ZBI_HEADER_SIZE..at + ZBI_HEADER_SIZE + len]);
        assert_eq!(crc32(&span), word(at + 28));
    }

    /// The container's own length has to grow with it, and stay 8-aligned, or
    /// every reader stops before the new item -- which is indistinguishable
    /// from the bug this fixes.
    #[test]
    fn the_container_length_grows_to_cover_the_new_item() {
        let base = container(&[]);
        let grown = zbi_with_extra_cmdline(&base, "k=v").unwrap();
        let len = u32::from_le_bytes(grown[4..8].try_into().unwrap()) as usize;
        assert_eq!(len + ZBI_HEADER_SIZE, grown.len());
        assert_eq!(grown.len() % 8, 0);
    }

    /// Nothing to add, or nothing that can be extended: the caller hands the
    /// guest the ZBI untouched rather than a container it cannot parse.
    #[test]
    fn an_unusable_container_is_declined_instead_of_corrupted() {
        assert!(zbi_with_extra_cmdline(&container(&[]), "").is_none());
        assert!(zbi_with_extra_cmdline(&[], "k=v").is_none());
        assert!(zbi_with_extra_cmdline(&[0u8; 8], "k=v").is_none());
        // A container header whose length runs past the end of the buffer.
        let mut truncated = container(&[(ZBI_TYPE_CMDLINE, b"k=v\0")]);
        truncated[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(zbi_with_extra_cmdline(&truncated, "k=v").is_none());
    }
}
