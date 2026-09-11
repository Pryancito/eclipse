//! Run Zircon user program (userboot) and manage trap/interrupt/syscall.
//!
//! Reference: <https://fuchsia.googlesource.com/fuchsia/+/3c234f79f71/zircon/kernel/lib/userabi/userboot.cc>

use alloc::{boxed::Box, sync::Arc, vec::Vec};
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
use zircon_object::vm::{VmObject, VmarFlags};


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
        use zircon_object::{util::kcounter::AllCounters, vm::pages};
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
    let proc = Process::create(&job, "userboot").unwrap();
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
    let zbi_vmo = {
        let vmo = VmObject::new_paged(zbi.len() / PAGE_SIZE + 1);
        vmo.write(0, zbi).unwrap();
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

    // kcounter
    let (desc_vmo, arena_vmo) = kcounter_vmos();

    // Resources. Only the name matters to userboot: it takes the one called
    // "vmex" as its VMEX capability (used to make bootfs VMOs executable) and
    // hands the rest on to the programs it starts.
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
        Handle::new(desc_vmo, Rights::DEFAULT_VMO),
        Handle::new(arena_vmo, Rights::DEFAULT_VMO),
    ];
    handles.extend(process_capabilities());
    handles.extend(alloc::vec![
        Handle::new(job, Rights::DEFAULT_JOB),
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

    // The kernel command line no longer travels down the bootstrap channel:
    // `_zx_startup_get_arguments` returns nothing and userboot reads its
    // options out of the ZBI's own `CMDLINE` items instead.
    let _ = cmdline;

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
            kernel_hal::thread::yield_now().await;
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
